//! `m.direct` repair: a pure function over the account-data map, so it can be
//! tested without Matrix.
//!
//! 2026-09-29 incident: the daemon accepted two GROUP-room invites from Tim and
//! recorded both as his DMs, so a `send_message(to: "tim")` could have landed
//! in a group room. On every connect the daemon now removes, from its own
//! `m.direct`, every room that is a configured `[[rooms]]` entry or a joined
//! room with more than two joined members.

use std::collections::BTreeMap;

/// One `(user, room)` pair removed from `m.direct`.
pub type Removed<K, R> = (K, R);

/// The repaired map and the pairs removed from it.
pub type Repaired<K, R> = (BTreeMap<K, Vec<R>>, Vec<Removed<K, R>>);

/// Remove every room for which `is_excluded(room_id)` holds from every user's
/// list; users left with no rooms are dropped from the map. Returns `None`
/// when nothing changed (so the caller skips the server write), otherwise the
/// repaired map and the removed pairs (in map order).
///
/// Generic over the key and room types so the daemon can pass ruma's
/// `OwnedDirectUserIdentifier` / `OwnedRoomId` and tests plain strings.
pub fn repair_direct<K, R>(
    map: &BTreeMap<K, Vec<R>>,
    is_excluded: impl Fn(&str) -> bool,
) -> Option<Repaired<K, R>>
where
    K: Ord + Clone,
    R: Clone + AsRef<str>,
{
    let mut removed = Vec::new();
    let mut out = BTreeMap::new();
    for (user, rooms) in map {
        let mut kept = Vec::with_capacity(rooms.len());
        for r in rooms {
            if is_excluded(r.as_ref()) {
                removed.push((user.clone(), r.clone()));
            } else {
                kept.push(r.clone());
            }
        }
        if !kept.is_empty() {
            out.insert(user.clone(), kept);
        }
    }
    if removed.is_empty() {
        None
    } else {
        Some((out, removed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(u, rs)| (u.to_string(), rs.iter().map(|r| r.to_string()).collect()))
            .collect()
    }

    #[test]
    fn removes_listed_and_group_rooms_only() {
        let before = m(&[
            ("@tim:x", &["!dm:x", "!daily:x", "!internal:x"]),
            ("@kenn:x", &["!kdm:x"]),
            ("@gone:x", &["!daily:x"]),
        ]);
        let excluded = ["!daily:x", "!internal:x"];
        let (after, removed) = repair_direct(&before, |r| excluded.contains(&r)).unwrap();
        assert_eq!(
            after,
            m(&[("@tim:x", &["!dm:x"]), ("@kenn:x", &["!kdm:x"])])
        );
        assert_eq!(
            removed,
            vec![
                ("@gone:x".to_string(), "!daily:x".to_string()),
                ("@tim:x".to_string(), "!daily:x".to_string()),
                ("@tim:x".to_string(), "!internal:x".to_string()),
            ]
        );
    }

    #[test]
    fn clean_map_is_untouched() {
        let before = m(&[("@tim:x", &["!dm:x"])]);
        assert!(repair_direct(&before, |r| r == "!other:x").is_none());
        assert!(repair_direct(&BTreeMap::<String, Vec<String>>::new(), |_| true).is_none());
    }

    #[test]
    fn keeps_order_and_duplicates_of_kept_rooms() {
        // Dedupe is the connector's job (mark_dm); repair only removes.
        let before = m(&[("@tim:x", &["!b:x", "!g:x", "!a:x", "!b:x"])]);
        let (after, removed) = repair_direct(&before, |r| r == "!g:x").unwrap();
        assert_eq!(after["@tim:x"], vec!["!b:x", "!a:x", "!b:x"]);
        assert_eq!(removed.len(), 1);
    }
}
