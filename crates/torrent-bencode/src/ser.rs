use serde::ser::{self, Serialize};
use std::io::Write;

use crate::error::{Error, Result};

/// Bencode serializer.
///
/// Writes bencode-encoded data to any `std::io::Write` sink.
pub struct Serializer<W> {
    writer: W,
}

impl<W: Write> Serializer<W> {
    /// Create a new serializer writing to the given sink.
    pub fn new(writer: W) -> Self {
        Serializer { writer }
    }

    fn write_byte_string(&mut self, v: &[u8]) -> Result<()> {
        write!(self.writer, "{}:", v.len()).map_err(|e| Error::Custom(e.to_string()))?;
        self.writer
            .write_all(v)
            .map_err(|e| Error::Custom(e.to_string()))
    }

    fn write_integer(&mut self, v: i64) -> Result<()> {
        write!(self.writer, "i{v}e").map_err(|e| Error::Custom(e.to_string()))
    }
}

impl<'a, W: Write> ser::Serializer for &'a mut Serializer<W> {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = &'a mut Serializer<W>;
    type SerializeTuple = &'a mut Serializer<W>;
    type SerializeTupleStruct = &'a mut Serializer<W>;
    type SerializeTupleVariant = &'a mut Serializer<W>;
    type SerializeMap = SortedMapSerializer<'a, W>;
    type SerializeStruct = SortedMapSerializer<'a, W>;
    type SerializeStructVariant = SortedMapSerializer<'a, W>;

    fn serialize_bool(self, v: bool) -> Result<()> {
        self.write_integer(if v { 1 } else { 0 })
    }

    fn serialize_i8(self, v: i8) -> Result<()> {
        self.write_integer(v as i64)
    }

    fn serialize_i16(self, v: i16) -> Result<()> {
        self.write_integer(v as i64)
    }

    fn serialize_i32(self, v: i32) -> Result<()> {
        self.write_integer(v as i64)
    }

    fn serialize_i64(self, v: i64) -> Result<()> {
        self.write_integer(v)
    }

    fn serialize_u8(self, v: u8) -> Result<()> {
        self.write_integer(v as i64)
    }

    fn serialize_u16(self, v: u16) -> Result<()> {
        self.write_integer(v as i64)
    }

    fn serialize_u32(self, v: u32) -> Result<()> {
        self.write_integer(v as i64)
    }

    fn serialize_u64(self, v: u64) -> Result<()> {
        self.write_integer(v as i64)
    }

    fn serialize_f32(self, _v: f32) -> Result<()> {
        Err(Error::Custom("bencode does not support floats".into()))
    }

    fn serialize_f64(self, _v: f64) -> Result<()> {
        Err(Error::Custom("bencode does not support floats".into()))
    }

    fn serialize_char(self, v: char) -> Result<()> {
        let mut buf = [0u8; 4];
        let s = v.encode_utf8(&mut buf);
        self.write_byte_string(s.as_bytes())
    }

    fn serialize_str(self, v: &str) -> Result<()> {
        self.write_byte_string(v.as_bytes())
    }

    fn serialize_bytes(self, v: &[u8]) -> Result<()> {
        self.write_byte_string(v)
    }

