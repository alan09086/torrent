use serde::de::{self, Visitor};

use crate::error::{Error, Result};

/// Bencode deserializer.
///
/// Parses bencode from a byte slice, supporting zero-copy deserialization
/// of byte strings via `deserialize_bytes`.
pub struct Deserializer<'de> {
    input: &'de [u8],
    pos: usize,
}

impl<'de> Deserializer<'de> {
    /// Create a new deserializer from a byte slice.
    pub fn new(input: &'de [u8]) -> Self {
        Deserializer { input, pos: 0 }
    }

    /// Verify all input has been consumed. Call after deserializing.
    pub fn finish(&self) -> Result<()> {
        if self.pos < self.input.len() {
            Err(Error::TrailingData {
                position: self.pos,
                count: self.input.len() - self.pos,
            })
        } else {
            Ok(())
        }
    }

    fn peek(&self) -> Result<u8> {
        self.input
            .get(self.pos)
            .copied()
            .ok_or(Error::UnexpectedEof {
                position: self.pos,
                context: "expected more data".into(),
            })
    }

    fn next(&mut self) -> Result<u8> {
        let byte = self.peek()?;
        self.pos += 1;
        Ok(byte)
    }

    fn expect(&mut self, expected: u8) -> Result<()> {
        let byte = self.next()?;
        if byte != expected {
            Err(Error::UnexpectedByte {
                byte,
                position: self.pos - 1,
                expected: match expected {
                    b'e' => "'e' (end marker)",
                    b'i' => "'i' (integer start)",
                    b'l' => "'l' (list start)",
                    b'd' => "'d' (dict start)",
                    b':' => "':' (string separator)",
                    _ => "specific byte",
                },
            })
        } else {
            Ok(())
        }
    }

    fn parse_integer_value(&mut self) -> Result<i64> {
        let start = self.pos;

        // Find the 'e' terminator
        let end = self.input[self.pos..]
            .iter()
            .position(|&b| b == b'e')
            .ok_or(Error::UnexpectedEof {
                position: self.pos,
                context: "unterminated integer".into(),
            })?;

        let num_bytes = &self.input[self.pos..self.pos + end];
        self.pos += end + 1; // skip past 'e'

        if num_bytes.is_empty() {
            return Err(Error::InvalidInteger {
                position: start,
                detail: "empty integer".into(),
            });
        }

        // Reject leading zeros (except "0" itself)
        if num_bytes.len() > 1 && num_bytes[0] == b'0' {
            return Err(Error::InvalidInteger {
                position: start,
                detail: "leading zero".into(),
            });
        }

        // Reject negative zero
        if num_bytes == b"-0" {
            return Err(Error::InvalidInteger {
                position: start,
                detail: "negative zero".into(),
            });
        }

        // Reject bare minus sign
        if num_bytes == b"-" {
            return Err(Error::InvalidInteger {
                position: start,
                detail: "bare minus sign".into(),
            });
        }

        // Reject leading zeros in negative numbers
        if num_bytes.len() > 2 && num_bytes[0] == b'-' && num_bytes[1] == b'0' {
            return Err(Error::InvalidInteger {
                position: start,
                detail: "leading zero in negative".into(),
            });
        }

        let s = std::str::from_utf8(num_bytes).map_err(|_| Error::InvalidInteger {
            position: start,
            detail: "non-ASCII integer".into(),
        })?;

        s.parse::<i64>().map_err(|e| Error::InvalidInteger {
            position: start,
            detail: e.to_string(),
        })
    }

    fn parse_byte_string(&mut self) -> Result<&'de [u8]> {
        let start = self.pos;

        // Parse length prefix
        let colon = self.input[self.pos..]
            .iter()
            .position(|&b| b == b':')
            .ok_or(Error::InvalidByteString {
                position: start,
                detail: "missing ':' separator".into(),
            })?;

        let len_bytes = &self.input[self.pos..self.pos + colon];
        if len_bytes.is_empty() {
            return Err(Error::InvalidByteString {
                position: start,
                detail: "empty length prefix".into(),
            });
        }

        // Reject leading zeros in length (except "0" itself)
        if len_bytes.len() > 1 && len_bytes[0] == b'0' {
            return Err(Error::InvalidByteString {
                position: start,
                detail: "leading zero in length".into(),
            });
        }

        let len_str = std::str::from_utf8(len_bytes).map_err(|_| Error::InvalidByteString {
            position: start,
            detail: "non-ASCII length".into(),
        })?;

        let len: usize =
            len_str
                .parse()
                .map_err(|e: std::num::ParseIntError| Error::InvalidByteString {
                    position: start,
                    detail: e.to_string(),
                })?;

        self.pos += colon + 1; // skip past ':'

        if self.pos + len > self.input.len() {
            return Err(Error::UnexpectedEof {
                position: self.pos,
                context: format!(
                    "byte string needs {len} bytes, only {} available",
                    self.input.len() - self.pos
                ),
            });
        }

        let data = &self.input[self.pos..self.pos + len];
        self.pos += len;
        Ok(data)
    }
}

