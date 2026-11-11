use crate::shared::reaction::ReactionKind;
use serde::Deserialize;
use std::sync::OnceLock;

#[derive(Debug, Deserialize)]
pub(crate) struct Workspace {
    pub(crate) organization: String,
    pub(crate) current_user: String,
    pub(crate) chat: ChatWorkspace,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatWorkspace {
    pub(crate) day_label: String,
    pub(crate) favorites: Vec<ChatListItem>,
    pub(crate) conversations: Vec<ChatListItem>,
    pub(crate) messages: Vec<SeedMessage>,
}

impl ChatWorkspace {
    pub(crate) fn primary_conversation(&self) -> &ChatListItem {
        self.conversations
            .first()
            .expect("workspace requires at least one conversation")
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatListItem {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) preview: String,
    pub(crate) time: String,
    pub(crate) initials: String,
    pub(crate) color: [u8; 3],
    pub(crate) unread: bool,
    pub(crate) subtitle: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SeedMessage {
    pub(crate) name: String,
    pub(crate) time: String,
    pub(crate) text: String,
    pub(crate) own: bool,
    pub(crate) attachment: Option<String>,
    #[serde(default)]
    pub(crate) day_divider_before: bool,
    pub(crate) reply_context: Option<ReplyContext>,
    #[serde(default)]
    pub(crate) reactions: Vec<SeedMessageReaction>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SeedMessageReaction {
    pub(crate) kind: String,
    pub(crate) count: usize,
    #[serde(default)]
    pub(crate) reacted_by_current_user: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ReplyContext {
    pub(crate) name: String,
    pub(crate) time: String,
}

pub(crate) fn workspace() -> &'static Workspace {
    static WORKSPACE: OnceLock<Workspace> = OnceLock::new();
    WORKSPACE.get_or_init(|| {
        let workspace: Workspace = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/data/workspace.json"
        )))
        .expect("workspace seed must be valid JSON");
        workspace
            .validate()
            .expect("workspace seed must satisfy required chat invariants");
        workspace
    })
}

impl Workspace {
    // Kept as `String`: this only checks the embedded seed at startup (the
    // result is `expect`ed and never reaches the UI), so no caller branches on it.
    fn validate(&self) -> Result<(), String> {
        if self.organization.trim().is_empty() || self.current_user.trim().is_empty() {
            return Err("organization and current user are required".into());
        }
        let chat = &self.chat;
        if chat.conversations.is_empty() {
            return Err("at least one conversation is required".into());
        }
        if chat.messages.is_empty() {
            return Err("at least one message is required".into());
        }
        if chat
            .conversations
            .iter()
            .chain(chat.favorites.iter())
            .any(|item| {
                item.id.trim().is_empty()
                    || item.name.trim().is_empty()
                    || item.preview.trim().is_empty()
                    || item.time.trim().is_empty()
                    || item.initials.trim().is_empty()
            })
        {
            return Err("chat list items require an id, name, preview, time, and initials".into());
        }
        let mut ids = std::collections::HashSet::new();
        if chat
            .conversations
            .iter()
            .chain(chat.favorites.iter())
            .any(|item| !ids.insert(item.id.as_str()))
        {
            return Err("chat list item ids must be globally unique".into());
        }
        if chat.messages.iter().any(|message| {
            message.name.trim().is_empty()
                || message.time.trim().is_empty()
                || message.text.trim().is_empty()
                || message.reactions.iter().any(|reaction| {
                    reaction.count == 0 || ReactionKind::from_seed(&reaction.kind).is_none()
                })
        }) {
            return Err("messages and reactions require valid content".into());
        }
        if !chat
            .messages
            .iter()
            .any(|message| message.attachment.as_deref() == Some("layout-mockup"))
            || !chat
                .messages
                .iter()
                .any(|message| message.attachment.as_deref() == Some("spreadsheet"))
            || !chat
                .messages
                .iter()
                .any(|message| message.reply_context.is_some())
        {
            return Err("the chat preview requires its two attachments and reply context".into());
        }
        if chat
            .messages
            .iter()
            .filter(|message| message.day_divider_before)
            .count()
            > 1
        {
            return Err("the chat preview supports at most one current-day divider".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::ui::avatar::initials;

    #[test]
    fn list_item_initials_match_their_names() {
        let chat = &workspace().chat;
        for item in chat.favorites.iter().chain(chat.conversations.iter()) {
            assert_eq!(
                initials(&item.name),
                item.initials,
                "{} needs matching initials",
                item.name
            );
        }
    }

    use super::workspace;

    #[test]
    fn workspace_seed_has_the_required_chat_preview_data() {
        let chat = &workspace().chat;

        assert!(!chat.favorites.is_empty());
        assert!(!chat.conversations.is_empty());
        assert!(
            chat.messages
                .iter()
                .any(|message| message.attachment.as_deref() == Some("layout-mockup"))
        );
        assert!(
            chat.messages
                .iter()
                .any(|message| message.attachment.as_deref() == Some("spreadsheet"))
        );
        assert!(
            chat.messages
                .iter()
                .any(|message| !message.reactions.is_empty())
        );
        assert!(
            chat.messages
                .last()
                .is_some_and(|message| message.day_divider_before)
        );
    }

    #[test]
    fn workspace_seed_keeps_the_reference_chat_within_the_initial_view() {
        let messages = &workspace().chat.messages;

        assert_eq!(messages.len(), 4);
        let last = messages
            .last()
            .expect("the reference chat must end with the shared spreadsheet");
        assert!(last.own);
        assert_eq!(last.attachment.as_deref(), Some("spreadsheet"));
    }
}
