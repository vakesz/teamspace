//! Sending @mentions: the roster the picker offers, how a draft becomes a
//! body, and how an ambiguous mention send is confirmed or restored.

use super::super::remote::RemoteSendError;
use super::super::{ChatAction, ChatState, RemoteReconciliationResolution};
use super::{remote_projection, remote_quote_source_history};
use crate::integrations::{
    ComposedBody, DraftMention, MentionHandle, RemoteChatMember, RemoteChatMembersProjection,
    RemoteChatMessage, RemoteChatMessagesProjection, WorkspaceIntentId,
};

const CHAT: &str = "19:mention-chat@thread.v2";

const ADA: u64 = 11;
const BOB: u64 = 12;

fn handle(raw: u64) -> MentionHandle {
    MentionHandle::for_test(raw)
}

fn picked(name: &str, raw: u64) -> DraftMention {
    DraftMention {
        handle: handle(raw),
        display: name.into(),
    }
}

/// A selected chat whose People roster has been read (and closed again).
fn chat_with_roster() -> ChatState {
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_projection(CHAT, "Preview"));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    chat.toggle_remote_chat_members(0);
    let _ = chat.take_remote_chat_members_request();
    assert!(
        chat.apply_remote_chat_members_projection(RemoteChatMembersProjection::for_test(
            CHAT,
            vec![
                RemoteChatMember::for_test("Ada", None).with_mention_for_test(handle(ADA)),
                RemoteChatMember::for_test("Bob", None).with_mention_for_test(handle(BOB)),
                // A guest with no handle cannot be mentioned.
                RemoteChatMember::for_test("Guest", None),
            ],
        ))
    );
    chat.toggle_remote_chat_members(0);
    assert!(chat.remote_chats.members_panel(0).is_none());
    chat
}

fn conversation(chat: &mut ChatState) -> &mut super::super::remote::RemoteConversation {
    chat.remote_chats.get_mut(0).expect("remote chat")
}

/// Types `draft`, picks `people`, queues the send, and returns the body.
fn queue(
    chat: &mut ChatState,
    draft: &str,
    people: &[DraftMention],
) -> (super::super::RemoteSubmissionId, ComposedBody) {
    let row = conversation(chat);
    *row.draft_mut() = draft.into();
    for member in people {
        assert!(row.add_draft_mention(member.clone()));
    }
    chat.queue_remote_message(0).expect("send queues");
    match chat.take_actions().into_iter().next().expect("one action") {
        ChatAction::SendRemoteMessage {
            submission, body, ..
        } => (submission, body),
        other => panic!("expected a remote send, got {other:?}"),
    }
}

fn own_row(text: &str, handles: Vec<MentionHandle>) -> RemoteChatMessage {
    RemoteChatMessage::for_test("You", "2026-11-06T10:31:00Z", text, true, false)
        .with_mentioned_handles_for_test(handles)
}

fn history(rows: Vec<RemoteChatMessage>) -> RemoteChatMessagesProjection {
    RemoteChatMessagesProjection::for_test(CHAT, rows)
}

#[test]
fn the_roster_outlives_the_people_panel_and_offers_only_mentionable_members() {
    let mut chat = chat_with_roster();
    let roster: Vec<_> = conversation(&mut chat)
        .mention_roster()
        .iter()
        .map(|member| (member.display.clone(), member.handle))
        .collect();
    assert_eq!(
        roster,
        [
            ("Ada".to_owned(), handle(ADA)),
            ("Bob".to_owned(), handle(BOB))
        ]
    );
}

#[test]
fn a_denied_roster_clears_the_offer() {
    let mut chat = chat_with_roster();
    chat.toggle_remote_chat_members(0);
    let _ = chat.take_remote_chat_members_request();
    assert!(chat.apply_remote_chat_members_projection(
        RemoteChatMembersProjection::access_denied_for_test(CHAT)
    ));
    assert!(conversation(&mut chat).mention_roster().is_empty());
}

#[test]
fn a_send_with_a_picked_mention_carries_its_handle() {
    let mut chat = chat_with_roster();
    let (_, body) = queue(
        &mut chat,
        "hi @Ada, and @Bob **too**",
        &[picked("Ada", ADA), picked("Bob", BOB)],
    );
    assert_eq!(body.mentions(), vec![handle(ADA), handle(BOB)]);
    assert_eq!(body.plain_projection(), "hi Ada, and Bob too");
    assert!(!body.is_plain());
    // Sending clears the picks.
    assert!(conversation(&mut chat).add_draft_mention(picked("Ada", ADA)));
}

#[test]
fn a_pick_whose_text_was_deleted_is_not_sent() {
    let mut chat = chat_with_roster();
    let (_, body) = queue(&mut chat, "never mind", &[picked("Ada", ADA)]);
    assert!(body.mentions().is_empty());
    assert!(body.is_plain());
    let mut chat = chat_with_roster();
    let (_, body) = queue(&mut chat, "@Ad broken", &[picked("Ada", ADA)]);
    assert!(body.mentions().is_empty());
}