impl<'de> de::Deserializer<'de> for &mut Deserializer<'de> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self.peek()? {
            b'i' => {
                self.pos += 1;
                let val = self.parse_integer_value()?;
                visitor.visit_i64(val)
            }
            b'l' => self.deserialize_seq(visitor),
            b'd' => self.deserialize_map(visitor),
            b'0'..=b'9' => {
                let data = self.parse_byte_string()?;
                visitor.visit_borrowed_bytes(data)
            }
            byte => Err(Error::UnexpectedByte {
                byte,
                position: self.pos,
                expected: "integer ('i'), string ('0'-'9'), list ('l'), or dict ('d')",
            }),
        }
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.expect(b'i')?;
        let val = self.parse_integer_value()?;
        visitor.visit_bool(val != 0)
    }

    fn deserialize_i8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.expect(b'i')?;
        visitor.visit_i64(self.parse_integer_value()?)
    }

    fn deserialize_i16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.expect(b'i')?;
        visitor.visit_i64(self.parse_integer_value()?)
    }

    fn deserialize_i32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.expect(b'i')?;
        visitor.visit_i64(self.parse_integer_value()?)
    }

    fn deserialize_i64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.expect(b'i')?;
        visitor.visit_i64(self.parse_integer_value()?)
    }

    fn deserialize_u8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.expect(b'i')?;
        visitor.visit_i64(self.parse_integer_value()?)
    }

    fn deserialize_u16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.expect(b'i')?;
        visitor.visit_i64(self.parse_integer_value()?)
    }

    fn deserialize_u32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.expect(b'i')?;
        visitor.visit_i64(self.parse_integer_value()?)
    }

    fn deserialize_u64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.expect(b'i')?;
        visitor.visit_i64(self.parse_integer_value()?)
    }

    fn deserialize_f32<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
        Err(Error::Custom("bencode does not support floats".into()))
    }

    fn deserialize_f64<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
        Err(Error::Custom("bencode does not support floats".into()))
    }

    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let data = self.parse_byte_string()?;
        let s = std::str::from_utf8(data)
            .map_err(|_| Error::Custom("char is not valid UTF-8".into()))?;
        let mut chars = s.chars();
        let c = chars
            .next()
            .ok_or_else(|| Error::Custom("empty string for char".into()))?;
        if chars.next().is_some() {
            return Err(Error::Custom("multi-char string for char".into()));
        }
        visitor.visit_char(c)
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let data = self.parse_byte_string()?;
        let s = std::str::from_utf8(data).map_err(|_| {
            Error::Custom("byte string is not valid UTF-8, use bytes instead".into())
        })?;
        visitor.visit_borrowed_str(s)
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_str(visitor)
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let data = self.parse_byte_string()?;
        visitor.visit_borrowed_bytes(data)
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_bytes(visitor)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_some(self)
    }

    fn deserialize_unit<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
        Err(Error::Custom("bencode does not support unit".into()))
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _visitor: V,
    ) -> Result<V::Value> {
        Err(Error::Custom(
            "bencode does not support unit structs".into(),
        ))
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.expect(b'l')?;
        let value = visitor.visit_seq(SeqAccess { de: self })?;
        self.expect(b'e')?;
        Ok(value)
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.expect(b'd')?;
        let value = visitor.visit_map(MapAccess {
            de: self,
            last_key: None,
        })?;
        self.expect(b'e')?;
        Ok(value)
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_map(visitor)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        match self.peek()? {
            b'd' => {
                // Dict-based enum variant: d<variant-name><value>e
                self.pos += 1;
                let value = visitor.visit_enum(EnumAccess { de: self })?;
                self.expect(b'e')?;
                Ok(value)
            }
            _ => {
                // String-based unit variant
                visitor.visit_enum(UnitVariantAccess { de: self })
            }
        }
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_str(visitor)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        // Skip over any bencode value
        self.skip_value()?;
        visitor.visit_unit()
    }
}

impl Deserializer<'_> {
    /// Skip over a complete bencode value without allocating.
    fn skip_value(&mut self) -> Result<()> {
        match self.peek()? {
            b'i' => {
                self.pos += 1;
                self.parse_integer_value()?;
                Ok(())
            }
            b'l' => {
                self.pos += 1;
                while self.peek()? != b'e' {
                    self.skip_value()?;
                }
                self.pos += 1;
                Ok(())
            }
            b'd' => {
                self.pos += 1;
                while self.peek()? != b'e' {
                    self.parse_byte_string()?; // key
                    self.skip_value()?; // value
                }
                self.pos += 1;
                Ok(())
            }
            b'0'..=b'9' => {
                self.parse_byte_string()?;
                Ok(())
            }
            byte => Err(Error::UnexpectedByte {
                byte,
                position: self.pos,
                expected: "bencode value",
            }),
        }
    }
}

struct SeqAccess<'a, 'de> {
    de: &'a mut Deserializer<'de>,
}

