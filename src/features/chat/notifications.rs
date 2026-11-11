//! In-app new-message notifications for Microsoft 365 chats.
//!
//! Detection lives in `remote/arrivals.rs`. This module applies the user's
//! choices (notifications on/off, chat names, per-chat mute) and builds the
//! one toast text. A notification never contains a message body, and by
//! default not even the chat name: only a count.

use super::ChatState;
use super::remote::NewActivityKind;
use crate::integrations::ProviderResourceRef;

const MAX_NAME_CHARS: usize = 60;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Announcement {
    name: String,
    mention: bool,
    count: usize,
}

impl ChatState {
    pub(crate) const fn notifications_enabled(&self) -> bool {
        self.prefs.notifications_enabled
    }

    pub(crate) fn set_notifications_enabled(&mut self, enabled: bool) {
        if self.prefs.notifications_enabled != enabled {
            self.prefs.notifications_enabled = enabled;
            self.prefs_dirty = true;
        }
    }

    pub(crate) const fn show_chat_name_in_notifications(&self) -> bool {
        self.prefs.show_chat_name
    }

    pub(crate) fn set_show_chat_name_in_notifications(&mut self, enabled: bool) {
        if self.prefs.show_chat_name != enabled {
            self.prefs.show_chat_name = enabled;
            self.prefs_dirty = true;
        }
    }

    /// Tells the feature whether a truthful system notifier exists, so the
    /// choice is offered only when it could work. Set once at startup.
    /// Whether the "All notification settings…" item was chosen since the
    /// last call.
    pub(crate) fn take_settings_request(&mut self) -> bool {
        std::mem::take(&mut self.settings_requested)
    }

    #[cfg(test)]
    pub(crate) const fn request_settings_for_test(&mut self) {
        self.settings_requested = true;
    }

    pub(crate) fn set_os_notifications_available(&mut self, available: bool) {
        self.os_notifications_available = available;
    }

    pub(crate) const fn os_notifications_available(&self) -> bool {
        self.os_notifications_available
    }

    pub(crate) const fn os_notifications_enabled(&self) -> bool {
        self.prefs.os_notifications_enabled
    }

    pub(crate) fn set_os_notifications_enabled(&mut self, enabled: bool) {
        if self.prefs.os_notifications_enabled != enabled {
            self.prefs.os_notifications_enabled = enabled;
            self.prefs_dirty = true;
        }
    }

    /// Whether an announcement should also go to the system: in-app
    /// notifications on, the system choice on, and a notifier exists.
    pub(crate) const fn os_notifications_wanted(&self) -> bool {
        self.prefs.notifications_enabled
            && self.prefs.os_notifications_enabled
            && self.os_notifications_available
    }

    /// The verified account the mute digests are keyed to. Session only.
    pub(crate) fn set_remote_account(&mut self, account_id: &str) {
        if self.remote_account_id.as_deref() != Some(account_id) {
            self.remote_account_id = Some(account_id.to_owned());
        }
    }

    fn mute_digest_for(&self, target: &ProviderResourceRef) -> Option<[u8; 16]> {
        let account = self.remote_account_id.as_deref()?;
        let secret = self.prefs.existing_secret()?;
        target.mute_digest(&secret, account)
    }

    /// Whether the remote chat at `index` is muted. Without a profile secret
    /// or a verified account nothing is muted.
    pub(crate) fn is_remote_chat_muted(&self, index: usize) -> bool {
        self.remote_chats
            .target_at(index)
            .and_then(|target| self.mute_digest_for(target))
            .is_some_and(|digest| self.prefs.is_muted(&digest))
    }

    /// Whether the mute control can be offered for this chat at all.
    pub(crate) fn can_mute_remote_chat(&self, index: usize) -> bool {
        self.remote_account_id.is_some() && self.remote_chats.target_at(index).is_some()
    }

