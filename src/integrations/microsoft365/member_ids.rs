//! The adapter's one per-chat store of member user IDs.
//!
//! It serves two readers that must never see each other's data on the seam:
//! the presence query (positional user IDs) and @mention authoring (opaque
//! [`MentionHandle`]s resolved back to a user ID and display name inside the
//! adapter). Both come from the same explicit People roster read, so there is
//! no second ID store. Bounded to 100 rows per chat and 16 chats (oldest chat
//! dropped), cleared on reauthentication, and never logged or sent across the
//! seam: `Debug` shows counts only.

use crate::integrations::transport::{MAX_REMOTE_CHAT_MEMBERS, MentionHandle};
use std::collections::{BTreeMap, BTreeSet};

const MAX_REMEMBERED_CHATS: usize = 16;

/// One roster row, positional with the rows the feature was shown.
struct MemberRow {
    /// `None` for a row without a validated Entra user ID.
    user_id: Option<String>,
    /// The sanitized display name that was shown (and is mentioned by).
    display: String,
    /// Present only for a row that can be mentioned: it has a user ID and a
    /// non-empty display name.
    handle: Option<MentionHandle>,
}

#[derive(Default)]
pub(super) struct MemberIdMap {
    chats: BTreeMap<String, Vec<MemberRow>>,
    order: Vec<String>,
}

impl MemberIdMap {
    /// Remembers one chat's roster: `(user ID, display name)` per shown row.
    /// A row keeps the handle it already had in this chat, so a handle the
    /// composer holds survives a roster re-read. Returns the handle of each
    /// row, positional with `rows`.
    pub(super) fn remember_roster(
        &mut self,
        chat_id: &str,
        mut rows: Vec<(Option<String>, String)>,
    ) -> Vec<Option<MentionHandle>> {
        rows.truncate(MAX_REMOTE_CHAT_MEMBERS);
        let previous = self.chats.remove(chat_id).unwrap_or_default();
        self.order.retain(|known| known != chat_id);
        if !rows.iter().any(|(id, _)| id.is_some()) {
            return vec![None; rows.len()];
        }
        let mut in_use: BTreeSet<u64> = self
            .chats
            .values()
            .flatten()
            .filter_map(|row| row.handle)
            .map(MentionHandle::raw)
            .collect();
        let mut stored = Vec::with_capacity(rows.len());
        for (user_id, display) in rows {
            let handle = match (&user_id, display.is_empty()) {
                (Some(id), false) => previous
                    .iter()
                    .find(|row| row.user_id.as_deref() == Some(id.as_str()))
                    .and_then(|row| row.handle)
                    .or_else(|| fresh_handle(&mut in_use)),
                _ => None,
            };
            stored.push(MemberRow {
                user_id,
                display,
                handle,
            });
        }
        while self.order.len() >= MAX_REMEMBERED_CHATS {
            let oldest = self.order.remove(0);
            self.chats.remove(&oldest);
        }
        let handles = stored.iter().map(|row| row.handle).collect();
        self.order.push(chat_id.to_owned());
        self.chats.insert(chat_id.to_owned(), stored);
        handles
    }

    /// The user IDs of the remembered roster, positional with its rows.
    pub(super) fn ids_for(&self, chat_id: &str) -> Option<Vec<Option<String>>> {
        self.chats
            .get(chat_id)
            .map(|rows| rows.iter().map(|row| row.user_id.clone()).collect())
    }

    /// The user ID and display name a handle was issued for, but only within
    /// the chat it was issued for: a handle from another chat resolves to
    /// nothing.
    pub(super) fn resolve(&self, chat_id: &str, handle: MentionHandle) -> Option<(&str, &str)> {
        self.chats
            .get(chat_id)?
            .iter()
            .find(|row| row.handle == Some(handle))
            .and_then(|row| Some((row.user_id.as_deref()?, row.display.as_str())))
    }

    /// The handle of the member with this user ID in this chat, if the
    /// roster has been read and that member can be mentioned.
    pub(super) fn handle_for_user(&self, chat_id: &str, user_id: &str) -> Option<MentionHandle> {
        self.chats
            .get(chat_id)?
            .iter()
            .find(|row| row.user_id.as_deref() == Some(user_id))
            .and_then(|row| row.handle)
    }