impl<'de> de::SeqAccess<'de> for SeqAccess<'_, 'de> {
    type Error = Error;

    fn next_element_seed<T: de::DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>> {
        if self.de.peek()? == b'e' {
            return Ok(None);
        }
        seed.deserialize(&mut *self.de).map(Some)
    }
}

struct MapAccess<'a, 'de> {
    de: &'a mut Deserializer<'de>,
    last_key: Option<Vec<u8>>,
}

impl<'de> de::MapAccess<'de> for MapAccess<'_, 'de> {
    type Error = Error;

    fn next_key_seed<K: de::DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>> {
        if self.de.peek()? == b'e' {
            return Ok(None);
        }

        // Read key bytes for sort-order validation
        let key_start = self.de.pos;
        let key_data = self.de.parse_byte_string()?;
        let key_vec = key_data.to_vec();

        // Validate sorted order
        if let Some(ref last) = self.last_key
            && key_vec <= *last
        {
            return Err(Error::UnsortedKeys {
                position: key_start,
            });
        }
        self.last_key = Some(key_vec);

        // Now deserialize the key via serde using a special deserializer
        // that yields the already-parsed string
        let key_de = BorrowedStrDeserializer(key_data);
        seed.deserialize(key_de).map(Some)
    }

    fn next_value_seed<V: de::DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value> {
        seed.deserialize(&mut *self.de)
    }
}

/// Minimal deserializer that yields a single already-parsed byte string.
struct BorrowedStrDeserializer<'de>(&'de [u8]);

impl<'de> de::Deserializer<'de> for BorrowedStrDeserializer<'de> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_borrowed_bytes(self.0)
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let s = std::str::from_utf8(self.0)
            .map_err(|_| Error::Custom("dict key is not valid UTF-8".into()))?;
        visitor.visit_borrowed_str(s)
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_str(visitor)
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_borrowed_bytes(self.0)
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_bytes(visitor)
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_str(visitor)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_unit()
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_some(self)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        visitor.visit_newtype_struct(self)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char
        unit unit_struct seq tuple tuple_struct map struct
        enum
    }
}

// Enum support

struct EnumAccess<'a, 'de> {
    de: &'a mut Deserializer<'de>,
}

impl<'de> de::EnumAccess<'de> for EnumAccess<'_, 'de> {
    type Error = Error;
    type Variant = Self;

    fn variant_seed<V: de::DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, Self::Variant)> {
        let val = seed.deserialize(&mut *self.de)?;
        Ok((val, self))
    }
}

impl<'de> de::VariantAccess<'de> for EnumAccess<'_, 'de> {
    type Error = Error;

    fn unit_variant(self) -> Result<()> {
        Err(Error::Custom(
            "expected newtype/tuple/struct variant inside dict".into(),
        ))
    }

    fn newtype_variant_seed<T: de::DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value> {
        seed.deserialize(&mut *self.de)
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value> {
        de::Deserializer::deserialize_seq(&mut *self.de, visitor)
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        de::Deserializer::deserialize_map(&mut *self.de, visitor)
    }
}

struct UnitVariantAccess<'a, 'de> {
    de: &'a mut Deserializer<'de>,
}

impl<'de> de::EnumAccess<'de> for UnitVariantAccess<'_, 'de> {
    type Error = Error;
    type Variant = Self;

    fn variant_seed<V: de::DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, Self::Variant)> {
        let val = seed.deserialize(&mut *self.de)?;
        Ok((val, self))
    }
}

impl<'de> de::VariantAccess<'de> for UnitVariantAccess<'_, 'de> {
    type Error = Error;

    fn unit_variant(self) -> Result<()> {
        Ok(())
    }

    fn newtype_variant_seed<T: de::DeserializeSeed<'de>>(self, _seed: T) -> Result<T::Value> {
        Err(Error::Custom(
            "expected unit variant for string enum".into(),
        ))
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, _visitor: V) -> Result<V::Value> {
        Err(Error::Custom(
            "expected unit variant for string enum".into(),
        ))
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value> {
        Err(Error::Custom(
            "expected unit variant for string enum".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use crate::from_bytes;

    #[test]
    fn deserialize_integer() {
        assert_eq!(from_bytes::<i64>(b"i42e").unwrap(), 42);
        assert_eq!(from_bytes::<i64>(b"i0e").unwrap(), 0);
        assert_eq!(from_bytes::<i64>(b"i-1e").unwrap(), -1);
    }

    #[test]
    fn deserialize_string() {
        assert_eq!(from_bytes::<String>(b"4:spam").unwrap(), "spam");
        assert_eq!(from_bytes::<String>(b"0:").unwrap(), "");
    }

    #[test]
    fn reject_negative_zero() {
        assert!(from_bytes::<i64>(b"i-0e").is_err());
    }

    #[test]
    fn reject_leading_zeros() {
        assert!(from_bytes::<i64>(b"i03e").is_err());
    }

    #[test]
    fn reject_trailing_data() {
        assert!(from_bytes::<i64>(b"i42eXXX").is_err());
    }
}
