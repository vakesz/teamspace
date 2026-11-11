use super::{
    AudioDevice, CallFilter, CallForwarding, CallsState, LocalContactError,
    MAX_DURABLE_CALL_CONTACT_BYTES, MAX_LOCAL_CALL_HISTORY_ENTRIES, MAX_SPEED_DIAL_ENTRIES,
    call_history,
};
use crate::testing::{render_at_size, render_headlessly, with_escape_press};
use crate::ui::toast::ToastState;
use eframe::egui;

fn local_history_contacts(calls: &CallsState) -> Vec<&str> {
    calls
        .local_call_history
        .iter()
        .map(|entry| entry.contact.as_str())
        .collect()
}

fn local_history_ids(calls: &CallsState) -> Vec<u64> {
    calls
        .local_call_history
        .iter()
        .map(|entry| entry.id)
        .collect()
}

#[test]
fn local_call_is_recorded_and_clears_the_dial_target() {
    let mut calls = CallsState {
        call_target: "  Design review  ".into(),
        ..Default::default()
    };
    let target = calls.call_target.clone();

    assert_eq!(
        calls.start_local_call(&target),
        Some("Design review".into())
    );
    assert_eq!(local_history_contacts(&calls), ["Design review"]);
    assert!(calls.call_target.is_empty());
    assert_eq!(
        calls
            .active_call
            .as_ref()
            .map(|call| call.participant.as_str()),
        Some("Design review")
    );
    assert_eq!(calls.end_active_call(), Some("Design review".into()));
    assert!(calls.active_call.is_none());
    assert_eq!(calls.start_local_call("   "), None);
}

#[test]
fn active_local_call_prevents_an_accidental_second_call() {
    let mut calls = CallsState::default();

    assert_eq!(
        calls.start_local_call("Wren Castellanos"),
        Some("Wren Castellanos".into())
    );
    assert_eq!(calls.start_local_call("Dara Whitlock"), None);
    assert_eq!(local_history_contacts(&calls), ["Wren Castellanos"]);
    assert_eq!(
        calls
            .active_call
            .as_ref()
            .map(|call| call.participant.as_str()),
        Some("Wren Castellanos")
    );
}

#[test]
fn call_start_feedback_covers_empty_targets_and_active_sessions() {
    let mut calls = CallsState::default();

    assert_eq!(
        calls.call_start_blocked_message(false),
        Some("Type a name before starting a local call")
    );

    calls.call_target = "Wren Castellanos".into();
    assert_eq!(calls.call_start_blocked_message(false), None);
    assert_eq!(
        calls.call_start_blocked_message(true),
        Some("Leave the active local meeting before starting a call")
    );

    assert!(
        calls
            .start_call("Tove Ashworth", super::CallMode::Audio)
            .is_some()
    );
    assert_eq!(
        calls.call_start_blocked_message(false),
        Some("End the active local call before starting another")
    );
}

#[test]
fn submitting_the_typed_call_target_records_a_trimmed_local_call() {
    let mut calls = CallsState {
        call_target: "  Reka Nagy  ".into(),
        ..Default::default()
    };

    assert_eq!(
        calls.start_call_from_target(false),
        Some("Reka Nagy".into())
    );
    assert!(calls.call_target.is_empty());
    assert_eq!(local_history_contacts(&calls), ["Reka Nagy"]);
}

#[test]
fn active_meeting_blocks_a_typed_call_without_clearing_its_target() {
    let mut calls = CallsState {
        call_target: "Design review".into(),
        ..Default::default()
    };

    assert_eq!(calls.start_call_from_target(true), None);
    assert_eq!(calls.call_target, "Design review");
    assert!(calls.local_call_history.is_empty());
    assert!(!calls.has_active_call());
}

#[test]
fn history_can_collapse_and_reopens_when_a_local_call_is_recorded() {
    let mut calls = CallsState::default();
    assert!(calls.history_open);

    calls.toggle_history();
    assert!(!calls.history_open);
    render_headlessly(|ui| {
        let mut toast = ToastState::default();
        calls.draw_history(ui, &mut toast);
    });

    assert_eq!(
        calls.record_outgoing_call("Design review"),
        Some("Design review".into())
    );
    assert!(calls.history_open);
    render_headlessly(|ui| {
        let mut toast = ToastState::default();
        calls.draw_history(ui, &mut toast);
    });
}