    fn serialize_none(self) -> Result<()> {
        Err(Error::Custom("bencode does not support None/null".into()))
    }

    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<()> {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<()> {
        Err(Error::Custom("bencode does not support unit".into()))
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<()> {
        Err(Error::Custom("bencode does not support unit structs".into()))
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
    ) -> Result<()> {
        self.write_byte_string(variant.as_bytes())
    }

    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<()> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<()> {
        self.writer
            .write_all(b"d")
            .map_err(|e| Error::Custom(e.to_string()))?;
        self.write_byte_string(variant.as_bytes())?;
        value.serialize(&mut *self)?;
        self.writer
            .write_all(b"e")
            .map_err(|e| Error::Custom(e.to_string()))
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq> {
        self.writer
            .write_all(b"l")
            .map_err(|e| Error::Custom(e.to_string()))?;
        Ok(self)
    }

    fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple> {
        self.serialize_seq(Some(len))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleStruct> {
        self.serialize_seq(Some(len))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant> {
        self.writer
            .write_all(b"d")
            .map_err(|e| Error::Custom(e.to_string()))?;
        self.write_byte_string(variant.as_bytes())?;
        self.writer
            .write_all(b"l")
            .map_err(|e| Error::Custom(e.to_string()))?;
        Ok(self)
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap> {
        Ok(SortedMapSerializer::new(self))
    }

    fn serialize_struct(self, _name: &'static str, _len: usize) -> Result<Self::SerializeStruct> {
        Ok(SortedMapSerializer::new(self))
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant> {
        // Wrap in a dict: d<variant><dict-value>e
        // The outer dict is handled by the caller; we buffer this inner dict.
        // Actually, for struct variants we need to write the outer dict key.
        self.writer
            .write_all(b"d")
            .map_err(|e| Error::Custom(e.to_string()))?;
        self.write_byte_string(variant.as_bytes())?;
        Ok(SortedMapSerializer::new_nested(self))
    }
}

// Sequence serialization
impl<W: Write> ser::SerializeSeq for &mut Serializer<W> {
    type Ok = ();
    type Error = Error;

    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<()> {
        value.serialize(&mut **self)
    }

    fn end(self) -> Result<()> {
        self.writer
            .write_all(b"e")
            .map_err(|e| Error::Custom(e.to_string()))
    }
}

impl<W: Write> ser::SerializeTuple for &mut Serializer<W> {
    type Ok = ();
    type Error = Error;

    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<()> {
        ser::SerializeSeq::serialize_element(self, value)
    }

    fn end(self) -> Result<()> {
        ser::SerializeSeq::end(self)
    }
}

impl<W: Write> ser::SerializeTupleStruct for &mut Serializer<W> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<()> {
        ser::SerializeSeq::serialize_element(self, value)
    }

    fn end(self) -> Result<()> {
        ser::SerializeSeq::end(self)
    }
}

impl<W: Write> ser::SerializeTupleVariant for &mut Serializer<W> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<()> {
        ser::SerializeSeq::serialize_element(self, value)
    }

    fn end(self) -> Result<()> {
        // Close list, then close outer dict
        self.writer
            .write_all(b"e")
            .map_err(|e| Error::Custom(e.to_string()))?;
        self.writer
            .write_all(b"e")
            .map_err(|e| Error::Custom(e.to_string()))
    }
}

/// Buffering map serializer that sorts keys before writing.
///
/// BEP 3 requires dictionary keys to be in sorted order. We buffer all
/// key-value pairs, sort by key, then write them all at once in `end()`.
pub struct SortedMapSerializer<'a, W> {
    ser: &'a mut Serializer<W>,
    entries: Vec<(Vec<u8>, Vec<u8>)>,
    current_key: Option<Vec<u8>>,
    /// If true, we need to close an outer dict wrapper on end().
    nested: bool,
}

impl<'a, W: Write> SortedMapSerializer<'a, W> {
    fn new(ser: &'a mut Serializer<W>) -> Self {
        SortedMapSerializer {
            ser,
            entries: Vec::new(),
            current_key: None,
            nested: false,
        }
    }

    fn new_nested(ser: &'a mut Serializer<W>) -> Self {
        SortedMapSerializer {
            ser,
            entries: Vec::new(),
            current_key: None,
            nested: true,
        }
    }

    fn flush_sorted(&mut self) -> Result<()> {
        self.entries.sort_by(|a, b| a.0.cmp(&b.0));
        self.ser
            .writer
            .write_all(b"d")
            .map_err(|e| Error::Custom(e.to_string()))?;
        for (key, value) in &self.entries {
            self.ser.write_byte_string(key)?;
            self.ser
                .writer
                .write_all(value)
                .map_err(|e| Error::Custom(e.to_string()))?;
        }
        self.ser
            .writer
            .write_all(b"e")
            .map_err(|e| Error::Custom(e.to_string()))
    }
}

impl<W: Write> ser::SerializeMap for SortedMapSerializer<'_, W> {
    type Ok = ();
    type Error = Error;

    fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> Result<()> {
        let mut buf = Vec::new();
        let mut key_ser = Serializer::new(&mut buf);
        key.serialize(&mut key_ser)?;
        // Extract the raw key bytes from the serialized bencode string.
        // Keys serialize as `<len>:<bytes>`, so we need the raw bytes.
        self.current_key = Some(extract_string_content(&buf)?);
        Ok(())
    }

    fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<()> {
        let key = self
            .current_key
            .take()
            .ok_or_else(|| Error::Custom("serialize_value called before serialize_key".into()))?;
        let mut buf = Vec::new();
        let mut val_ser = Serializer::new(&mut buf);
        value.serialize(&mut val_ser)?;
        self.entries.push((key, buf));
        Ok(())
    }

    fn end(mut self) -> Result<()> {
        self.flush_sorted()?;
        if self.nested {
            self.ser
                .writer
                .write_all(b"e")
                .map_err(|e| Error::Custom(e.to_string()))?;
        }
        Ok(())
    }
}

impl<W: Write> ser::SerializeStruct for SortedMapSerializer<'_, W> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<()> {
        let mut buf = Vec::new();
        let mut val_ser = Serializer::new(&mut buf);
        value.serialize(&mut val_ser)?;
        self.entries.push((key.as_bytes().to_vec(), buf));
        Ok(())
    }

    fn end(mut self) -> Result<()> {
        self.flush_sorted()?;
        if self.nested {
            self.ser
                .writer
                .write_all(b"e")
                .map_err(|e| Error::Custom(e.to_string()))?;
        }
        Ok(())
    }

    fn skip_field(&mut self, _key: &'static str) -> Result<()> {
        Ok(())
    }
}

impl<W: Write> ser::SerializeStructVariant for SortedMapSerializer<'_, W> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<()> {
        ser::SerializeStruct::serialize_field(self, key, value)
    }

    fn end(self) -> Result<()> {
        ser::SerializeStruct::end(self)
    }
}

