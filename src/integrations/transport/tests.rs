use super::{
    CommandTargetProvider, ProviderBrowserUrl, ProviderResourceRef, RemoteChatConversation,
    RemoteChatKind, RemoteChatMessage, RemoteChatMessageImportance, RemoteChatMessagesProjection,
    RemoteChatProjection, RemoteDriveItem, RemoteDriveItemKind, TransportError, WorkspaceCommand,
};
use crate::shared::reaction::ReactionKind;

#[test]
fn provider_browser_urls_are_strict_and_redacted() {
    let destination = "https://tenant.sharepoint.com/:x:/r/personal/opaque-debug-sentinel";
    let url = ProviderBrowserUrl::microsoft365(destination)
        .expect("plain HTTPS browser destination must normalize");

    assert_eq!(url.as_ref(), destination);
    assert!(!format!("{url:?}").contains(destination));
    let item = RemoteDriveItem::for_test_with_web_url(
        "opaque-browser-url-item",
        "Roadmap.xlsx",
        "2026-11-06T10:30:00Z",
        Some(1_536),
        RemoteDriveItemKind::File,
        destination,
    );
    assert!(!format!("{item:?}").contains(destination));
    for value in [
        "http://tenant.sharepoint.com/sites/Team",
        "https:/tenant.sharepoint.com/sites/Team",
        "https:////tenant.sharepoint.com/sites/Team",
        "https://user:pass@tenant.sharepoint.com/sites/Team",
        "https://tenant.sharepoint.com/\nnext",
        "https://",
    ] {
        assert!(
            ProviderBrowserUrl::microsoft365(value).is_none(),
            "{value:?} must not become a browser destination"
        );
    }
    let too_long = format!(
        "https://tenant.sharepoint.com/{}",
        "x".repeat(super::MAX_PROVIDER_BROWSER_URL_BYTES)
    );
    assert!(ProviderBrowserUrl::microsoft365(&too_long).is_none());
}

#[test]
fn microsoft365_identifier_accessor_never_exposes_a_local_preview_key() {
    let local = ProviderResourceRef::local_preview("fixture-chat");
    let remote = ProviderResourceRef::microsoft365("graph-chat-id");

    assert_eq!(local.microsoft365_id(), None);
    assert_eq!(remote.microsoft365_id(), Some("graph-chat-id"));
}

#[test]
fn intent_commands_reject_missing_provider_independent_fields() {
    let command = WorkspaceCommand::SendChannelPost {
        team: ProviderResourceRef::local_preview("team-1"),
        channel: ProviderResourceRef::local_preview("channel-1"),
        body: "   ".into(),
    };

    assert_eq!(
        command.validate(),
        Err(TransportError::InvalidCommand(
            "post body is required".into()
        ))
    );
}

#[test]
fn intent_commands_allow_normalized_remote_identifiers() {
    let commands = [
        WorkspaceCommand::SetChatMessageReaction {
            chat: ProviderResourceRef::microsoft365("19:chat@thread.v2"),
            message: ProviderResourceRef::microsoft365("1710172611094"),
            reaction: ReactionKind::Like,
            selected: true,
        },
        WorkspaceCommand::ReplyToChannelPost {
            team: ProviderResourceRef::microsoft365("team-1"),
            channel: ProviderResourceRef::microsoft365("channel-1"),
            post: ProviderResourceRef::microsoft365("1710172611094"),
            body: "I can take this.".into(),
        },
        WorkspaceCommand::SetChannelMessageReplyReaction {
            team: ProviderResourceRef::microsoft365("team-1"),
            channel: ProviderResourceRef::microsoft365("channel-1"),
            post: ProviderResourceRef::microsoft365("1710172611094"),
            reply: ProviderResourceRef::microsoft365("1710172611095"),
            reaction: ReactionKind::Like,
            selected: true,
        },
    ];

    for command in commands {
        assert_eq!(command.validate(), Ok(()));
        assert_eq!(command.validate_for_microsoft365(), Ok(()));
    }
}