#[test]
fn every_call_filter_has_matching_local_history() {
    for filter in [
        CallFilter::All,
        CallFilter::Missed,
        CallFilter::Incoming,
        CallFilter::Outgoing,
        CallFilter::Voicemail,
    ] {
        assert!(
            call_history()
                .into_iter()
                .any(|(_, direction, _, _)| filter.includes(direction)),
            "{} should include at least one call",
            filter.label(),
        );
    }
}

#[test]
fn externally_completed_outgoing_calls_are_added_without_changing_the_dial_target() {
    let mut calls = CallsState {
        call_target: "Dara Whitlock".into(),
        ..Default::default()
    };

    assert_eq!(
        calls.record_outgoing_call("Wren Castellanos"),
        Some("Wren Castellanos".into())
    );
    assert_eq!(calls.call_target, "Dara Whitlock");
    assert_eq!(local_history_contacts(&calls), ["Wren Castellanos"]);
}

#[test]
fn removing_local_history_is_durable_and_never_reuses_an_id() {
    let mut calls = CallsState::default();
    assert_eq!(
        calls.record_outgoing_call("Design review"),
        Some("Design review".into())
    );
    assert_eq!(
        calls.record_outgoing_call("Roadmap review"),
        Some("Roadmap review".into())
    );
    let removed_id = calls.local_call_history[0].id;
    calls.mark_persisted();

    assert_eq!(
        calls.remove_local_history_entry(removed_id),
        Some("Roadmap review".into())
    );
    assert_eq!(local_history_contacts(&calls), ["Design review"]);
    assert_eq!(local_history_ids(&calls), [1]);
    assert_eq!(calls.next_local_history_id, 3);
    assert!(calls.is_persistence_dirty());

    let mut restored = CallsState::from_stored(calls.stored());
    assert_eq!(local_history_contacts(&restored), ["Design review"]);
    assert_eq!(local_history_ids(&restored), [1]);
    assert_eq!(restored.next_local_history_id, 3);
    assert_eq!(
        restored.record_outgoing_call("Follow-up"),
        Some("Follow-up".into())
    );
    assert_eq!(local_history_ids(&restored), [3, 1]);
}

#[test]
fn removing_an_active_calls_history_entry_does_not_end_the_call() {
    let mut calls = CallsState::default();
    assert_eq!(
        calls.start_call("Wren Castellanos", super::CallMode::Audio),
        Some("Wren Castellanos".into())
    );
    let history_id = calls.local_call_history[0].id;
    calls.mark_persisted();

    assert_eq!(
        calls.remove_local_history_entry(history_id),
        Some("Wren Castellanos".into())
    );
    assert!(calls.has_active_call());
    assert_eq!(
        calls
            .active_call
            .as_ref()
            .map(|call| call.participant.as_str()),
        Some("Wren Castellanos")
    );
    assert!(calls.local_call_history.is_empty());
    assert!(calls.is_persistence_dirty());
}

#[test]
fn reference_history_cannot_be_removed_by_the_local_history_operation() {
    let mut calls = CallsState::default();
    calls.mark_persisted();
    let reference_history = call_history();

    assert_eq!(calls.remove_local_history_entry(1), None);
    assert_eq!(call_history(), reference_history);
    assert!(!calls.is_persistence_dirty());
}

#[test]
fn search_selects_a_contact_without_starting_a_call() {
    let mut calls = CallsState {
        contacts_open: true,
        ..Default::default()
    };
    let results = calls.search_results("wren");

    assert_eq!(results.len(), 1);
    calls.select_search_result("Wren Castellanos".into());
    assert_eq!(calls.call_target, "Wren Castellanos");
    assert!(!calls.contacts_open);
    assert!(calls.local_call_history.is_empty());
}

#[test]
fn choosing_a_history_contact_prepares_but_does_not_start_a_call() {
    let mut calls = CallsState::default();

    calls.select_search_result("Dara Whitlock".into());

    assert_eq!(calls.call_target, "Dara Whitlock");
    assert!(calls.local_call_history.is_empty());
}

#[test]
fn speed_dial_members_are_durable_and_deduplicated() {
    let mut calls = CallsState::default();

    assert!(calls.add_speed_dial("Kiran Doyle"));
    assert!(!calls.add_speed_dial("Kiran Doyle"));
    assert!(calls.add_speed_dial("Dara Whitlock"));
    assert_eq!(calls.speed_dial, ["Kiran Doyle", "Dara Whitlock"]);
    assert!(calls.local_call_history.is_empty());
    assert!(calls.is_persistence_dirty());
}

