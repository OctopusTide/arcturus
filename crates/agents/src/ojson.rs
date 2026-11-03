//! A JSON value that keeps object keys in file order, for editing an agent's own config file
//! without reshuffling the user's settings. (`serde_json::Value` sorts keys unless the
//! workspace-wide `preserve_order` feature is on, which would change every other crate.)

use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};

#[derive(Debug, Clone, PartialEq)]
pub enum OValue {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<OValue>),
    Object(Vec<(String, OValue)>),
}

impl OValue {
    pub fn parse(text: &str) -> Result<OValue, String> {
        serde_json::from_str(text).map_err(|e| e.to_string())
    }

    /// Two-space indented, like the agents write their own files, with a final newline.
    pub fn to_pretty(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).unwrap_or_default();
        s.push('\n');
        s
    }

    pub fn str(s: &str) -> OValue {
        OValue::String(s.to_owned())
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            OValue::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<OValue>> {
        match self {
            OValue::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_array_mut(&mut self) -> Option<&mut Vec<OValue>> {
        match self {
            OValue::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&OValue> {
        match self {
            OValue::Object(o) => o.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut OValue> {
        match self {
            OValue::Object(o) => o.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Sets `key` on an object, keeping its position when it already exists.
    pub fn set(&mut self, key: &str, value: OValue) {
        if let OValue::Object(o) = self {
            match o.iter_mut().find(|(k, _)| k == key) {
                Some((_, v)) => *v = value,
                None => o.push((key.to_owned(), value)),
            }
        }
    }

    pub fn remove(&mut self, key: &str) -> Option<OValue> {
        match self {
            OValue::Object(o) => o.iter().position(|(k, _)| k == key).map(|i| o.remove(i).1),
            _ => None,
        }
    }

    pub fn is_object(&self) -> bool {
        matches!(self, OValue::Object(_))
    }

    /// Converts to an ordinary value (key order is lost); for reading and comparing.
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

impl Serialize for OValue {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            OValue::Null => s.serialize_unit(),
            OValue::Bool(b) => s.serialize_bool(*b),
            OValue::Number(n) => n.serialize(s),
            OValue::String(v) => s.serialize_str(v),
            OValue::Array(a) => {
                let mut seq = s.serialize_seq(Some(a.len()))?;
                for v in a {
                    seq.serialize_element(v)?;
                }
                seq.end()
            }
            OValue::Object(o) => {
                let mut map = s.serialize_map(Some(o.len()))?;
                for (k, v) in o {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for OValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<OValue, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OValue;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_unit<E>(self) -> Result<OValue, E> {
                Ok(OValue::Null)
            }
            fn visit_none<E>(self) -> Result<OValue, E> {
                Ok(OValue::Null)
            }
            fn visit_bool<E>(self, v: bool) -> Result<OValue, E> {
                Ok(OValue::Bool(v))
            }
            fn visit_i64<E>(self, v: i64) -> Result<OValue, E> {
                Ok(OValue::Number(v.into()))
            }
            fn visit_u64<E>(self, v: u64) -> Result<OValue, E> {
                Ok(OValue::Number(v.into()))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<OValue, E> {
                serde_json::Number::from_f64(v).map(OValue::Number).ok_or_else(|| E::custom("not a finite number"))
            }
            fn visit_str<E>(self, v: &str) -> Result<OValue, E> {
                Ok(OValue::String(v.to_owned()))
            }
            fn visit_string<E>(self, v: String) -> Result<OValue, E> {
                Ok(OValue::String(v))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<OValue, A::Error> {
                let mut out = Vec::new();
                while let Some(v) = seq.next_element()? {
                    out.push(v);
                }
                Ok(OValue::Array(out))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<OValue, A::Error> {
                let mut out: Vec<(String, OValue)> = Vec::new();
                while let Some((k, v)) = map.next_entry::<String, OValue>()? {
                    // Last one wins, like JSON.parse.
                    match out.iter_mut().find(|(key, _)| *key == k) {
                        Some((_, old)) => *old = v,
                        None => out.push((k, v)),
                    }
                }
                Ok(OValue::Object(out))
            }
        }
        d.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_order_survives_a_round_trip() {
        let text = "{\n  \"zeta\": 1,\n  \"alpha\": {\n    \"y\": [\n      true,\n      null,\n      2.5\n    ],\n    \"b\": \"x\"\n  }\n}\n";
        let v = OValue::parse(text).unwrap();
        assert_eq!(v.to_pretty(), text);
        let mut v = v;
        v.set("middle", OValue::str("new"));
        v.get_mut("alpha").unwrap().set("y", OValue::Null);
        assert_eq!(
            v.to_pretty(),
            "{\n  \"zeta\": 1,\n  \"alpha\": {\n    \"y\": null,\n    \"b\": \"x\"\n  },\n  \"middle\": \"new\"\n}\n"
        );
        assert_eq!(v.remove("zeta"), Some(OValue::Number(1.into())));
    }
}