#[test]
fn intent_commands_allow_multiline_message_content() {
    let command = WorkspaceCommand::SendChatMessage {
        chat: ProviderResourceRef::local_preview("chat-42"),
        body: "First line\nSecond line".into(),
        importance: RemoteChatMessageImportance::Normal,
        attachment: None,
    };

    assert_eq!(command.validate(), Ok(()));
}

#[test]
fn command_target_provider_requires_one_adapter_family() {
    let local = WorkspaceCommand::SendChatMessage {
        chat: ProviderResourceRef::local_preview("local-chat"),
        body: "This stays in the preview".into(),
        importance: RemoteChatMessageImportance::Normal,
        attachment: None,
    };
    let remote = WorkspaceCommand::SendChatMessage {
        chat: ProviderResourceRef::microsoft365("19:remote@thread.v2"),
        body: "This stays remote".into(),
        importance: RemoteChatMessageImportance::Normal,
        attachment: None,
    };
    let mixed = WorkspaceCommand::SendChannelPost {
        team: ProviderResourceRef::microsoft365("team-1"),
        channel: ProviderResourceRef::local_preview("channel-1"),
        body: "Never route this ambiguously".into(),
    };
    let remote_reaction = WorkspaceCommand::SetChatMessageReaction {
        chat: ProviderResourceRef::microsoft365("19:remote@thread.v2"),
        message: ProviderResourceRef::microsoft365("remote-message"),
        reaction: ReactionKind::Like,
        selected: true,
    };
    let mixed_reaction = WorkspaceCommand::SetChatMessageReaction {
        chat: ProviderResourceRef::microsoft365("19:remote@thread.v2"),
        message: ProviderResourceRef::local_preview("local-message"),
        reaction: ReactionKind::Like,
        selected: true,
    };

    assert_eq!(
        local.target_provider(),
        Ok(CommandTargetProvider::LocalPreview)
    );
    assert_eq!(
        remote.target_provider(),
        Ok(CommandTargetProvider::Microsoft365)
    );
    assert_eq!(
        remote_reaction.target_provider(),
        Ok(CommandTargetProvider::Microsoft365)
    );
    assert!(matches!(
        mixed.target_provider(),
        Err(TransportError::InvalidCommand(detail)) if detail.contains("cannot mix")
    ));
    assert!(matches!(
        mixed_reaction.target_provider(),
        Err(TransportError::InvalidCommand(detail)) if detail.contains("cannot mix")
    ));
}

#[test]
fn microsoft365_validation_rejects_a_local_preview_target() {
    let command = WorkspaceCommand::SendChatMessage {
        chat: ProviderResourceRef::local_preview("fixture-chat"),
        body: "This stays local".into(),
        importance: RemoteChatMessageImportance::Normal,
        attachment: None,
    };

    assert_eq!(
        command.validate_for_microsoft365(),
        Err(TransportError::Unsupported(
            "Microsoft Graph cannot target a local preview chat".into()
        ))
    );
}