#[test]
fn a_custom_local_contact_is_durable_searchable_and_callable_from_speed_dial() {
    let mut calls = CallsState::default();
    calls.open_contacts_dialog();
    calls.local_contact_draft = "  Tove Ashworth  ".into();

    assert_eq!(calls.add_local_contact_draft(), Ok("Tove Ashworth".into()));
    assert!(calls.contacts_open);
    assert!(calls.local_contact_draft.is_empty());
    assert_eq!(calls.speed_dial, ["Tove Ashworth"]);
    assert!(calls.is_persistence_dirty());

    let mut restored = CallsState::from_stored(calls.stored());
    assert_eq!(restored.speed_dial, ["Tove Ashworth"]);
    let search = restored.search_results("tove");
    assert!(search.iter().any(|result| {
        result.label == "Tove Ashworth" && result.detail == "Calls · local speed dial"
    }));
    let contact = restored.speed_dial[0].clone();
    assert_eq!(
        restored.start_local_call(&contact),
        Some("Tove Ashworth".into())
    );
    assert_eq!(local_history_contacts(&restored), ["Tove Ashworth"]);
}

#[test]
fn custom_contact_validation_retains_rejected_drafts_and_does_not_dirty_state() {
    let mut calls = CallsState::default();
    calls.open_contacts_dialog();
    assert!(calls.add_speed_dial("Tove Ashworth"));
    calls.mark_persisted();
    let before = calls.stored();

    for (draft, expected) in [
        ("   ".to_owned(), LocalContactError::Empty),
        ("Tove\u{7} Ashworth".to_owned(), LocalContactError::Invalid),
        (
            "x".repeat(MAX_DURABLE_CALL_CONTACT_BYTES + 1),
            LocalContactError::Invalid,
        ),
        ("  tove ashworth  ".to_owned(), LocalContactError::Duplicate),
    ] {
        calls.local_contact_draft = draft.clone();
        assert_eq!(calls.add_local_contact_draft(), Err(expected));
        assert_eq!(calls.local_contact_draft, draft);
        assert_eq!(calls.stored(), before);
        assert!(!calls.is_persistence_dirty());
    }

    calls.speed_dial = (0..MAX_SPEED_DIAL_ENTRIES)
        .map(|index| format!("Local contact {index}"))
        .collect();
    calls.mark_persisted();
    let full_before = calls.stored();
    calls.local_contact_draft = "Another local contact".into();
    assert_eq!(
        calls.add_local_contact_draft(),
        Err(LocalContactError::Full)
    );
    assert_eq!(calls.local_contact_draft, "Another local contact");
    assert_eq!(calls.stored(), full_before);
    assert!(!calls.is_persistence_dirty());
}

#[test]
fn adding_a_custom_contact_never_changes_an_active_local_call() {
    let mut calls = CallsState::default();
    assert_eq!(
        calls.start_local_call("Wren Castellanos"),
        Some("Wren Castellanos".into())
    );
    let history_before = calls.local_call_history.clone();
    calls.mark_persisted();
    calls.open_contacts_dialog();
    calls.local_contact_draft = "Tove Ashworth".into();

    assert_eq!(calls.add_local_contact_draft(), Ok("Tove Ashworth".into()));
    assert_eq!(calls.local_call_history, history_before);
    assert_eq!(
        calls
            .active_call
            .as_ref()
            .map(|call| call.participant.as_str()),
        Some("Wren Castellanos")
    );
    assert_eq!(calls.speed_dial, ["Tove Ashworth"]);
    assert!(calls.is_persistence_dirty());
}

#[test]
fn calls_page_renders_empty_and_populated_speed_dial_at_compact_and_desktop_widths() {
    let mut calls = CallsState::default();
    let mut toast = ToastState::default();

    render_at_size(egui::vec2(720.0, 600.0), |ui| {
        calls.draw_page(ui, &mut toast, false)
    });

    assert!(calls.speed_dial.is_empty());
    assert!(calls.add_speed_dial("Wren Castellanos"));
    render_at_size(egui::vec2(1_100.0, 700.0), |ui| {
        calls.draw_page(ui, &mut toast, false)
    });
    assert_eq!(calls.speed_dial, ["Wren Castellanos"]);
}

