use super::persistence::{
    StoredAttachment, StoredConversation, StoredMessage, StoredReaction, StoredReactionKind,
    StoredReply,
};
use crate::data::workspace::workspace;
use crate::shared::model::{Message, MessageAttachment, MessageReaction, MessageReply};
use crate::shared::reaction::ReactionKind;
use crate::ui::theme;
use eframe::egui::Color32;

use super::{ComposerState, MessageTimeline};

pub(super) fn seeded_messages() -> Vec<Message> {
    let chat = &workspace().chat;
    chat.messages
        .iter()
        .map(|message| Message {
            name: message.name.clone(),
            time: message.time.clone(),
            text: message.text.clone(),
            color: if message.own {
                theme::brand()
            } else {
                color(chat.primary_conversation().color)
            },
            own: message.own,
            attachment: match message.attachment.as_deref() {
                Some("layout-mockup") => Some(MessageAttachment::LayoutMockup),
                Some("spreadsheet") => Some(MessageAttachment::Spreadsheet),
                _ => None,
            },
            day_divider_before: message.day_divider_before,
            reply_context: message.reply_context.as_ref().map(|reply| MessageReply {
                name: reply.name.clone(),
                time: reply.time.clone(),
                text: "attached the draft floor plan for the reading nook, shelf heights can be…"
                    .into(),
            }),
            reply_context_extra: 0,
            file_chips: Vec::new(),
            rich: None,
            reactions: message
                .reactions
                .iter()
                .filter_map(|reaction| {
                    ReactionKind::from_seed(&reaction.kind).map(|kind| MessageReaction {
                        kind,
                        count: reaction.count,
                        reacted_by_current_user: reaction.reacted_by_current_user,
                    })
                })
                .collect(),
        })
        .collect()
}

pub(super) fn stored_conversation(
    messages: Option<&MessageTimeline>,
    notes: &str,
    composer: Option<&ComposerState>,
    read: bool,
) -> StoredConversation {
    StoredConversation {
        messages: messages.map_or_else(Vec::new, MessageTimeline::stored_messages),
        notes: notes.into(),
        draft: composer.map_or_else(String::new, |composer| composer.draft.clone()),
        pending_attachment: composer
            .and_then(|composer| composer.pending_attachment)
            .map(attachment_to_stored),
        read,
    }
}

pub(super) fn message_to_stored(message: &Message, local_id: Option<u64>) -> StoredMessage {
    StoredMessage {
        local_id,
        name: message.name.clone(),
        time: message.time.clone(),
        text: message.text.clone(),
        color_rgb: rgb(message.color),
        own: message.own,
        attachment: message.attachment.map(attachment_to_stored),
        day_divider_before: message.day_divider_before,
        reply_context: message.reply_context.as_ref().map(|reply| StoredReply {
            name: reply.name.clone(),
            time: reply.time.clone(),
            text: reply.text.clone(),
        }),
        reactions: message
            .reactions
            .iter()
            .map(|reaction| StoredReaction {
                kind: reaction_to_stored(reaction.kind),
                count: reaction.count,
                reacted_by_current_user: reaction.reacted_by_current_user,
            })
            .collect(),
    }
}

pub(super) fn message_from_stored(message: StoredMessage) -> Message {
    Message {
        name: message.name,
        time: message.time,
        text: message.text,
        color: color(message.color_rgb),
        own: message.own,
        attachment: message.attachment.map(attachment_from_stored),
        day_divider_before: message.day_divider_before,
        reply_context: message.reply_context.map(|reply| MessageReply {
            name: reply.name,
            time: reply.time,
            text: reply.text,
        }),
        reply_context_extra: 0,
        file_chips: Vec::new(),
        rich: None,
        reactions: message
            .reactions
            .into_iter()
            .map(|reaction| MessageReaction {
                kind: reaction_from_stored(reaction.kind),
                count: reaction.count,
                reacted_by_current_user: reaction.reacted_by_current_user,
            })
            .collect(),
    }
}

const fn attachment_to_stored(attachment: MessageAttachment) -> StoredAttachment {
    match attachment {
        MessageAttachment::LayoutMockup => StoredAttachment::LayoutMockup,
        MessageAttachment::Spreadsheet => StoredAttachment::Spreadsheet,
    }
}

pub(super) const fn attachment_from_stored(attachment: StoredAttachment) -> MessageAttachment {
    match attachment {
        StoredAttachment::LayoutMockup => MessageAttachment::LayoutMockup,
        StoredAttachment::Spreadsheet => MessageAttachment::Spreadsheet,
    }
}

const fn reaction_to_stored(reaction: ReactionKind) -> StoredReactionKind {
    match reaction {
        ReactionKind::Like => StoredReactionKind::Like,
        ReactionKind::Heart => StoredReactionKind::Heart,
        ReactionKind::Laugh => StoredReactionKind::Laugh,
        ReactionKind::Surprised => StoredReactionKind::Surprised,
        ReactionKind::Sad => StoredReactionKind::Sad,
        ReactionKind::Angry => StoredReactionKind::Angry,
    }
}

const fn reaction_from_stored(reaction: StoredReactionKind) -> ReactionKind {
    match reaction {
        StoredReactionKind::Like => ReactionKind::Like,
        StoredReactionKind::Heart => ReactionKind::Heart,
        StoredReactionKind::Laugh => ReactionKind::Laugh,
        StoredReactionKind::Surprised => ReactionKind::Surprised,
        StoredReactionKind::Sad => ReactionKind::Sad,
        StoredReactionKind::Angry => ReactionKind::Angry,
    }
}

pub(super) fn color(rgb: [u8; 3]) -> Color32 {
    Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}

pub(super) const fn rgb(color: Color32) -> [u8; 3] {
    [color.r(), color.g(), color.b()]
}
