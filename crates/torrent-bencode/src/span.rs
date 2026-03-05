use crate::error::{Error, Result};

/// Find the raw byte span of a value for a given key in a bencoded dictionary.
///
/// This is critical for info-hash computation: the info-hash is the SHA1 of the
/// *original raw bytes* of the "info" dictionary value, not a re-serialized copy.
///
/// Returns the byte range `start..end` of the value associated with `key`.
///
/// # Example
///
/// ```
/// use torrent_bencode::find_dict_key_span;
///
/// let data = b"d4:infod4:name4:test12:piece lengthi1024eee";
/// let span = find_dict_key_span(data, "info").unwrap();
/// assert_eq!(&data[span.clone()], b"d4:name4:test12:piece lengthi1024ee");
/// ```
pub fn find_dict_key_span(data: &[u8], key: &str) -> Result<std::ops::Range<usize>> {
    let mut pos = 0;

    // Expect dict start
    if data.get(pos) != Some(&b'd') {
        return Err(Error::NotADictionary { position: pos });
    }
    pos += 1;

    let key_bytes = key.as_bytes();

    loop {
        // Check for dict end
        if data.get(pos) == Some(&b'e') {
            return Err(Error::KeyNotFound {
                key: key.to_string(),
            });
        }

        if pos >= data.len() {
            return Err(Error::UnexpectedEof {
                position: pos,
                context: "while scanning dict for key".into(),
            });
        }

        // Parse key (byte string)
        let parsed_key = parse_byte_string(data, &mut pos)?;

        // Record value start
        let value_start = pos;

        // Skip value
        skip_value(data, &mut pos)?;

        // Check if this was our target key
        if parsed_key == key_bytes {
            return Ok(value_start..pos);
        }
    }
}

/// Parse a bencode byte string, returning the string data and advancing `pos`.
fn parse_byte_string<'a>(data: &'a [u8], pos: &mut usize) -> Result<&'a [u8]> {
    let start = *pos;

    // Find colon
    let colon = data[*pos..]
        .iter()
        .position(|&b| b == b':')
        .ok_or(Error::InvalidByteString {
            position: start,
            detail: "missing ':'".into(),
        })?;

    let len_str =
        std::str::from_utf8(&data[*pos..*pos + colon]).map_err(|_| Error::InvalidByteString {
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

    *pos += colon + 1;

    if *pos + len > data.len() {
        return Err(Error::UnexpectedEof {
            position: *pos,
            context: format!("byte string needs {len} bytes"),
        });
    }

    let result = &data[*pos..*pos + len];
    *pos += len;
    Ok(result)
}

/// Skip over a complete bencode value, advancing `pos` past it.
fn skip_value(data: &[u8], pos: &mut usize) -> Result<()> {
    match data.get(*pos) {
        Some(b'i') => {
            *pos += 1;
            let end = data[*pos..]
                .iter()
                .position(|&b| b == b'e')
                .ok_or(Error::UnexpectedEof {
                    position: *pos,
                    context: "unterminated integer".into(),
                })?;
            *pos += end + 1;
            Ok(())
        }
        Some(b'l') => {
            *pos += 1;
            while data.get(*pos) != Some(&b'e') {
                if *pos >= data.len() {
                    return Err(Error::UnexpectedEof {
                        position: *pos,
                        context: "unterminated list".into(),
                    });
                }
                skip_value(data, pos)?;
            }
            *pos += 1; // skip 'e'
            Ok(())
        }
        Some(b'd') => {
            *pos += 1;
            while data.get(*pos) != Some(&b'e') {
                if *pos >= data.len() {
                    return Err(Error::UnexpectedEof {
                        position: *pos,
                        context: "unterminated dict".into(),
                    });
                }
                parse_byte_string(data, pos)?; // key
                skip_value(data, pos)?; // value
            }
            *pos += 1; // skip 'e'
            Ok(())
        }
        Some(b'0'..=b'9') => {
            parse_byte_string(data, pos)?;
            Ok(())
        }
        Some(&byte) => Err(Error::UnexpectedByte {
            byte,
            position: *pos,
            expected: "bencode value",
        }),
        None => Err(Error::UnexpectedEof {
            position: *pos,
            context: "expected value".into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_info_key() {
        let data = b"d4:infod4:name4:test12:piece lengthi1024ee8:url-list4:httpe";
        let span = find_dict_key_span(data, "info").unwrap();
        assert_eq!(&data[span], b"d4:name4:test12:piece lengthi1024ee");
    }

    #[test]
    fn find_last_key() {
        let data = b"d1:ai1e1:bi2e1:ci3ee";
        let span = find_dict_key_span(data, "c").unwrap();
        assert_eq!(&data[span], b"i3e");
    }

    #[test]
    fn key_not_found() {
        let data = b"d1:ai1ee";
        assert!(matches!(
            find_dict_key_span(data, "z"),
            Err(Error::KeyNotFound { .. })
        ));
    }

    #[test]
    fn not_a_dict() {
        assert!(matches!(
            find_dict_key_span(b"i42e", "info"),
            Err(Error::NotADictionary { .. })
        ));
    }

    #[test]
    fn nested_dict_value() {
        let data = b"d5:outerd5:inner3:valee";
        let span = find_dict_key_span(data, "outer").unwrap();
        assert_eq!(&data[span], b"d5:inner3:vale");
    }
}
