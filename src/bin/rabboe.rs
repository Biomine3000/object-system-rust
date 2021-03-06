use std::collections::BTreeMap;
use std::fmt;
use std::io::{Write,Error, ErrorKind};
use std::io;
use std::net::SocketAddr;
use std::rc::Rc;
use std::str::FromStr;

#[macro_use]
extern crate log;
extern crate env_logger;

extern crate rustc_serialize;
use rustc_serialize::json::ToJson;

extern crate mio;
use mio::*;
use mio::buf::ByteBuf;
use mio::tcp::*;
use mio::util::Slab;

extern crate time;
use time::{Timespec, get_time};

extern crate object_system;
use object_system::BusinessObject;
use object_system::io::*;
use object_system::subscription;
use object_system::subscription::{BusinessSubscription, BusinessSubscriptionError, routing_decision};


fn parse_subscription(obj: &BusinessObject) -> Result<BusinessSubscription, BusinessSubscriptionError> {
    // trace!("Parsing subscription: {:?}", &obj.to_json());
    match obj.event {
        Some(ref event) => {
            if event == "routing/subscribe" {
                match obj.metadata.get("subscriptions") {
                    Some(subscriptions) => {
                        match subscription::parse_subscription(subscriptions) {
                            Ok(subs) => Ok(subs),
                            Err(e) => Err(e)
                        }
                    },
                    // TODO: default subscription
                    None => Err(BusinessSubscriptionError::NoSubscriptionMetadataKey)
                }
            } else {
                Err(BusinessSubscriptionError::UnknownSubscriptionEvent)
            }
        },
        None => Err(BusinessSubscriptionError::SubscriptionNotEvent)
    }
}


fn subscription_reply(subscriptions: &BusinessSubscription, request: &BusinessObject) -> Rc<BusinessObject> {
    let mut metadata = BTreeMap::new();
    metadata.insert("subscriptions".to_string(), subscriptions.to_json());

    match request.metadata.get("id") {
        Some(id) => {
            if id.is_string() {
                metadata.insert("in-reply-to".to_string(), id.as_string().unwrap().to_json());
            }
        },
        None => {}
    }

    Rc::new(BusinessObject {
        _type: None,
        payload: None,
        size: None,
        event: Some("routing/subscribe/reply".to_string()),
        metadata: metadata,
    })
}


fn ping_reply(request: &BusinessObject) -> Rc<BusinessObject> {
    let mut metadata = BTreeMap::new();

    match request.metadata.get("id") {
        Some(id) => {
            if id.is_string() {
                metadata.insert("in-reply-to".to_string(), id.as_string().unwrap().to_json());
            }
        },
        None => {}
    }

    Rc::new(BusinessObject {
        _type: None,
        payload: None,
        size: None,
        event: Some("pong".to_string()),
        metadata: metadata,
    })
}


struct Server {
    socket: TcpListener,
    token: Token,
    clients: Slab<BusinessClient>,
}


fn client_for_token<'a>(server: &'a mut Server, token: Token) -> &'a mut BusinessClient {
    &mut server.clients[token]
}


impl Server {
    fn new(socket: TcpListener) -> Server {
        Server {
            socket: socket,

            // As per
            // <https://github.com/hjr3/mob/blob/multi-echo-blog-post/src/main.rs>
            // something else but actually our registered events come in with
            // Token(0) by default.
            token: Token(1),

            clients: Slab::new_starting_at(Token(2), 128)
        }
    }

    fn register(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
        event_loop.register_opt(&self.socket, self.token, EventSet::readable(),
                                PollOpt::edge() | PollOpt::oneshot()
                                ).or_else(|e| {
                                    error!("Failed to register server {:?}, {:?}", self.token, e);
                                    Err(e)
                                })
    }

    fn reregister(&mut self, event_loop: &mut EventLoop<Server>) {
        event_loop.reregister(&self.socket, self.token, EventSet::readable(),
                              PollOpt::edge() | PollOpt::oneshot()
                              ).unwrap_or_else(|e| {
                                  error!("Failed to reregister server {:?}, {:?}", self.token, e);
                                  let server_token = self.token;
                                  self.reset_connection(event_loop, server_token);
                              })
    }

    fn new_client(&mut self, event_loop: &mut EventLoop<Server>) {
        // Log an error if there is no socket, but otherwise move on so we do not tear down the
        // entire server.
        let sock = match self.socket.accept() {
            Ok(s) => {
                match s {
                    Some(sock) => {
                        match sock.peer_addr() {
                            Ok(addr) => {
                                info!("Accepted connection from {:?}", addr);
                            },
                            Err(_) => {
                                self.reregister(event_loop);
                                return;
                            }
                        }
                        sock
                    },
                    None => {
                        error!("Failed to accept new socket");
                        self.reregister(event_loop);
                        return;
                    }
                }
            },
            Err(e) => {
                error!("Failed to accept new socket, {:?}", e);
                self.reregister(event_loop);
                return;
            }
        };

        match self.clients.insert_with(|token| {
            trace!("Registering {:?} with event loop", token);
            BusinessClient::new(sock, token)
        }) {
            Some(token) => {
                match client_for_token(self, token).register(event_loop) {
                    Ok(_) => {},
                    Err(e) => {
                        error!("Failed to register {:?} connection with event loop, {:?}", token, e);
                        self.clients.remove(token);
                    }
                }
            },
            None => {
                // If we fail to insert, `conn` will go out of scope and be dropped.
                error!("Failed to insert connection into slab");
            }
        };

        // Re-register server after received event
        self.reregister(event_loop);
    }

