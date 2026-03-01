//! BEP 52 v2 file tree types and parsing.
//!
//! The v2 file tree is a nested dict where path components are dict keys
//! and the empty string `""` holds file attributes. This cannot use serde
//! derive because keys are arbitrary filenames — requires manual
//! `BencodeValue::Dict` walking.

use std::collections::BTreeMap;

use ferrite_bencode::BencodeValue;

use crate::error::Error;
use crate::hash::Id32;

/// Attributes of a single file in a v2 file tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V2FileAttr {
    /// File length in bytes.
    pub length: u64,
    /// Merkle root of the file's block hashes. `None` for empty files (length == 0).
    pub pieces_root: Option<Id32>,
}

/// A node in the v2 file tree — either a file or a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileTreeNode {
    /// A file with its attributes.
    File(V2FileAttr),
    /// A directory containing named children.
    Directory(BTreeMap<String, FileTreeNode>),
}

/// Flattened file info from a v2 file tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V2FileInfo {
    /// Path components from the tree root to this file.
    pub path: Vec<String>,
    /// File attributes.
    pub attr: V2FileAttr,
}

impl FileTreeNode {
    /// Parse a file tree node from a `BencodeValue`.
    ///
    /// A node is a dict. If it contains a `""` key, it's a file node (the `""`
    /// value holds file attributes). Otherwise, each key is a directory entry
    /// name and its value is a child `FileTreeNode`.
    pub fn from_bencode(value: &BencodeValue) -> Result<Self, Error> {
        let dict = value
            .as_dict()
            .ok_or_else(|| Error::InvalidTorrent("file tree node must be a dict".into()))?;

        // Check for the empty-string key which signals a file node
        if let Some(attr_value) = dict.get(b"".as_ref()) {
            let attr = parse_file_attr(attr_value)?;
            return Ok(FileTreeNode::File(attr));
        }

        // Otherwise it's a directory — each key is a child name
        let mut children = BTreeMap::new();
        for (key, child_value) in dict {
            let name = String::from_utf8(key.clone()).map_err(|_| {
                Error::InvalidTorrent("file tree key is not valid UTF-8".into())
            })?;
            let child = FileTreeNode::from_bencode(child_value)?;
            children.insert(name, child);
        }

        Ok(FileTreeNode::Directory(children))
    }

    /// Recursively flatten the tree into a list of files with full paths.
    pub fn flatten(&self) -> Vec<V2FileInfo> {
        let mut result = Vec::new();
        self.flatten_into(&mut result, &mut Vec::new());
        result
    }

    fn flatten_into(&self, result: &mut Vec<V2FileInfo>, path: &mut Vec<String>) {
        match self {
            FileTreeNode::File(attr) => {
                result.push(V2FileInfo {
                    path: path.clone(),
                    attr: attr.clone(),
                });
            }
            FileTreeNode::Directory(children) => {
                for (name, child) in children {
                    path.push(name.clone());
                    child.flatten_into(result, path);
                    path.pop();
                }
            }
        }
    }
}

/// Parse file attributes from the `""` key's value dict.
fn parse_file_attr(value: &BencodeValue) -> Result<V2FileAttr, Error> {
    let dict = value
        .as_dict()
        .ok_or_else(|| Error::InvalidTorrent("file attr must be a dict".into()))?;

    let length = dict
        .get(b"length".as_ref())
        .and_then(|v| v.as_int())
        .ok_or_else(|| Error::InvalidTorrent("file attr missing 'length'".into()))?;

    if length < 0 {
        return Err(Error::InvalidTorrent(format!(
            "file attr has negative length: {length}"
        )));
    }

    let pieces_root = if let Some(root_val) = dict.get(b"pieces root".as_ref()) {
        let bytes = root_val
            .as_bytes_raw()
            .ok_or_else(|| Error::InvalidTorrent("pieces root must be bytes".into()))?;
        Some(Id32::from_bytes(bytes)?)
    } else {
        None
    };

    Ok(V2FileAttr {
        length: length as u64,
        pieces_root,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to build a BencodeValue dict from key-value pairs.
    fn bdict(pairs: Vec<(&[u8], BencodeValue)>) -> BencodeValue {
        let mut map = BTreeMap::new();
        for (k, v) in pairs {
            map.insert(k.to_vec(), v);
        }
        BencodeValue::Dict(map)
    }

    fn bint(v: i64) -> BencodeValue {
        BencodeValue::Integer(v)
    }

    fn bbytes(v: &[u8]) -> BencodeValue {
        BencodeValue::Bytes(v.to_vec())
    }

    #[test]
    fn single_file() {
        // { "test.txt": { "": { "length": 1024, "pieces root": <32 bytes> } } }
        let root_hash = [0xABu8; 32];
        let tree = bdict(vec![(
            b"test.txt",
            bdict(vec![(
                b"",
                bdict(vec![
                    (b"length", bint(1024)),
                    (b"pieces root", bbytes(&root_hash)),
                ]),
            )]),
        )]);

        let node = FileTreeNode::from_bencode(&tree).unwrap();
        let files = node.flatten();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, vec!["test.txt"]);
        assert_eq!(files[0].attr.length, 1024);
        assert_eq!(files[0].attr.pieces_root, Some(Id32(root_hash)));
    }

    #[test]
    fn nested_directory() {
        // { "dir": { "subfile.dat": { "": { "length": 512 } } } }
        let tree = bdict(vec![(
            b"dir",
            bdict(vec![(
                b"subfile.dat",
                bdict(vec![(b"", bdict(vec![(b"length", bint(512))]))]),
            )]),
        )]);

        let node = FileTreeNode::from_bencode(&tree).unwrap();
        let files = node.flatten();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, vec!["dir", "subfile.dat"]);
        assert_eq!(files[0].attr.length, 512);
        assert_eq!(files[0].attr.pieces_root, None);
    }

    #[test]
    fn multiple_files_btreemap_ordering() {
        // BTreeMap sorts lexicographically, so "alpha" < "beta"
        let tree = bdict(vec![
            (
                b"beta.txt",
                bdict(vec![(b"", bdict(vec![(b"length", bint(200))]))]),
            ),
            (
                b"alpha.txt",
                bdict(vec![(b"", bdict(vec![(b"length", bint(100))]))]),
            ),
        ]);

        let node = FileTreeNode::from_bencode(&tree).unwrap();
        let files = node.flatten();
        assert_eq!(files.len(), 2);
        // BTreeMap iterates in sorted order
        assert_eq!(files[0].path, vec!["alpha.txt"]);
        assert_eq!(files[1].path, vec!["beta.txt"]);
    }

    #[test]
    fn reject_missing_length() {
        // File attr without "length" key
        let tree = bdict(vec![(
            b"bad.txt",
            bdict(vec![(b"", bdict(vec![(b"pieces root", bbytes(&[0u8; 32]))]))]),
        )]);

        assert!(FileTreeNode::from_bencode(&tree).is_err());
    }

    #[test]
    fn reject_non_dict() {
        let value = BencodeValue::Integer(42);
        assert!(FileTreeNode::from_bencode(&value).is_err());
    }

    #[test]
    fn empty_file_no_pieces_root() {
        // Empty files (length == 0) should not have pieces_root
        let tree = bdict(vec![(
            b"empty.txt",
            bdict(vec![(b"", bdict(vec![(b"length", bint(0))]))]),
        )]);

        let node = FileTreeNode::from_bencode(&tree).unwrap();
        let files = node.flatten();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].attr.length, 0);
        assert_eq!(files[0].attr.pieces_root, None);
    }
}