#[test]
fn contacts_dialog_with_speed_dial_renders_without_a_native_window() {
    let mut calls = CallsState {
        contacts_open: true,
        speed_dial: vec!["Wren Castellanos".into()],
        ..Default::default()
    };
    let mut toast = ToastState::default();

    render_headlessly(|ui| calls.draw_page(ui, &mut toast, false));

    assert!(calls.contacts_open);
    assert_eq!(calls.speed_dial, ["Wren Castellanos"]);
}

#[test]
fn escape_closes_contacts_without_changing_speed_dial() {
    let mut calls = CallsState {
        contacts_open: true,
        speed_dial: vec!["Wren Castellanos".into()],
        local_contact_draft: "Unsaved local contact".into(),
        ..Default::default()
    };
    let mut toast = ToastState::default();

    with_escape_press(|ctx| calls.draw_contacts_dialog(ctx, &mut toast, false));

    assert!(!calls.contacts_open);
    assert!(calls.local_contact_draft.is_empty());
    assert_eq!(calls.speed_dial, ["Wren Castellanos"]);
}

#[test]
fn contacts_dialog_claims_keyboard_priority() {
    let mut calls = CallsState::default();

    assert!(!calls.has_keyboard_modal());
    calls.open_contacts_dialog();
    assert!(calls.has_keyboard_modal());
    assert!(calls.local_contact_focus_requested);
    calls.local_contact_draft = "Discard this".into();
    calls.dismiss_contextual_overlays();
    assert!(!calls.has_keyboard_modal());
    assert!(calls.local_contact_draft.is_empty());
}

#[test]
fn dismissing_contextual_overlays_keeps_an_active_call() {
    let mut calls = CallsState {
        contacts_open: true,
        local_contact_draft: "Discard this".into(),
        active_call: Some(super::ActiveCall {
            participant: "Wren Castellanos".into(),
            mode: super::CallMode::Audio,
        }),
        ..Default::default()
    };

    calls.dismiss_contextual_overlays();

    assert!(!calls.contacts_open);
    assert!(calls.local_contact_draft.is_empty());
    assert_eq!(
        calls
            .active_call
            .as_ref()
            .map(|call| call.participant.as_str()),
        Some("Wren Castellanos")
    );
}

#[test]
fn active_call_renders_without_a_native_window() {
    let mut calls = CallsState::default();
    let mut toast = ToastState::default();
    let _ = calls.start_local_call("Wren Castellanos");

    render_headlessly(|ui| calls.draw_active_call(ui.ctx(), &mut toast));

    assert!(calls.active_call.is_some());
}

#[test]
fn call_settings_are_durable_and_change_only_when_the_choice_changes() {
    let mut calls = CallsState::default();

    assert_eq!(calls.call_forwarding, CallForwarding::Disabled);
    assert_eq!(calls.audio_device, AudioDevice::Internal);
    assert!(!calls.is_persistence_dirty());
    assert!(!calls.set_call_forwarding(CallForwarding::Disabled));
    assert!(calls.set_call_forwarding(CallForwarding::Voicemail));
    assert_eq!(calls.call_forwarding, CallForwarding::Voicemail);
    assert!(!calls.set_audio_device(AudioDevice::Internal));
    assert!(calls.set_audio_device(AudioDevice::Headset));
    assert_eq!(calls.audio_device, AudioDevice::Headset);
    assert!(calls.is_persistence_dirty());
    calls.mark_persisted();
    assert!(!calls.is_persistence_dirty());
}

