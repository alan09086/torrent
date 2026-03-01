//! BEP 53 file selection for magnet URIs.
//!
//! The `so=` parameter selects specific files by index or range.
//! Example: `so=0,2,4-6` selects files 0, 2, 4, 5, and 6.

use crate::FilePriority;

/// A file selection entry from a BEP 53 `so=` parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileSelection {
    /// A single file index.
    Single(usize),
    /// An inclusive range of file indices.
    Range(usize, usize),
}

impl FileSelection {
    /// Parse a `so=` parameter value into a list of file selections.
    ///
    /// Format: comma-separated entries, each either a single index or `start-end` range.
    /// Example: `"0,2,4-6"` -> `[Single(0), Single(2), Range(4, 6)]`
    pub fn parse(value: &str) -> Result<Vec<FileSelection>, String> {
        if value.is_empty() {
            return Err("empty so= value".into());
        }

        let mut selections = Vec::new();
        for part in value.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if let Some((start_str, end_str)) = part.split_once('-') {
                let start: usize = start_str
                    .parse()
                    .map_err(|_| format!("invalid range start: {start_str}"))?;
                let end: usize = end_str
                    .parse()
                    .map_err(|_| format!("invalid range end: {end_str}"))?;
                if start > end {
                    return Err(format!("invalid range: {start}-{end} (start > end)"));
                }
                selections.push(FileSelection::Range(start, end));
            } else {
                let index: usize = part
                    .parse()
                    .map_err(|_| format!("invalid file index: {part}"))?;
                selections.push(FileSelection::Single(index));
            }
        }

        if selections.is_empty() {
            return Err("no valid entries in so= value".into());
        }

        Ok(selections)
    }

    /// Convert file selections into a `FilePriority` vector.
    ///
    /// Selected files get `Normal` priority, unselected get `Skip`.
    /// `num_files` is the total number of files in the torrent.
    /// Out-of-range selections are silently ignored.
    pub fn to_priorities(selections: &[FileSelection], num_files: usize) -> Vec<FilePriority> {
        let mut priorities = vec![FilePriority::Skip; num_files];
        for sel in selections {
            match *sel {
                FileSelection::Single(i) => {
                    if i < num_files {
                        priorities[i] = FilePriority::Normal;
                    }
                }
                FileSelection::Range(start, end) => {
                    let end = end.min(num_files.saturating_sub(1));
                    for p in priorities.iter_mut().take(end + 1).skip(start) {
                        *p = FilePriority::Normal;
                    }
                }
            }
        }
        priorities
    }

    /// Serialize file selections back to a `so=` parameter value string.
    pub fn to_so_value(selections: &[FileSelection]) -> String {
        selections
            .iter()
            .map(|s| match s {
                FileSelection::Single(i) => i.to_string(),
                FileSelection::Range(start, end) => format!("{start}-{end}"),
            })
            .collect::<Vec<_>>()
            .join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_index() {
        let sels = FileSelection::parse("3").unwrap();
        assert_eq!(sels, vec![FileSelection::Single(3)]);
    }

    #[test]
    fn parse_multiple_singles() {
        let sels = FileSelection::parse("0,2,5").unwrap();
        assert_eq!(
            sels,
            vec![
                FileSelection::Single(0),
                FileSelection::Single(2),
                FileSelection::Single(5),
            ]
        );
    }

    #[test]
    fn parse_range() {
        let sels = FileSelection::parse("4-6").unwrap();
        assert_eq!(sels, vec![FileSelection::Range(4, 6)]);
    }

    #[test]
    fn parse_mixed() {
        let sels = FileSelection::parse("0,2,4-6").unwrap();
        assert_eq!(
            sels,
            vec![
                FileSelection::Single(0),
                FileSelection::Single(2),
                FileSelection::Range(4, 6),
            ]
        );
    }

    #[test]
    fn parse_empty_rejected() {
        assert!(FileSelection::parse("").is_err());
    }

    #[test]
    fn parse_invalid_index() {
        assert!(FileSelection::parse("abc").is_err());
    }

    #[test]
    fn parse_invalid_range() {
        assert!(FileSelection::parse("6-4").is_err()); // start > end
    }

    #[test]
    fn to_priorities_basic() {
        let sels = vec![
            FileSelection::Single(0),
            FileSelection::Single(2),
            FileSelection::Range(4, 6),
        ];
        let prios = FileSelection::to_priorities(&sels, 8);
        assert_eq!(prios[0], FilePriority::Normal);
        assert_eq!(prios[1], FilePriority::Skip);
        assert_eq!(prios[2], FilePriority::Normal);
        assert_eq!(prios[3], FilePriority::Skip);
        assert_eq!(prios[4], FilePriority::Normal);
        assert_eq!(prios[5], FilePriority::Normal);
        assert_eq!(prios[6], FilePriority::Normal);
        assert_eq!(prios[7], FilePriority::Skip);
    }

    #[test]
    fn to_priorities_out_of_range_ignored() {
        let sels = vec![FileSelection::Single(10), FileSelection::Range(8, 12)];
        let prios = FileSelection::to_priorities(&sels, 5);
        // All should remain Skip since indices are beyond num_files
        assert!(prios.iter().all(|&p| p == FilePriority::Skip));
    }

    #[test]
    fn to_priorities_partial_range() {
        let sels = vec![FileSelection::Range(3, 100)];
        let prios = FileSelection::to_priorities(&sels, 5);
        assert_eq!(prios[0], FilePriority::Skip);
        assert_eq!(prios[1], FilePriority::Skip);
        assert_eq!(prios[2], FilePriority::Skip);
        assert_eq!(prios[3], FilePriority::Normal);
        assert_eq!(prios[4], FilePriority::Normal);
    }

    #[test]
    fn so_value_round_trip() {
        let sels = vec![
            FileSelection::Single(0),
            FileSelection::Single(2),
            FileSelection::Range(4, 6),
        ];
        let value = FileSelection::to_so_value(&sels);
        assert_eq!(value, "0,2,4-6");
        let parsed = FileSelection::parse(&value).unwrap();
        assert_eq!(parsed, sels);
    }
}