#[test]
fn a_member_is_held_once_and_a_draft_holds_at_most_twenty() {
    let mut chat = chat_with_roster();
    let row = conversation(&mut chat);
    assert!(row.add_draft_mention(picked("Ada", ADA)));
    assert!(row.add_draft_mention(picked("Ada", ADA)));
    for raw in 100..119 {
        assert!(row.add_draft_mention(picked(&format!("P{raw}"), raw)));
    }
    assert!(!row.add_draft_mention(picked("One too many", 999)));
    // The one already held still counts as accepted.
    assert!(row.add_draft_mention(picked("Ada", ADA)));
}

/// Runs an uncertain mention send against `row` and returns how it settled.
fn settle(
    draft: &str,
    people: &[DraftMention],
    row: RemoteChatMessage,
) -> (RemoteReconciliationResolution, String) {
    let mut chat = chat_with_roster();
    let (submission, _) = queue(&mut chat, draft, people);
    let intent = WorkspaceIntentId::for_test(41);
    assert!(chat.bind_remote_submission(submission, intent));
    assert!(chat.remote_send_delivery_uncertain(intent));
    assert!(chat.apply_remote_chat_messages_projection(history(vec![row])));
    let resolution = chat
        .complete_remote_send_reconciliation(intent)
        .expect("reconciliation completes");
    let restored = chat.remote_chats.get(0).expect("row").draft().to_owned();
    (resolution, restored)
}

#[test]
fn mention_send_reconciliation_matrix() {
    let ada = [picked("Ada", ADA)];
    let both = [picked("Ada", ADA), picked("Bob", BOB)];
    let text = "hi Ada there";
    let confirmed = RemoteReconciliationResolution::Confirmed;
    let restored = RemoteReconciliationResolution::DraftRestored;

    // The exact text and the exact mentions confirm.
    assert_eq!(
        settle("hi @Ada there", &ada, own_row(text, vec![handle(ADA)])).0,
        confirmed
    );
    // Order is ignored, repeats are counted.
    assert_eq!(
        settle(
            "@Bob and @Ada",
            &both,
            own_row("Bob and Ada", vec![handle(ADA), handle(BOB)])
        )
        .0,
        confirmed
    );
    assert_eq!(
        settle(
            "@Ada @Ada",
            &ada,
            own_row("Ada Ada", vec![handle(ADA), handle(ADA)])
        )
        .0,
        confirmed
    );
    assert_eq!(
        settle("@Ada @Ada", &ada, own_row("Ada Ada", vec![handle(ADA)])).0,
        restored
    );
    // Same words but nobody mentioned, someone else, or one too many.
    assert_eq!(
        settle("hi @Ada there", &ada, own_row(text, vec![])).0,
        restored
    );
    assert_eq!(
        settle("hi @Ada there", &ada, own_row(text, vec![handle(BOB)])).0,
        restored
    );
    assert_eq!(
        settle(
            "hi @Ada there",
            &ada,
            own_row(text, vec![handle(ADA), handle(BOB)])
        )
        .0,
        restored
    );
    // A plain send never matches a row that mentions someone.
    assert_eq!(
        settle("hi Ada there", &[], own_row(text, vec![handle(ADA)])).0,
        restored
    );
    // Not an own row, or different words.
    assert_eq!(
        settle(
            "hi @Ada there",
            &ada,
            RemoteChatMessage::for_test("Eve", "2026-11-06T10:31:00Z", text, false, false)
                .with_mentioned_handles_for_test(vec![handle(ADA)])
        )
        .0,
        restored
    );
    assert_eq!(
        settle("hi @Ada there", &ada, own_row("bye Ada", vec![handle(ADA)])).0,
        restored
    );
}

#[test]
fn an_unconfirmed_mention_send_restores_the_draft_and_its_picks() {
    let ada = [picked("Ada", ADA)];
    let (resolution, restored) = settle("hi @Ada there", &ada, own_row("other", vec![]));
    assert_eq!(resolution, RemoteReconciliationResolution::DraftRestored);
    assert_eq!(restored, "hi @Ada there");

    // The restored draft still sends the mention without picking again.
    let mut chat = chat_with_roster();
    let (submission, _) = queue(&mut chat, "hi @Ada there", &ada);
    let intent = WorkspaceIntentId::for_test(42);
    assert!(chat.bind_remote_submission(submission, intent));
    assert!(chat.remote_send_failed(intent));
    assert_eq!(
        chat.remote_chats.get(0).expect("row").draft(),
        "hi @Ada there"
    );
    chat.queue_remote_message(0).expect("re-send queues");
    match chat.take_actions().into_iter().next().expect("one action") {
        ChatAction::SendRemoteMessage { body, .. } => {
            assert_eq!(body.mentions(), vec![handle(ADA)]);
        }
        other => panic!("expected a remote send, got {other:?}"),
    }
}

