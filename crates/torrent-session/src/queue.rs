//! Queue position arithmetic for auto-managed torrents.

use torrent_core::Id20;

/// Compact representation of a queued torrent for position operations.
#[derive(Debug, Clone)]
pub(crate) struct QueueEntry {
    pub info_hash: Id20,
    pub position: i32,
}

/// Assigns a position at the end of the queue. Returns the assigned position.
#[allow(dead_code)] // used by tests; session uses SessionActor::next_queue_position() instead
pub(crate) fn append_position(entries: &[QueueEntry]) -> i32 {
    entries
        .iter()
        .map(|e| e.position)
        .max()
        .map(|m| m + 1)
        .unwrap_or(0)
}

/// Removes a position and shifts all positions above it down by 1.
/// Returns the list of (info_hash, old_pos, new_pos) that changed.
pub(crate) fn remove_position(entries: &mut Vec<QueueEntry>, pos: i32) -> Vec<(Id20, i32, i32)> {
    entries.retain(|e| e.position != pos);
    let mut changed = Vec::new();
    for entry in entries.iter_mut() {
        if entry.position > pos {
            let old = entry.position;
            entry.position -= 1;
            changed.push((entry.info_hash, old, entry.position));
        }
    }
    changed
}

/// Moves a torrent to a new absolute position. Shifts others to maintain density.
/// Returns the list of (info_hash, old_pos, new_pos) for all changed entries.
pub(crate) fn set_position(
    entries: &mut [QueueEntry],
    info_hash: Id20,
    new_pos: i32,
) -> Vec<(Id20, i32, i32)> {
    let new_pos = new_pos.clamp(0, entries.len().saturating_sub(1) as i32);
    let old_pos = match entries.iter().find(|e| e.info_hash == info_hash) {
        Some(e) => e.position,
        None => return Vec::new(),
    };
    if old_pos == new_pos {
        return Vec::new();
    }

    let mut changed = Vec::new();

    if new_pos < old_pos {
        // Moving up: shift entries in [new_pos, old_pos) down by +1
        for entry in entries.iter_mut() {
            if entry.info_hash == info_hash {
                changed.push((entry.info_hash, old_pos, new_pos));
                entry.position = new_pos;
            } else if entry.position >= new_pos && entry.position < old_pos {
                let old = entry.position;
                entry.position += 1;
                changed.push((entry.info_hash, old, entry.position));
            }
        }
    } else {
        // Moving down: shift entries in (old_pos, new_pos] up by -1
        for entry in entries.iter_mut() {
            if entry.info_hash == info_hash {
                changed.push((entry.info_hash, old_pos, new_pos));
                entry.position = new_pos;
            } else if entry.position > old_pos && entry.position <= new_pos {
                let old = entry.position;
                entry.position -= 1;
                changed.push((entry.info_hash, old, entry.position));
            }
        }
    }

    changed
}

/// Move one position up (lower number = higher priority). Returns changes.
pub(crate) fn move_up(entries: &mut [QueueEntry], info_hash: Id20) -> Vec<(Id20, i32, i32)> {
    let pos = match entries.iter().find(|e| e.info_hash == info_hash) {
        Some(e) if e.position > 0 => e.position,
        _ => return Vec::new(),
    };
    set_position(entries, info_hash, pos - 1)
}

/// Move one position down. Returns changes.
pub(crate) fn move_down(entries: &mut [QueueEntry], info_hash: Id20) -> Vec<(Id20, i32, i32)> {
    let max_pos = entries.iter().map(|e| e.position).max().unwrap_or(0);
    let pos = match entries.iter().find(|e| e.info_hash == info_hash) {
        Some(e) if e.position < max_pos => e.position,
        _ => return Vec::new(),
    };
    set_position(entries, info_hash, pos + 1)
}

/// Move to position 0 (highest priority). Returns changes.
pub(crate) fn move_top(entries: &mut [QueueEntry], info_hash: Id20) -> Vec<(Id20, i32, i32)> {
    set_position(entries, info_hash, 0)
}

/// Move to last position (lowest priority). Returns changes.
pub(crate) fn move_bottom(entries: &mut [QueueEntry], info_hash: Id20) -> Vec<(Id20, i32, i32)> {
    let max_pos = entries.len().saturating_sub(1) as i32;
    set_position(entries, info_hash, max_pos)
}

// ---------------------------------------------------------------------------
// Auto-manage evaluation
// ---------------------------------------------------------------------------

/// Classification of a torrent for queue evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueueCategory {
    Downloading,
    Seeding,
}

/// Snapshot of a torrent for queue evaluation.
#[derive(Debug, Clone)]
pub(crate) struct QueueCandidate {
    pub info_hash: Id20,
    pub position: i32,
    pub category: QueueCategory,
    pub is_active: bool,
    pub is_inactive: bool,
}

/// Result of queue evaluation: which torrents to start and stop.
#[derive(Debug, Default)]
pub(crate) struct QueueDecision {
    pub to_resume: Vec<Id20>,
    pub to_pause: Vec<Id20>,
}