#[test]
fn microsoft365_validation_checks_every_command_target() {
    let commands = vec![
        WorkspaceCommand::SetChatMessageReaction {
            chat: ProviderResourceRef::microsoft365("19:chat@thread.v2"),
            message: ProviderResourceRef::local_preview("fixture-message"),
            reaction: ReactionKind::Like,
            selected: true,
        },
        WorkspaceCommand::SendChannelPost {
            team: ProviderResourceRef::microsoft365("team-1"),
            channel: ProviderResourceRef::local_preview("fixture-channel"),
            body: "This channel post stays local".into(),
        },
        WorkspaceCommand::ReplyToChannelPost {
            team: ProviderResourceRef::microsoft365("team-1"),
            channel: ProviderResourceRef::microsoft365("channel-1"),
            post: ProviderResourceRef::local_preview("fixture-post"),
            body: "This reply stays local".into(),
        },
        WorkspaceCommand::SetChannelMessageReaction {
            team: ProviderResourceRef::microsoft365("team-1"),
            channel: ProviderResourceRef::microsoft365("channel-1"),
            post: ProviderResourceRef::local_preview("fixture-post"),
            reaction: ReactionKind::Like,
            selected: true,
        },
        WorkspaceCommand::SetChannelMessageReplyReaction {
            team: ProviderResourceRef::microsoft365("team-1"),
            channel: ProviderResourceRef::microsoft365("channel-1"),
            post: ProviderResourceRef::microsoft365("fixture-post"),
            reply: ProviderResourceRef::local_preview("fixture-reply"),
            reaction: ReactionKind::Like,
            selected: true,
        },
        WorkspaceCommand::SetMessageReaction {
            message: ProviderResourceRef::local_preview("fixture-message"),
            reaction: "👍".into(),
            selected: true,
        },
        WorkspaceCommand::CreateCalendarEvent {
            calendar: ProviderResourceRef::local_preview("local-primary"),
            title: "Local event".into(),
            starts_at: "2026-11-03T14:00:00".into(),
            ends_at: "2026-11-03T14:30:00".into(),
            online_meeting: false,
        },
        WorkspaceCommand::UploadFile {
            destination: ProviderResourceRef::local_preview("local-files"),
            file_name: "local.txt".into(),
            size_bytes: 1,
        },
    ];

    for command in commands {
        assert!(matches!(
            command.validate_for_microsoft365(),
            Err(TransportError::Unsupported(detail)) if detail.contains("local preview")
        ));
    }
}

#[test]
fn remote_chat_projection_rejects_a_local_preview_target() {
    let result = RemoteChatConversation::microsoft365(
        ProviderResourceRef::local_preview("fixture-chat"),
        RemoteChatKind::OneOnOne,
        "Fixture chat".into(),
        "Local preview".into(),
        "FC".into(),
        false,
        Vec::new(),
    );

    assert_eq!(
        result,
        Err(TransportError::Unsupported(
            "Microsoft Graph cannot target a local preview chat".into()
        ))
    );
}

#[test]
fn selected_chat_history_projection_rejects_local_targets_and_keeps_ids_opaque() {
    let local = RemoteChatMessagesProjection::microsoft365(
        ProviderResourceRef::local_preview("fixture-chat"),
        Vec::new(),
    );
    assert_eq!(
        local,
        Err(TransportError::Unsupported(
            "Microsoft Graph cannot target a local preview chat".into()
        ))
    );

    let provider_id = "19:opaque-history-chat@thread.v2";
    let projection = RemoteChatMessagesProjection::microsoft365(
        ProviderResourceRef::microsoft365(provider_id),
        vec![RemoteChatMessage::microsoft365(
            None,
            "Ada".into(),
            "2026-11-06T12:00:00Z".into(),
            "Normalized history text".into(),
            false,
            false,
            RemoteChatMessageImportance::Normal,
            false,
            Vec::new(),
            Vec::new(),
        )],
    )
    .expect("a provider-issued chat target must construct a history projection");

    assert_eq!(projection.chat().microsoft365_id(), Some(provider_id));
    assert_eq!(
        projection.messages()[0].plain_text(),
        "Normalized history text"
    );
    assert!(!format!("{projection:?}").contains(provider_id));
    assert!(!format!("{:?}", projection.chat()).contains(provider_id));
}

#[test]
fn remote_chat_projection_preserves_an_opaque_send_target_without_debug_leakage() {
    let provider_id = "19:opaque-chat-id@thread.v2";
    let conversation = RemoteChatConversation::microsoft365(
        ProviderResourceRef::microsoft365(provider_id),
        RemoteChatKind::Group,
        "Design review".into(),
        "Group chat".into(),
        "DR".into(),
        true,
        vec![RemoteChatMessage::microsoft365(
            None,
            "Ada".into(),
            "2026-11-06T12:00:00Z".into(),
            "The normalized message body is plain text.".into(),
            false,
            false,
            RemoteChatMessageImportance::Normal,
            false,
            Vec::new(),
            Vec::new(),
        )],
    )
    .expect("a provider-issued chat target must construct a remote projection");
    let projection = RemoteChatProjection::microsoft365(vec![conversation]);

    let chat = projection
        .conversations()
        .first()
        .expect("projection must retain the conversation");
    assert_eq!(chat.target().microsoft365_id(), Some(provider_id));
    assert_eq!(chat.kind(), RemoteChatKind::Group);
    assert_eq!(chat.title(), "Design review");
    assert!(chat.is_unread());
    assert_eq!(
        chat.messages()[0].plain_text(),
        "The normalized message body is plain text."
    );
    assert!(!format!("{chat:?}").contains(provider_id));
    assert!(!format!("{:?}", chat.target()).contains(provider_id));
}