/// Extract the raw byte content from a bencode-encoded string `<len>:<data>`.
fn extract_string_content(encoded: &[u8]) -> Result<Vec<u8>> {
    let colon_pos = encoded
        .iter()
        .position(|&b| b == b':')
        .ok_or_else(|| Error::Custom("map key must be a string".into()))?;
    Ok(encoded[colon_pos + 1..].to_vec())
}

#[cfg(test)]
mod tests {
    use serde::Serialize;

    #[test]
    fn serialize_integer() {
        assert_eq!(to_bytes(&42i64), b"i42e");
        assert_eq!(to_bytes(&0i64), b"i0e");
        assert_eq!(to_bytes(&-1i64), b"i-1e");
    }

    #[test]
    fn serialize_string() {
        assert_eq!(to_bytes("spam"), b"4:spam");
        assert_eq!(to_bytes(""), b"0:");
    }

    #[test]
    fn serialize_list() {
        let list = vec!["spam", "eggs"];
        assert_eq!(to_bytes(&list), b"l4:spam4:eggse");
    }

    #[test]
    fn serialize_struct_sorted_keys() {
        #[derive(Serialize)]
        struct Test {
            zebra: i64,
            alpha: i64,
        }
        let val = Test {
            zebra: 2,
            alpha: 1,
        };
        // Keys must be sorted: alpha before zebra
        assert_eq!(to_bytes(&val), b"d5:alphai1e5:zebrai2ee");
    }

    fn to_bytes<T: Serialize + ?Sized>(value: &T) -> Vec<u8> {
        crate::to_bytes(value).unwrap()
    }
}