/// Evaluate the queue and decide which torrents to start/stop.
///
/// Negative limits mean "unlimited" for that category.
/// When `prefer_seeds` is true, seeding slots are allocated before download slots.
pub(crate) fn evaluate(
    candidates: &[QueueCandidate],
    active_downloads: i32,
    active_seeds: i32,
    active_limit: i32,
    dont_count_slow: bool,
    prefer_seeds: bool,
) -> QueueDecision {
    let mut decision = QueueDecision::default();

    let mut downloads: Vec<_> = candidates
        .iter()
        .filter(|c| c.category == QueueCategory::Downloading)
        .collect();
    downloads.sort_by_key(|c| c.position);

    let mut seeds: Vec<_> = candidates
        .iter()
        .filter(|c| c.category == QueueCategory::Seeding)
        .collect();
    seeds.sort_by_key(|c| c.position);

    let mut total_active: i32 = 0;

    let groups: Vec<(&[&QueueCandidate], i32)> = if prefer_seeds {
        vec![(&seeds, active_seeds), (&downloads, active_downloads)]
    } else {
        vec![(&downloads, active_downloads), (&seeds, active_seeds)]
    };

    for (group, limit) in groups {
        let mut category_active: i32 = 0;

        for candidate in group {
            let counts_toward_limit = !(dont_count_slow && candidate.is_inactive);

            if candidate.is_active {
                if counts_toward_limit {
                    category_active += 1;
                    total_active += 1;
                }
                let over_category = limit >= 0 && category_active > limit;
                let over_total = active_limit >= 0 && total_active > active_limit;
                if over_category || over_total {
                    decision.to_pause.push(candidate.info_hash);
                    if counts_toward_limit {
                        category_active -= 1;
                        total_active -= 1;
                    }
                }
            } else {
                let under_category = limit < 0 || category_active < limit;
                let under_total = active_limit < 0 || total_active < active_limit;
                if under_category && under_total {
                    decision.to_resume.push(candidate.info_hash);
                    if counts_toward_limit {
                        category_active += 1;
                        total_active += 1;
                    }
                }
            }
        }
    }

    decision
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_hash(n: u8) -> Id20 {
        Id20::from([n; 20])
    }

    fn make_entries(n: usize) -> Vec<QueueEntry> {
        (0..n)
            .map(|i| QueueEntry {
                info_hash: make_hash(i as u8),
                position: i as i32,
            })
            .collect()
    }

    fn positions(entries: &[QueueEntry]) -> Vec<(u8, i32)> {
        let mut v: Vec<_> = entries
            .iter()
            .map(|e| (e.info_hash.as_ref()[0], e.position))
            .collect();
        v.sort_by_key(|&(_, pos)| pos);
        v
    }

    // ---- Position arithmetic ----

    #[test]
    fn append_to_empty() {
        assert_eq!(append_position(&[]), 0);
    }

    #[test]
    fn append_to_existing() {
        let entries = make_entries(3);
        assert_eq!(append_position(&entries), 3);
    }

    #[test]
    fn remove_middle_shifts_down() {
        let mut entries = make_entries(4);
        let changed = remove_position(&mut entries, 1);
        assert_eq!(entries.len(), 3);
        assert_eq!(positions(&entries), vec![(0, 0), (2, 1), (3, 2)]);
        assert_eq!(changed.len(), 2);
    }

    #[test]
    fn remove_last_no_shifts() {
        let mut entries = make_entries(3);
        let changed = remove_position(&mut entries, 2);
        assert_eq!(entries.len(), 2);
        assert_eq!(positions(&entries), vec![(0, 0), (1, 1)]);
        assert!(changed.is_empty());
    }

    #[test]
    fn set_position_move_up() {
        let mut entries = make_entries(4);
        let changed = set_position(&mut entries, make_hash(3), 1);
        assert_eq!(positions(&entries), vec![(0, 0), (3, 1), (1, 2), (2, 3)]);
        assert_eq!(changed.len(), 3);
    }

    #[test]
    fn set_position_move_down() {
        let mut entries = make_entries(4);
        let changed = set_position(&mut entries, make_hash(0), 2);
        assert_eq!(positions(&entries), vec![(1, 0), (2, 1), (0, 2), (3, 3)]);
        assert_eq!(changed.len(), 3);
    }

    #[test]
    fn set_position_same_is_noop() {
        let mut entries = make_entries(3);
        let changed = set_position(&mut entries, make_hash(1), 1);
        assert!(changed.is_empty());
    }

    #[test]
    fn move_up_from_zero_is_noop() {
        let mut entries = make_entries(3);
        let changed = move_up(&mut entries, make_hash(0));
        assert!(changed.is_empty());
    }

    #[test]
    fn move_up_swaps_adjacent() {
        let mut entries = make_entries(3);
        let changed = move_up(&mut entries, make_hash(2));
        assert_eq!(positions(&entries), vec![(0, 0), (2, 1), (1, 2)]);
        assert_eq!(changed.len(), 2);
    }

    #[test]
    fn move_down_from_last_is_noop() {
        let mut entries = make_entries(3);
        let changed = move_down(&mut entries, make_hash(2));
        assert!(changed.is_empty());
    }

    #[test]
    fn move_top_sends_to_front() {
        let mut entries = make_entries(4);
        let _changed = move_top(&mut entries, make_hash(3));
        assert_eq!(positions(&entries), vec![(3, 0), (0, 1), (1, 2), (2, 3)]);
    }

    #[test]
    fn move_bottom_sends_to_end() {
        let mut entries = make_entries(4);
        let _changed = move_bottom(&mut entries, make_hash(0));
        assert_eq!(positions(&entries), vec![(1, 0), (2, 1), (3, 2), (0, 3)]);
    }

    // ---- Evaluate algorithm ----

    #[test]
    fn evaluate_starts_up_to_limit() {
        let candidates = vec![
            QueueCandidate {
                info_hash: make_hash(0),
                position: 0,
                category: QueueCategory::Downloading,
                is_active: false,
                is_inactive: false,
            },
            QueueCandidate {
                info_hash: make_hash(1),
                position: 1,
                category: QueueCategory::Downloading,
                is_active: false,
                is_inactive: false,
            },
            QueueCandidate {
                info_hash: make_hash(2),
                position: 2,
                category: QueueCategory::Downloading,
                is_active: false,
                is_inactive: false,
            },
        ];
        let decision = evaluate(&candidates, 2, 5, 500, true, false);
        assert_eq!(decision.to_resume.len(), 2);
        assert_eq!(decision.to_resume[0], make_hash(0));
        assert_eq!(decision.to_resume[1], make_hash(1));
        assert!(decision.to_pause.is_empty());
    }

    #[test]
    fn evaluate_pauses_over_limit() {
        let candidates = vec![
            QueueCandidate {
                info_hash: make_hash(0),
                position: 0,
                category: QueueCategory::Downloading,
                is_active: true,
                is_inactive: false,
            },
            QueueCandidate {
                info_hash: make_hash(1),
                position: 1,
                category: QueueCategory::Downloading,
                is_active: true,
                is_inactive: false,
            },
            QueueCandidate {
                info_hash: make_hash(2),
                position: 2,
                category: QueueCategory::Downloading,
                is_active: true,
                is_inactive: false,
            },
        ];
        let decision = evaluate(&candidates, 2, 5, 500, true, false);
        assert!(decision.to_resume.is_empty());
        assert_eq!(decision.to_pause.len(), 1);
        assert_eq!(decision.to_pause[0], make_hash(2));
    }

    #[test]
    fn evaluate_inactive_dont_count() {
        let candidates = vec![
            QueueCandidate {
                info_hash: make_hash(0),
                position: 0,
                category: QueueCategory::Downloading,
                is_active: true,
                is_inactive: true,
            },
            QueueCandidate {
                info_hash: make_hash(1),
                position: 1,
                category: QueueCategory::Downloading,
                is_active: true,
                is_inactive: true,
            },
            QueueCandidate {
                info_hash: make_hash(2),
                position: 2,
                category: QueueCategory::Downloading,
                is_active: true,
                is_inactive: false,
            },
        ];
        let decision = evaluate(&candidates, 2, 5, 500, true, false);
        assert!(decision.to_resume.is_empty());
        assert!(decision.to_pause.is_empty());
    }

    #[test]
    fn evaluate_respects_active_limit() {
        let candidates = vec![
            QueueCandidate {
                info_hash: make_hash(0),
                position: 0,
                category: QueueCategory::Downloading,
                is_active: true,
                is_inactive: false,
            },
            QueueCandidate {
                info_hash: make_hash(1),
                position: 1,
                category: QueueCategory::Downloading,
                is_active: true,
                is_inactive: false,
            },
            QueueCandidate {
                info_hash: make_hash(10),
                position: 0,
                category: QueueCategory::Seeding,
                is_active: true,
                is_inactive: false,
            },
            QueueCandidate {
                info_hash: make_hash(11),
                position: 1,
                category: QueueCategory::Seeding,
                is_active: true,
                is_inactive: false,
            },
            QueueCandidate {
                info_hash: make_hash(12),
                position: 2,
                category: QueueCategory::Seeding,
                is_active: true,
                is_inactive: false,
            },
        ];
        let decision = evaluate(&candidates, 3, 5, 4, true, false);
        assert_eq!(decision.to_pause.len(), 1);
        assert_eq!(decision.to_pause[0], make_hash(12));
    }

    #[test]
    fn evaluate_unlimited_limits() {
        let candidates = vec![
            QueueCandidate {
                info_hash: make_hash(0),
                position: 0,
                category: QueueCategory::Downloading,
                is_active: false,
                is_inactive: false,
            },
            QueueCandidate {
                info_hash: make_hash(1),
                position: 1,
                category: QueueCategory::Downloading,
                is_active: false,
                is_inactive: false,
            },
        ];
        let decision = evaluate(&candidates, -1, -1, -1, true, false);
        assert_eq!(decision.to_resume.len(), 2);
        assert!(decision.to_pause.is_empty());
    }
}
