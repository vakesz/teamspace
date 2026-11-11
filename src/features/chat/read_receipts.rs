//! Opt-in read receipts for the selected Microsoft 365 chat.
//!
//! A receipt is sent at most once per selection view, only when the user
//! turned the preference on, the optional `Chat.ReadWrite` capability is
//! offered, the window is focused, the provider still reports the chat
//! unread, and its history is `Ready` from an exact read. It has no
//! optimistic state, no reconciliation and no retry: a failed receipt is
//! simply not sent again for that view.

use super::prefs::ChatPrefs;
use super::{ChatState, ConversationSelection};
use crate::integrations::{Capability, ProviderResourceRef};

impl ChatState {
    pub(crate) const fn send_read_receipts(&self) -> bool {
        self.prefs.send_read_receipts
    }

    /// Whether the receipts affordance may be offered at all this session.
    pub(crate) fn read_receipts_offered(&self) -> bool {
        self.effective_remote_capabilities()
            .chat_mark_read
            .is_offered()
    }

    pub(crate) fn set_send_read_receipts(&mut self, enabled: bool) {
        if self.prefs.send_read_receipts != enabled {
            self.prefs.send_read_receipts = enabled;
            self.prefs_dirty = true;
        }
    }

    pub(crate) fn prefs(&self) -> ChatPrefs {
        self.prefs.clone()
    }

    /// Restores stored choices without marking them dirty.
    pub(crate) fn restore_prefs(&mut self, prefs: ChatPrefs) {
        self.prefs = prefs;
        self.prefs_dirty = false;
    }

    pub(crate) const fn is_prefs_dirty(&self) -> bool {
        self.prefs_dirty
    }

    pub(crate) fn mark_prefs_persisted(&mut self) {
        self.prefs_dirty = false;
    }

    /// One receipt target for this view, if every condition holds. Calling
    /// it again for the same view returns `None`.
    pub(crate) fn take_mark_read_request(&mut self, focused: bool) -> Option<ProviderResourceRef> {
        if !focused || !self.prefs.send_read_receipts || !self.read_receipts_offered() {
            return None;
        }
        let ConversationSelection::Remote(index) = self.selected else {
            return None;
        };
        let target = self.remote_chats.read_receipt_candidate(index)?;
        if self.read_receipt_sent.as_ref() == Some(&target) {
            return None;
        }
        self.read_receipt_sent = Some(target.clone());
        Some(target)
    }

    /// A `403` makes read receipts `NotGranted` for the rest of the session.
    pub(crate) fn mark_read_forbidden(&mut self) {
        self.remote_mark_read_forbidden = true;
    }

    pub(super) fn effective_mark_read_capability(&self, reported: Capability) -> Capability {
        if self.remote_mark_read_forbidden {
            Capability::NotGranted
        } else {
            reported
        }
    }
}