    /// Mutes or unmutes one remote chat. The profile secret is created on
    /// the first mute; only a digest is ever stored.
    pub(crate) fn set_remote_chat_muted(&mut self, index: usize, muted: bool) {
        let Some(account) = self.remote_account_id.clone() else {
            return;
        };
        let Some(target) = self.remote_chats.target_at(index).cloned() else {
            return;
        };
        let secret = if muted {
            self.prefs.secret()
        } else {
            self.prefs.existing_secret()
        };
        let Some(digest) = secret.and_then(|secret| target.mute_digest(&secret, &account)) else {
            return;
        };
        if self.prefs.set_muted(digest, muted) {
            self.prefs_dirty = true;
        }
    }

    /// Drains arrivals and returns at most one toast text. It always drains,
    /// so a disabled or muted chat can never build up a backlog.
    pub(crate) fn take_notification_toast(&mut self) -> Option<String> {
        let activity = self.remote_chats.take_new_activity();
        if !self.prefs.notifications_enabled {
            return None;
        }
        let announcements: Vec<Announcement> = activity
            .into_iter()
            .filter(|item| !self.is_remote_chat_muted(item.chat_index_hint))
            .filter_map(|item| {
                let name = self.remote_chats.title_at(item.chat_index_hint)?;
                Some(Announcement {
                    name: name.chars().take(MAX_NAME_CHARS).collect(),
                    mention: item.kind == NewActivityKind::Mention,
                    count: item.count,
                })
            })
            .collect();
        compose_toast(&announcements, self.prefs.show_chat_name)
    }
}

fn compose_toast(announcements: &[Announcement], show_names: bool) -> Option<String> {
    let [first, rest @ ..] = announcements else {
        return None;
    };
    if rest.is_empty() {
        let name = show_names.then_some(first.name.as_str());
        return Some(match (first.mention, name, first.count) {
            (true, Some(name), _) => format!("You were mentioned in {name}"),
            (true, None, _) => "You were mentioned in a Microsoft 365 chat".into(),
            (false, Some(name), 1) => format!("New message in {name}"),
            (false, Some(name), count) => format!("{count} new messages in {name}"),
            (false, None, 1) => "New Microsoft 365 message".into(),
            (false, None, count) => format!("{count} new Microsoft 365 messages"),
        });
    }
    let total: usize = announcements.iter().map(|item| item.count).sum();
    let chats = announcements.len();
    Some(format!(
        "{total} new Microsoft 365 messages in {chats} chats"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, mention: bool, count: usize) -> Announcement {
        Announcement {
            name: name.into(),
            mention,
            count,
        }
    }

    #[test]
    fn default_text_is_generic_with_only_a_count() {
        assert_eq!(compose_toast(&[], false), None);
        let one = [item("Secret project", false, 1)];
        assert_eq!(
            compose_toast(&one, false).as_deref(),
            Some("New Microsoft 365 message")
        );
        let many = [item("Secret project", false, 3)];
        assert_eq!(
            compose_toast(&many, false).as_deref(),
            Some("3 new Microsoft 365 messages")
        );
        let mention = [item("Secret project", true, 1)];
        assert_eq!(
            compose_toast(&mention, false).as_deref(),
            Some("You were mentioned in a Microsoft 365 chat")
        );
    }

    #[test]
    fn names_appear_only_when_opted_in_and_for_a_single_chat() {
        let one = [item("Design sync", false, 2)];
        assert_eq!(
            compose_toast(&one, true).as_deref(),
            Some("2 new messages in Design sync")
        );
        let mention = [item("Design sync", true, 1)];
        assert_eq!(
            compose_toast(&mention, true).as_deref(),
            Some("You were mentioned in Design sync")
        );
        let two = [item("A", false, 1), item("B", false, 2)];
        for names in [false, true] {
            let text = compose_toast(&two, names).expect("text");
            assert_eq!(text, "3 new Microsoft 365 messages in 2 chats");
            assert!(!text.contains('A') || text.contains("Microsoft"));
        }
    }
}