    fn readable(&mut self, event_loop: &mut EventLoop<Server>, token: Token) -> io::Result<()> {
        trace!("Server conn readable, token: {:?}", token);
        let objs_result = client_for_token(self, token).read_objects();

        match objs_result {
            Ok(objs) => {
                for obj in objs.into_iter() {
                    debug!("IN({:?}): {:?}", client_for_token(self, token).peer_addr, obj);
                    self.handle_incoming_object(event_loop, token, Rc::new(obj));
                }
            },
            Err(e) => {
                warn!("Couldn't read objects: {:?}", e);
            }
        };


        Ok(())
    }

    // fn periodical(&mut self, event_loop: &mut EventLoop<Server>) {
    //     for client in self.clients.iter_mut() {
    //         if client.last_activity - time::get_time() >= Duration::seconds(1) {
    //             // TODO: ping / schedule disconnect
    //         }
    //     }
    // }

    fn reset_connection(&mut self, event_loop: &mut EventLoop<Server>, token: Token) {
        if self.token == token {
            event_loop.shutdown();
        } else {
            trace!("Reset connection, token: {:?}", token);
            self.clients.remove(token);
        }
    }

    fn handle_incoming_object(&mut self, event_loop: &mut EventLoop<Server>,
                               token: Token, object: Rc<BusinessObject>) {
        match client_for_token(self, token).subscription {
            Some(_) => {
                trace!("Would handle {:?}", &object);
                client_for_token(self, token).last_activity = time::get_time();

                let is_ping = match object.event { Some(ref event) => event == "ping",
                                                   None => false };

                let mut bad_tokens = Vec::new();
                if is_ping {
                    let event: Option<&str> = Some("pong");

                    // TODO: this .clone() sucks, but it's needed for borrow checker. :(
                    let sub_opt: Option<BusinessSubscription> = client_for_token(self, token).subscription.clone();
                    let decision = routing_decision(None, event, None, &sub_opt.unwrap());

                    let pong = ping_reply(&object);
                    if decision {
                        client_for_token(self, token).send_object(pong)
                            .and_then(|_| client_for_token(self, token).reregister(event_loop))
                            .unwrap_or_else(|e| {
                                error!("Failed to queue message for {:?}: {:?}", token, e);
                                bad_tokens.push(token)
                            });
                    }
                } else {
                    // Queue up a write for all connected clients.
                    for client in self.clients.iter_mut() {
                        if client.subscription.is_none() {
                            trace!("Not subscribed; not routing {:?} to {:?}", object, client);
                            break;
                        }

                        let natures = object.natures();
                    
                        let event: Option<&str> = match object.event {
                            Some(ref t) => Some(t.as_ref()),
                            None => None
                        };

                        let payload_type: Option<&str> = match object._type {
                            Some(ref t) => Some(t.as_ref()),
                            None => None
                        };

                        // TODO: this .clone() sucks, but it's needed for borrow checker. :(
                        let sub_opt: Option<BusinessSubscription> = client.subscription.clone();
                        let decision = routing_decision(Some(natures), event, payload_type, &sub_opt.unwrap());

                        if decision {
                            client.send_object(object.clone())
                                .and_then(|_| client.reregister(event_loop))
                                .unwrap_or_else(|e| {
                                    error!("Failed to queue message for {:?}: {:?}", client.token, e);
                                    bad_tokens.push(client.token)
                                });
                        }
                    }
                }

                for t in bad_tokens {
                    self.reset_connection(event_loop, t);
                }
            },
            None => {
                trace!("Would subscribe {:?}", &object);
                match parse_subscription(&object) {
                    Ok(subscription) => {
                        let reply = subscription_reply(&subscription, &object);
                        let client = client_for_token(self, token);
                        let _ = client.send_object(reply);
                        client.subscription = Some(subscription);
                        client.last_activity = time::get_time();
                        // TODO: routing announcements
                    },
                    Err(e) => {
                        warn!("Couldn't parse subscription from client: {:?}", e);
                        self.reset_connection(event_loop, token);
                    }
                }
            }
        }
    }
}


impl Handler for Server {
    type Timeout = ();
    type Message = ();