#[test]
fn edit_and_delete_commands_validate_and_stay_provider_exclusive() {
    let remote = |id: &str| ProviderResourceRef::microsoft365(id);
    let local = |id: &str| ProviderResourceRef::local_preview(id);
    let edit = |chat, message, body: &str| WorkspaceCommand::EditChatMessage {
        chat,
        message,
        body: body.into(),
    };
    let delete = |chat, message| WorkspaceCommand::DeleteChatMessage { chat, message };

    assert_eq!(
        edit(remote("c"), remote("m"), "x").target_provider(),
        Ok(CommandTargetProvider::Microsoft365)
    );
    assert_eq!(
        delete(remote("c"), remote("m")).target_provider(),
        Ok(CommandTargetProvider::Microsoft365)
    );
    assert_eq!(
        delete(local("c"), local("m")).target_provider(),
        Ok(CommandTargetProvider::LocalPreview)
    );
    for command in [
        edit(remote("c"), local("m"), "x"),
        edit(local("c"), remote("m"), "x"),
        delete(remote("c"), local("m")),
        delete(local("c"), remote("m")),
    ] {
        assert!(
            matches!(
                command.target_provider(),
                Err(TransportError::InvalidCommand(_))
            ),
            "{command:?}"
        );
    }
    assert!(edit(remote("c"), remote("m"), "  ").validate().is_err());
    assert!(
        edit(remote("c"), remote("m"), "x")
            .validate_for_microsoft365()
            .is_ok()
    );
    assert!(
        edit(local("c"), local("m"), "x")
            .validate_for_microsoft365()
            .is_err()
    );
    assert!(
        delete(local("c"), local("m"))
            .validate_for_microsoft365()
            .is_err()
    );
}

#[test]
fn mute_digest_is_deterministic_keyed_and_never_the_chat_id() {
    let chat = ProviderResourceRef::microsoft365("19:digest-chat@thread.v2");
    let other = ProviderResourceRef::microsoft365("19:digest-other@thread.v2");
    let secret = [7u8; 32];

    let digest = chat.mute_digest(&secret, "acct-1").expect("remote chat");
    assert_eq!(chat.mute_digest(&secret, "acct-1"), Some(digest));
    assert_eq!(digest.len(), 16);

    assert_ne!(
        other.mute_digest(&secret, "acct-1"),
        Some(digest),
        "per chat"
    );
    assert_ne!(
        chat.mute_digest(&secret, "acct-2"),
        Some(digest),
        "per account"
    );
    assert_ne!(
        chat.mute_digest(&[8u8; 32], "acct-1"),
        Some(digest),
        "per secret"
    );

    // The 0 separator stops account/chat boundary shifts from colliding.
    let a = ProviderResourceRef::microsoft365("bc");
    let b = ProviderResourceRef::microsoft365("c");
    assert_ne!(a.mute_digest(&secret, "a"), b.mute_digest(&secret, "ab"));

    // A local-preview reference has no Microsoft 365 ID to digest.
    let local = ProviderResourceRef::local_preview("fixture");
    assert_eq!(local.mute_digest(&secret, "acct-1"), None);
}

#[test]
fn stable_key_is_equal_for_equal_refs_distinct_otherwise_and_hides_the_id() {
    let a = ProviderResourceRef::microsoft365("19:secret-message-id");
    let same = ProviderResourceRef::microsoft365("19:secret-message-id");
    let other = ProviderResourceRef::microsoft365("19:another-message-id");
    let local = ProviderResourceRef::local_preview("19:secret-message-id");
    assert_eq!(a.stable_key(), same.stable_key());
    assert_ne!(a.stable_key(), other.stable_key());
    assert_ne!(a.stable_key(), local.stable_key());
    assert!(!format!("{a:?}").contains("secret-message-id"));
}

