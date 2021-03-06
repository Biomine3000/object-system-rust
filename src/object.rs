use std::cmp::PartialEq;
use std::collections::BTreeMap;
use std::error;
use std::fmt;
use std::io;

use rustc_serialize::json::{ToJson, Json};


#[derive(Debug, Clone)]
pub struct BusinessObject {
    pub event: Option<String>,
    pub _type: Option<String>,
    pub size: Option<usize>,
    pub payload: Option<Payload>,
    pub metadata: BTreeMap<String,Json>
}


#[derive(Eq, PartialEq, Debug, Clone)]
pub enum Payload {
    Bytes(Vec<u8>)
}


#[derive(Debug)]
pub enum ReadBusinessObjectError {
    ReadError(io::Error),

    JsonSemanticsError(&'static str),
    JsonSyntaxError(String, String),
    BufferCharacterDecodingError
}


impl PartialEq for BusinessObject {
    fn eq(&self, other: &BusinessObject) -> bool {
        self.event == other.event &&
            self._type == other._type &&
            self.size == other.size &&
            self.payload == other.payload
    }
}


fn extract_reason(error: &ReadBusinessObjectError) -> &str {
    match *error {
        ReadBusinessObjectError::JsonSemanticsError(ref reason) => reason,
        ReadBusinessObjectError::JsonSyntaxError(_, ref reason) => reason,
        ReadBusinessObjectError::BufferCharacterDecodingError => "Character encoding error",
        ReadBusinessObjectError::ReadError(_) => "Read error"
    }
}


impl fmt::Display for ReadBusinessObjectError {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(f, "{:?}", extract_reason(self))
    }
}

impl error::Error for ReadBusinessObjectError {
    fn description(&self) -> &str {
        extract_reason(self)
    }
}


impl ToJson for BusinessObject {
    fn to_json(&self) -> Json {
        let mut d = BTreeMap::new();

        for (key, value) in self.metadata.iter() {
            d.insert(key.to_string(), value.clone());
        }

        if self._type.is_some() { d.insert("type".to_string(), (&self._type).clone().unwrap().to_json()); }
        if self.size.is_some() { d.insert("size".to_string(), (&self.size).clone().unwrap().to_json()); }
        if self.event.is_some() { d.insert("event".to_string(), (&self.event).clone().unwrap().to_json()); }

        Json::Object(d)
    }
}


impl BusinessObject {
    pub fn from_json(obj: &Json) -> Result<BusinessObject, ReadBusinessObjectError> {
        match obj.as_object() {
            Some(btree_obj) => Ok(btree_obj.to_business_object()),
            None => Err(ReadBusinessObjectError::JsonSemanticsError("Unsupported JSON type"))
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut result = self.to_json().to_string().into_bytes();
        result.push(b'\0');

        match self.payload {
            Some(Payload::Bytes(ref payload)) => {
                assert!(self.has_payload());
                assert!(self.size.unwrap() == payload.len());

                result.extend(payload);
            },
            None => {}
        }

        result
    }

    pub fn has_payload(&self) -> bool {
        match self.size {
            Some(size) => size > 0,
            None => false
        }
    }

    pub fn natures(&self) -> Vec<&str> {
        let mut result = Vec::new();

        match self.metadata.get("natures") {
            Some(natures) => {
                match natures.as_array() {
                    Some(natures) => {
                        for item in natures {
                            match item.as_string() {
                                Some(nature) => { result.push(nature); },
                                _ => { trace!("Cannot use {} as a nature", item); }
                            }
                        }
                    },
                    None => {}
                }

            }
            None => {}
        };

        result
    }
}


trait ToBusinessObject {
    fn to_business_object(&self) -> BusinessObject;
}


impl ToBusinessObject for BTreeMap<String,Json> {
    fn to_business_object(&self) -> BusinessObject {
        let mut result = BusinessObject {
            event: None,
            _type: None,
            size: None,
            payload: None,
            metadata: BTreeMap::new()
        };

        let event = self.get("event");
        if event.is_some() {
            let value = event.unwrap().as_string();
            if value.is_some() {
                result.event = Some(value.unwrap().to_string());
            }
        }

        let _type = self.get("type");
        if _type.is_some() {
            let value = _type.unwrap().as_string();
            if value.is_some() {
                result._type = Some(value.unwrap().to_string());
            }
        }

        let size = self.get("size");
        if size.is_some() {
            let value = size.unwrap().as_u64();
            if value.is_some() {
                let s = value.unwrap();
                if s > 0 {
                    result.size = Some(s as usize);
                }
            }
        }

        for (key, value) in self.iter() {
            if key == "event" || key == "type" || key == "size" {
                continue;
            }

            result.metadata.insert(key.to_string(), value.to_json());
        }

        result
    }
}


#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use rustc_serialize::json::{Json, ToJson};

    use super::BusinessObject;


    #[test]
    fn smoke_test_serialization_and_deserialization() {
        let mut metadata = BTreeMap::new();
        metadata.insert("subscriptions".to_string(),
                        vec!["@routing/*".to_string(), "@services/*".to_string(),
                             "@ping".to_string(), "@pong".to_string()].to_json());
        metadata.insert("subscriptions".to_string(), vec!["*".to_string()].to_json());

        let subscription = BusinessObject {
            _type: None,
            payload: None,
            size: None,
            event: Some("routing/subscribe".to_string()),
            metadata: metadata,
        };

        let json_repr_from = subscription.to_json();
        let string_repr = json_repr_from.to_string();
        let json_repr_to = Json::from_str(&string_repr).unwrap();
        let back = BusinessObject::from_json(&json_repr_to).unwrap();

        assert!(json_repr_from == json_repr_to);
        assert!(subscription == back);
    }
}