    fn ready(&mut self, event_loop: &mut EventLoop<Server>, token: Token, events: EventSet) {
        trace!("Events = {:?}", events);
        assert!(token != Token(0), "[BUG]: Received event for Token(0)");

        if events.is_error() {
            warn!("Error event for {:?}", token);
            self.reset_connection(event_loop, token);
            return;
        }

        if events.is_hup() {
            trace!("Hup event for {:?}", token);
            self.reset_connection(event_loop, token);
            return;
        }

        // We never expect a write event for our `Server` token . A write event for any other token
        // should be handed off to that connection.
        if events.is_writable() {
            trace!("Write event for {:?}", token);
            assert!(self.token != token, "Received writable event for Server");

            client_for_token(self, token).writable()
                .and_then(|_| client_for_token(self, token).reregister(event_loop))
                .unwrap_or_else(|e| {
                    warn!("Write event failed for {:?}, {:?}", token, e);
                    self.reset_connection(event_loop, token);
                });
        }

        if events.is_readable() {
            trace!("Read event for {:?}", token);
            if self.token == token {
                self.new_client(event_loop);
            } else {
                self.readable(event_loop, token)
                    .and_then(|_| client_for_token(self, token).reregister(event_loop))
                    .unwrap_or_else(|e| {
                        warn!("Read event failed for {:?}: {:?}", token, e);
                        self.reset_connection(event_loop, token);
                    });
            }
        }
    }
}


struct BusinessClient {
    stream: BusinessObjectStream<TcpStream>,
    token: Token,
    interest: EventSet,
    send_queue: Vec<Rc<BusinessObject>>,

    subscription: Option<BusinessSubscription>,
    last_activity: Timespec,

    peer_addr: SocketAddr
}


impl fmt::Debug for BusinessClient {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let timestamp = match time::strftime("%Y-%m-%dT%H:%M:%S",
                                             &time::at_utc(self.last_activity)) {
            Ok(ts) => ts,
            Err(_) => "Couldn't format".to_string()
        };

        write!(f, "BusinessClient(token: {}, last_activity: {}, peer: {}, subscription: {:?})",
               self.token.as_usize(),
               timestamp,
               self.peer_addr,
               self.subscription)
    }
}


impl BusinessClient {
    fn new(socket: TcpStream, token: Token) -> BusinessClient {
        BusinessClient {
            peer_addr: socket.peer_addr().unwrap().clone(),

            stream: BusinessObjectStream::new(socket),
            token: token,

            interest: EventSet::hup(),

            send_queue: Vec::new(),

            subscription: Option::None,
            last_activity: time::get_time(),

        }
    }

    fn read_objects(&mut self) -> io::Result<Vec<BusinessObject>> {
        match self.stream.read_business_objects() {
            Ok(objs) => { Ok(objs) }
            Err(e) => { Err(Error::new(ErrorKind::Other, e)) }
        }
    }

    fn writable(&mut self) -> io::Result<()> {
        try!(self.send_queue.pop()
            .ok_or(Error::new(ErrorKind::Other, "Could not pop send queue"))
            .and_then(|object| {
                let bytes = &object.to_bytes();
                let mut buf = ByteBuf::from_slice(bytes);
                match self.stream.try_write_buf(&mut buf) {
                    Ok(None) => {
                        warn!("Tried to write {}, none written, putting object back to queue", bytes.len());
                        self.send_queue.push(object);
                        Ok(())
                    },
                    Ok(Some(n)) => {
                        if n != bytes.len() {
                            panic!("Wrote only {:?}, should have written {:?}", n, bytes.len());
                        }
                        debug!("Sent object to {:?}", self);
                        let _ = self.stream.flush();
                        trace!("CONN : we wrote {} bytes", n);
                        Ok(())
                    },
                    Err(e) => {
                        error!("Failed to send buffer for {:?}, error: {}", self.token, e);
                        Err(e)
                    }
                }
            })
        );

        if self.send_queue.is_empty() {
            self.interest.remove(EventSet::writable());
        }

        Ok(())
    }

    fn send_object(&mut self, object: Rc<BusinessObject>) -> io::Result<()> {
        debug!("OUT({:?}): {:?}", self.peer_addr, object);
        self.send_queue.push(object);
        self.interest.insert(EventSet::writable());
        Ok(())
    }

    fn register(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
        self.interest.insert(EventSet::readable());

        event_loop.register_opt(&self.stream.socket, self.token, self.interest, 
                                PollOpt::edge() | PollOpt::oneshot()
                                ).or_else(|e| {
                                    error!("Failed to register {:?}, {:?}", self.token, e);
                                    Err(e)
                                })
    }

    fn reregister(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
        event_loop.reregister(&self.stream.socket, self.token, self.interest,
                              PollOpt::edge() | PollOpt::oneshot()
                              ).or_else(|e| {
                                  error!("Failed to reregister {:?}, {:?}", self.token, e);
                                  Err(e)
                              })
    }
}


fn main() {
    env_logger::init().ok().expect("Failed to init logger");

    let addr: SocketAddr = FromStr::from_str("127.0.0.1:7890")
        .ok().expect("Failed to parse host:port string");
    let sock = TcpListener::bind(&addr).ok().expect("Failed to bind address");

    let mut event_loop = EventLoop::new().ok().expect("Failed to create event loop");

    let mut server = Server::new(sock);
    server.register(&mut event_loop).ok().expect("Failed to register server with event loop");

    info!("Server starting...");
    event_loop.run(&mut server).ok().expect("Failed to start event loop");
}