#[test]
fn stored_calls_restore_user_choices_but_not_active_session_state() {
    let mut calls = CallsState {
        call_target: "Session-only target".into(),
        call_filter: CallFilter::Missed,
        contacts_open: true,
        active_call: Some(super::ActiveCall {
            participant: "Session-only active call".into(),
            mode: super::CallMode::Video,
        }),
        ..Default::default()
    };
    assert!(calls.set_call_forwarding(CallForwarding::Voicemail));
    assert!(calls.set_audio_device(AudioDevice::Headset));
    assert!(calls.add_speed_dial("Wren Castellanos"));
    assert_eq!(
        calls.record_outgoing_call("Dara Whitlock"),
        Some("Dara Whitlock".into())
    );
    calls.toggle_history();

    let stored = calls.stored();
    assert_eq!(
        stored.call_forwarding,
        super::StoredCallForwarding::Voicemail
    );
    assert_eq!(stored.audio_device, super::StoredAudioDevice::Headset);
    assert_eq!(stored.speed_dial, ["Wren Castellanos"]);
    assert!(!stored.history_open);
    assert_eq!(stored.next_local_history_id, 2);
    assert_eq!(stored.local_call_history.len(), 1);
    assert_eq!(stored.local_call_history[0].id, 1);
    assert_eq!(stored.local_call_history[0].contact, "Dara Whitlock");

    let restored = CallsState::from_stored(stored);
    assert_eq!(restored.call_forwarding, CallForwarding::Voicemail);
    assert_eq!(restored.audio_device, AudioDevice::Headset);
    assert_eq!(restored.speed_dial, ["Wren Castellanos"]);
    assert_eq!(local_history_contacts(&restored), ["Dara Whitlock"]);
    assert_eq!(restored.local_call_history[0].id, 1);
    assert!(!restored.history_open);
    assert_eq!(restored.next_local_history_id, 2);
    assert!(restored.call_target.is_empty());
    assert!(matches!(restored.call_filter, CallFilter::All));
    assert!(!restored.contacts_open);
    assert!(restored.active_call.is_none());
    assert!(!restored.is_persistence_dirty());
}

#[test]
fn local_history_is_bounded_and_keeps_monotonic_ids() {
    let mut calls = CallsState::default();

    for index in 0..=MAX_LOCAL_CALL_HISTORY_ENTRIES {
        assert!(
            calls
                .record_outgoing_call(&format!("Contact {index}"))
                .is_some()
        );
    }

    assert_eq!(
        calls.local_call_history.len(),
        MAX_LOCAL_CALL_HISTORY_ENTRIES
    );
    assert_eq!(
        calls.local_call_history.first().map(|entry| entry.id),
        Some(MAX_LOCAL_CALL_HISTORY_ENTRIES as u64 + 1)
    );
    assert_eq!(
        calls.local_call_history.last().map(|entry| entry.id),
        Some(2)
    );
    assert_eq!(
        calls.next_local_history_id,
        MAX_LOCAL_CALL_HISTORY_ENTRIES as u64 + 2
    );
    assert_eq!(
        calls.stored().local_call_history.len(),
        MAX_LOCAL_CALL_HISTORY_ENTRIES
    );
}

#[test]
fn non_default_call_settings_render_without_a_native_window() {
    let mut calls = CallsState::default();
    let mut toast = ToastState::default();
    calls.call_target = "Wren Castellanos".into();
    calls.set_call_forwarding(CallForwarding::Voicemail);
    calls.set_audio_device(AudioDevice::Headset);

    render_headlessly(|ui| calls.draw_sidebar(ui, &mut toast, false));
}

#[test]
fn undoing_a_history_removal_restores_the_entry_in_order_even_after_newer_calls() {
    let mut calls = CallsState::default();
    for name in ["Oldest", "Middle", "Newest"] {
        assert!(calls.record_outgoing_call(name).is_some());
    }
    let middle_id = calls.local_call_history[1].id;
    assert_eq!(
        calls.remove_local_history_entry(middle_id),
        Some("Middle".into())
    );
    assert!(calls.record_outgoing_call("Latest").is_some());
    calls.mark_persisted();

    assert!(calls.undo_remove_history_entry());
    assert_eq!(
        local_history_contacts(&calls),
        ["Latest", "Newest", "Middle", "Oldest"]
    );
    assert!(calls.is_persistence_dirty());
    assert!(!calls.undo_remove_history_entry());
}

#[test]
fn undoing_a_speed_dial_removal_restores_its_position_unless_it_was_re_added() {
    let mut calls = CallsState::default();
    for name in ["Ada", "Grace", "Linus"] {
        assert!(calls.add_speed_dial(name));
    }
    assert_eq!(calls.remove_speed_dial(1), Some("Grace".into()));
    calls.mark_persisted();
    assert!(calls.undo_remove_speed_dial());
    assert_eq!(calls.speed_dial, ["Ada", "Grace", "Linus"]);
    assert!(calls.is_persistence_dirty());

    assert_eq!(calls.remove_speed_dial(0), Some("Ada".into()));
    assert!(calls.add_speed_dial("ada"));
    assert!(!calls.undo_remove_speed_dial());
    assert_eq!(calls.speed_dial, ["Grace", "Linus", "ada"]);
}
