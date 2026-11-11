use crate::shared::reaction::ReactionKind;
use eframe::egui::Color32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Section {
    Activity,
    Chat,
    Teams,
    Calendar,
    Calls,
    Files,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChatTab {
    Chat,
    Shared,
    Notes,
    Recap,
    Storyline,
    Info,
}

pub(crate) struct Message {
    pub(crate) name: String,
    pub(crate) time: String,
    pub(crate) text: String,
    pub(crate) color: Color32,
    pub(crate) own: bool,
    pub(crate) attachment: Option<MessageAttachment>,
    pub(crate) day_divider_before: bool,
    pub(crate) reply_context: Option<MessageReply>,
    /// Session-only count of further quoted messages beyond `reply_context`.
    pub(crate) reply_context_extra: usize,
    /// Display names of the files a received Microsoft 365 message links,
    /// shown as read-only chips. Empty for every local message.
    pub(crate) file_chips: Vec<String>,
    pub(crate) reactions: Vec<MessageReaction>,
    /// Formatted runs of a received Microsoft 365 message, when its body had
    /// allow-listed formatting. `text` stays the plain fallback.
    pub(crate) rich: Option<Vec<crate::integrations::RichRun>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MessageReaction {
    pub(crate) kind: ReactionKind,
    pub(crate) count: usize,
    pub(crate) reacted_by_current_user: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MessageAttachment {
    LayoutMockup,
    Spreadsheet,
}

impl MessageAttachment {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::LayoutMockup => "layout-mockup.png",
            Self::Spreadsheet => "supply-costs.png",
        }
    }
}

pub(crate) struct MessageReply {
    pub(crate) name: String,
    pub(crate) time: String,
    pub(crate) text: String,
}