#[test]
fn mentions_are_offered_only_for_an_ordinary_unlocked_send() {
    let mut chat = chat_with_roster();
    assert!(conversation(&mut chat).can_mention());

    // A quote reply is plain text.
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(remote_quote_source_history(
            CHAT,
            "src",
            "Please review."
        ))
    );
    chat.toggle_remote_quote_source(0, 0).expect("quote mode");
    assert!(!conversation(&mut chat).can_mention());
    // ...and its body ignores any pick.
    *conversation(&mut chat).draft_mut() = "@Ada ok".into();
    assert!(conversation(&mut chat).add_draft_mention(picked("Ada", ADA)));
    chat.queue_remote_message(0).expect("quote reply queues");
    match chat.take_actions().into_iter().next().expect("one action") {
        ChatAction::ReplyToRemoteChatMessageWithQuote { body, .. } => {
            assert_eq!(body, "@Ada ok");
        }
        other => panic!("expected a quote reply, got {other:?}"),
    }
    // A locked composer (send in flight) offers nothing.
    assert!(!conversation(&mut chat).can_mention());

    // An edit is not offered mentions either.
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_projection(CHAT, "Preview"));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(chat.apply_remote_chat_messages_projection(history(vec![
        RemoteChatMessage::for_test_own("m1", "hello world", true, false)
    ])));
    chat.begin_remote_edit(0, 0).expect("edit mode");
    assert!(!conversation(&mut chat).can_mention());
    assert_eq!(
        chat.begin_remote_edit(0, 0),
        Err(RemoteSendError::Pending),
        "edit mode is already active"
    );
}

/// The picker end to end in a real egui pass: typing `@` opens the list,
/// Enter picks instead of sending, and the pick is remembered.
mod picker_ui {
    use super::*;
    use crate::features::chat::mention_picker;
    use crate::features::chat::remote_composer::remote_composer_id;
    use eframe::egui;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    fn ensure_theme(ui: &egui::Ui) -> bool {
        let key = egui::Id::new("mention_picker_test_theme");
        if ui.ctx().data(|data| data.get_temp::<bool>(key)).is_some() {
            return true;
        }
        crate::ui::theme::apply(ui.ctx());
        ui.ctx().data_mut(|data| data.insert_temp(key, true));
        ui.ctx().request_discard("install theme fonts");
        false
    }

    fn harness(chat: ChatState) -> Harness<'static, ChatState> {
        Harness::new_ui_state(
            |ui, chat: &mut ChatState| {
                if !ensure_theme(ui) {
                    return;
                }
                let conversation = chat.remote_chats.get_mut(0).expect("remote chat");
                let picker = mention_picker::begin(ui.ctx(), conversation);
                let input = ui.add(
                    egui::TextEdit::singleline(conversation.draft_mut()).id(remote_composer_id()),
                );
                if let Some(frame) = &picker {
                    mention_picker::show(ui, conversation, &input, frame, false);
                }
            },
            chat,
        )
    }

    #[test]
    fn typing_at_lists_matches_and_enter_picks_one() {
        let mut harness = harness(chat_with_roster());
        harness.run();
        harness
            .get_by_role(egui::accesskit::Role::TextInput)
            .focus();
        harness.run();
        harness
            .get_by_role(egui::accesskit::Role::TextInput)
            .type_text("hi @A");
        harness.run();
        // The matching member is offered; a non-match and a guest are not.
        harness.get_by_label("Ada");
        assert!(harness.query_by_label("Bob").is_none());
        assert!(harness.query_by_label("Guest").is_none());

        harness.key_press(egui::Key::Enter);
        harness.run();
        let state = harness.state_mut();
        let row = state.remote_chats.get_mut(0).expect("row");
        assert_eq!(row.draft_mut().as_str(), "hi @Ada ");
        // Nothing was sent by that Enter.
        assert!(state.take_actions().is_empty());
        // The pick was remembered, so sending carries the mention.
        state.queue_remote_message(0).expect("send queues");
        match state.take_actions().into_iter().next().expect("one action") {
            ChatAction::SendRemoteMessage { body, .. } => {
                assert_eq!(body.mentions(), vec![handle(ADA)]);
                assert_eq!(body.plain_projection(), "hi Ada");
            }
            other => panic!("expected a remote send, got {other:?}"),
        }
    }

    #[test]
    fn escape_dismisses_and_a_roster_less_chat_shows_the_hint() {
        let mut harness = harness(chat_with_roster());
        harness.run();
        harness
            .get_by_role(egui::accesskit::Role::TextInput)
            .focus();
        harness.run();
        harness
            .get_by_role(egui::accesskit::Role::TextInput)
            .type_text("@");
        harness.run();
        harness.get_by_label("Ada");
        harness.get_by_label("Bob");
        harness.key_press(egui::Key::Escape);
        harness.run();
        assert!(harness.query_by_label("Ada").is_none());

        // No roster read yet: the picker explains how to get one.
        let mut chat = ChatState::default();
        chat.apply_remote_projection(remote_projection(CHAT, "Preview"));
        chat.select_remote_conversation(0);
        let mut harness = self::harness(chat);
        harness.run();
        harness
            .get_by_role(egui::accesskit::Role::TextInput)
            .focus();
        harness.run();
        harness
            .get_by_role(egui::accesskit::Role::TextInput)
            .type_text("@");
        harness.run();
        harness.get_by_label("Open People to enable @mentions");
    }
}
