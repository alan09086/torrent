use std::collections::BTreeMap;
use std::fmt;

/// A dynamically-typed bencode value.
///
/// Useful for inspecting bencode data without a schema, and for
/// `find_dict_key_span` operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BencodeValue {
    /// Integer: `i42e`
    Integer(i64),
    /// Byte string: `4:spam`
    Bytes(Vec<u8>),
    /// List: `l...e`
    List(Vec<BencodeValue>),
    /// Dictionary: `d...e` (keys sorted lexicographically)
    Dict(BTreeMap<Vec<u8>, BencodeValue>),
}

impl BencodeValue {
    /// Returns the integer value if this is a `BencodeValue::Integer`.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            BencodeValue::Integer(n) => Some(*n),
            _ => None,
        }
    }

    /// Returns the raw byte slice if this is a `BencodeValue::Bytes`.
    pub fn as_bytes_raw(&self) -> Option<&[u8]> {
        match self {
            BencodeValue::Bytes(b) => Some(b),
            _ => None,
        }
    }

    /// Returns the list if this is a `BencodeValue::List`.
    pub fn as_list(&self) -> Option<&[BencodeValue]> {
        match self {
            BencodeValue::List(items) => Some(items),
            _ => None,
        }
    }

    /// Returns the dictionary if this is a `BencodeValue::Dict`.
    pub fn as_dict(&self) -> Option<&BTreeMap<Vec<u8>, BencodeValue>> {
        match self {
            BencodeValue::Dict(map) => Some(map),
            _ => None,
        }
    }
}

impl fmt::Display for BencodeValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BencodeValue::Integer(n) => write!(f, "{n}"),
            BencodeValue::Bytes(b) => match std::str::from_utf8(b) {
                Ok(s) => write!(f, "\"{s}\""),
                Err(_) => write!(f, "<{} bytes>", b.len()),
            },
            BencodeValue::List(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, "]")
            }
            BencodeValue::Dict(map) => {
                write!(f, "{{")?;
                for (i, (key, val)) in map.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    match std::str::from_utf8(key) {
                        Ok(s) => write!(f, "\"{s}\": {val}")?,
                        Err(_) => write!(f, "<{} bytes>: {val}", key.len())?,
                    }
                }
                write!(f, "}}")
            }
        }
    }
}

impl serde::Serialize for BencodeValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            BencodeValue::Integer(n) => serializer.serialize_i64(*n),
            BencodeValue::Bytes(b) => serializer.serialize_bytes(b),
            BencodeValue::List(items) => {
                use serde::ser::SerializeSeq;
                let mut seq = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            BencodeValue::Dict(map) => {
                use serde::ser::SerializeMap;
                let mut m = serializer.serialize_map(Some(map.len()))?;
                for (key, val) in map {
                    m.serialize_entry(&serde_bytes::Bytes::new(key), val)?;
                }
                m.end()
            }
        }
    }
}

impl<'de> serde::Deserialize<'de> for BencodeValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        deserializer.deserialize_any(BencodeValueVisitor)
    }
}

struct BencodeValueVisitor;

impl<'de> serde::de::Visitor<'de> for BencodeValueVisitor {
    type Value = BencodeValue;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "a bencode value")
    }

    fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
        Ok(BencodeValue::Integer(v))
    }

    fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
        Ok(BencodeValue::Integer(v as i64))
    }

    fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> std::result::Result<Self::Value, E> {
        Ok(BencodeValue::Bytes(v.to_vec()))
    }

    fn visit_borrowed_bytes<E: serde::de::Error>(self, v: &'de [u8]) -> std::result::Result<Self::Value, E> {
        Ok(BencodeValue::Bytes(v.to_vec()))
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<Self::Value, E> {
        Ok(BencodeValue::Bytes(v.as_bytes().to_vec()))
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(BencodeValue::List(items))
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
        let mut dict = BTreeMap::new();
        while let Some((key, val)) = map.next_entry::<serde_bytes::ByteBuf, BencodeValue>()? {
            dict.insert(key.into_vec(), val);
        }
        Ok(BencodeValue::Dict(dict))
    }
}