#[test]
fn an_upload_command_is_validated_and_routed_by_its_chat() {
    use super::{LocalUploadSource, MAX_CHAT_UPLOAD_BYTES, MAX_CHAT_UPLOAD_SESSION_BYTES};
    let source = |name: &str, size: u64| {
        LocalUploadSource::for_test(std::path::PathBuf::from("/tmp/picked"), size, None, name)
    };
    let remote = |name: &str, size: u64| WorkspaceCommand::UploadChatFile {
        chat: ProviderResourceRef::microsoft365("19:chat@thread.v2"),
        source: source(name, size),
    };
    assert_eq!(
        remote("a.txt", 5).target_provider(),
        Ok(CommandTargetProvider::Microsoft365)
    );
    assert!(remote("a.txt", 5).validate_for_microsoft365().is_ok());
    // Over 4 MiB a session is used; the 100 MiB bound is inclusive.
    assert!(
        remote("a.txt", MAX_CHAT_UPLOAD_BYTES + 1)
            .validate()
            .is_ok()
    );
    assert!(
        remote("a.txt", MAX_CHAT_UPLOAD_SESSION_BYTES)
            .validate()
            .is_ok()
    );
    assert!(
        remote("a.txt", MAX_CHAT_UPLOAD_SESSION_BYTES + 1)
            .validate()
            .is_err()
    );
    // A bad name never reaches an adapter.
    assert!(remote("a/b.txt", 5).validate().is_err());
    assert!(remote("..", 5).validate().is_err());
    // A local-preview chat is not a Graph route.
    let local = WorkspaceCommand::UploadChatFile {
        chat: ProviderResourceRef::local_preview("chat-1"),
        source: source("a.txt", 5),
    };
    assert_eq!(
        local.target_provider(),
        Ok(CommandTargetProvider::LocalPreview)
    );
    assert!(local.validate_for_microsoft365().is_err());
    // The command never prints its path.
    assert!(!format!("{:?}", remote("a.txt", 5)).contains("/tmp/picked"));
}

#[test]
fn a_file_may_stand_in_for_the_message_text() {
    use super::{ComposedBody, UploadedFileHandle};
    let send =
        |body: &str, attachment: Option<UploadedFileHandle>| WorkspaceCommand::SendChatMessage {
            chat: ProviderResourceRef::microsoft365("19:chat@thread.v2"),
            body: ComposedBody::plain(body),
            importance: RemoteChatMessageImportance::Normal,
            attachment,
        };
    let handle = || Some(UploadedFileHandle::for_test(1, "a.txt", "ref"));
    assert!(send("", None).validate().is_err());
    assert!(send("hi", None).validate().is_ok());
    assert!(send("", handle()).validate().is_ok());
    assert!(send("a\0b", handle()).validate().is_err());
}

#[test]
fn sharing_a_file_is_validated_and_routed_by_its_chat() {
    use super::UploadedFileHandle;
    let share = |chat: ProviderResourceRef| WorkspaceCommand::ShareUploadedFileWithChat {
        chat,
        upload: UploadedFileHandle::for_test(1, "a.txt", "ref"),
    };
    let remote = share(ProviderResourceRef::microsoft365("19:chat@thread.v2"));
    assert_eq!(
        remote.target_provider(),
        Ok(CommandTargetProvider::Microsoft365)
    );
    assert!(remote.validate_for_microsoft365().is_ok());
    let local = share(ProviderResourceRef::local_preview("chat-1"));
    assert_eq!(
        local.target_provider(),
        Ok(CommandTargetProvider::LocalPreview)
    );
    assert!(local.validate_for_microsoft365().is_err());
    // The command never prints the file's name or reference.
    assert!(!format!("{remote:?}").contains("a.txt"));
}