    pub(super) fn clear(&mut self) {
        self.chats.clear();
        self.order.clear();
    }

    #[cfg(test)]
    pub(super) fn len_for_test(&self) -> usize {
        self.chats.len()
    }
}

/// A new random handle not used by any remembered row, or `None` when the
/// system has no entropy (the member then simply cannot be mentioned).
fn fresh_handle(in_use: &mut BTreeSet<u64>) -> Option<MentionHandle> {
    for _ in 0..4 {
        let mut bytes = [0_u8; 8];
        getrandom::fill(&mut bytes).ok()?;
        let raw = u64::from_le_bytes(bytes);
        if in_use.insert(raw) {
            return Some(MentionHandle::from_raw(raw));
        }
    }
    None
}

impl std::fmt::Debug for MemberIdMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemberIdMap")
            .field("chats", &self.chats.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(count: usize) -> Vec<(Option<String>, String)> {
        (0..count)
            .map(|index| (Some(format!("user-{index}")), format!("Person {index}")))
            .collect()
    }

    #[test]
    fn bounds_100_per_chat_16_chats() {
        let mut map = MemberIdMap::default();
        for chat in 0..20 {
            let handles = map.remember_roster(&format!("c{chat}"), rows(150));
            assert_eq!(handles.len(), 100);
        }
        assert_eq!(map.len_for_test(), 16);
        assert!(map.ids_for("c0").is_none());
        assert_eq!(map.ids_for("c19").unwrap().len(), 100);
        map.clear();
        assert_eq!(map.len_for_test(), 0);
    }

    #[test]
    fn handles_are_stable_across_roster_rereads_and_unique() {
        let mut map = MemberIdMap::default();
        let first = map.remember_roster("chat", rows(3));
        assert!(first.iter().all(Option::is_some));
        let distinct: BTreeSet<_> = first.iter().flatten().map(|h| h.raw()).collect();
        assert_eq!(distinct.len(), 3);
        // Re-read with one member gone and a new one added.
        let second = map.remember_roster(
            "chat",
            vec![
                (Some("user-2".into()), "Person 2".into()),
                (Some("user-0".into()), "Person 0".into()),
                (Some("new".into()), "New".into()),
            ],
        );
        assert_eq!(second[0], first[2]);
        assert_eq!(second[1], first[0]);
        assert!(second[2].is_some() && second[2] != first[1]);
    }

    #[test]
    fn only_members_with_an_id_and_a_name_get_a_handle() {
        let mut map = MemberIdMap::default();
        let handles = map.remember_roster(
            "chat",
            vec![
                (None, "Guest".into()),
                (Some("u1".into()), String::new()),
                (Some("u2".into()), "Real".into()),
            ],
        );
        assert_eq!(handles[0], None);
        assert_eq!(handles[1], None);
        assert!(handles[2].is_some());
        // IDs stay positional for presence, including the unnamed row.
        assert_eq!(
            map.ids_for("chat").unwrap(),
            vec![None, Some("u1".into()), Some("u2".into())]
        );
        // A roster with no user ID at all is not remembered.
        let none = map.remember_roster("other", vec![(None, "A".into())]);
        assert_eq!(none, vec![None]);
        assert!(map.ids_for("other").is_none());
    }

    #[test]
    fn a_handle_resolves_only_in_its_own_chat() {
        let mut map = MemberIdMap::default();
        let a = map.remember_roster("a", rows(1))[0].unwrap();
        let b = map.remember_roster("b", rows(1))[0].unwrap();
        assert_eq!(map.resolve("a", a), Some(("user-0", "Person 0")));
        assert_eq!(map.resolve("b", a), None);
        assert_eq!(map.resolve("a", b), None);
        assert_eq!(map.handle_for_user("a", "user-0"), Some(a));
        assert_eq!(map.handle_for_user("a", "nobody"), None);
        assert_eq!(map.handle_for_user("missing", "user-0"), None);
    }

    #[test]
    fn debug_shows_no_ids_or_handles() {
        let mut map = MemberIdMap::default();
        let handle = map.remember_roster("chat", rows(1))[0].unwrap();
        let debug = format!("{map:?} {handle:?}");
        assert!(!debug.contains("user-0") && !debug.contains("Person 0"));
        assert!(!debug.contains(&handle.raw().to_string()));
    }
}
