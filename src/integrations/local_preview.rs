//! Deterministic adapter for the existing on-device preview.

use super::{
    CommandDisposition, CommandReceipt, ConnectionStatus, SyncInterest, TransportError,
    WorkspaceCommand, WorkspaceDelta, WorkspaceTransport,
};

/// Stages user intent locally while feature-owned state remains the source of
/// truth for the preview. It gives the coordinator a real adapter today
/// without implying that a local action was sent to Microsoft 365.
#[derive(Default)]
pub(crate) struct LocalPreviewTransport {
    next_command_id: u64,
    staged_commands: Vec<WorkspaceCommand>,
}

impl LocalPreviewTransport {
    #[cfg(test)]
    pub(crate) fn take_staged_commands(&mut self) -> Vec<WorkspaceCommand> {
        std::mem::take(&mut self.staged_commands)
    }
}

impl WorkspaceTransport for LocalPreviewTransport {
    fn connection_status(&self) -> ConnectionStatus {
        ConnectionStatus::LocalPreview
    }

    fn poll(&mut self, interest: SyncInterest) -> Result<WorkspaceDelta, TransportError> {
        Ok(WorkspaceDelta {
            completed: vec![interest],
            ..WorkspaceDelta::default()
        })
    }

    fn dispatch(&mut self, command: WorkspaceCommand) -> Result<CommandReceipt, TransportError> {
        // Quote replies intentionally have no local-preview counterpart. The
        // UI only exposes them for selected Microsoft 365 history, but this
        // adapter must reject a manually constructed all-local command too
        // rather than falsely reporting a provider-only mutation as staged.
        if matches!(
            &command,
            WorkspaceCommand::ReplyToChatMessageWithQuote { .. }
        ) {
            return Err(TransportError::Unsupported(
                "quote replies are available only for Microsoft 365 chats".into(),
            ));
        }
        if matches!(
            &command,
            WorkspaceCommand::EditChatMessage { .. } | WorkspaceCommand::DeleteChatMessage { .. }
        ) {
            return Err(TransportError::Unsupported(
                "editing and deleting messages is available only for Microsoft 365 chats".into(),
            ));
        }
        if matches!(&command, WorkspaceCommand::MarkChatRead { .. }) {
            return Err(TransportError::Unsupported(
                "read receipts are available only for Microsoft 365 chats".into(),
            ));
        }
        if matches!(
            &command,
            WorkspaceCommand::UploadChatFile { .. }
                | WorkspaceCommand::ShareUploadedFileWithChat { .. }
                | WorkspaceCommand::SendChatMessage {
                    attachment: Some(_),
                    ..
                }
        ) {
            return Err(TransportError::Unsupported(
                "file attachments are available only for Microsoft 365 chats".into(),
            ));
        }
        command.validate()?;
        let command_id = self.next_command_id.checked_add(1).ok_or_else(|| {
            TransportError::Unavailable("local command sequence is exhausted".into())
        })?;
        self.next_command_id = command_id;
        self.staged_commands.push(command);
        Ok(CommandReceipt {
            command_id: format!("local:{command_id}"),
            disposition: CommandDisposition::StagedLocally,
            upload: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::LocalPreviewTransport;
    use crate::integrations::{
        CommandDisposition, ConnectionStatus, ProviderResourceRef, RemoteChatMessageImportance,
        SyncInterest, WorkspaceCommand, WorkspaceTransport,
    };

    #[test]
    fn local_adapter_refuses_uploads_and_file_attachments() {
        use crate::integrations::{LocalUploadSource, UploadedFileHandle};
        let mut transport = LocalPreviewTransport::default();
        let upload = WorkspaceCommand::UploadChatFile {
            chat: ProviderResourceRef::local_preview("chat-42"),
            source: LocalUploadSource::for_test(
                std::path::PathBuf::from("/tmp/x"),
                1,
                None,
                "x.txt",
            ),
        };
        assert!(transport.dispatch(upload).is_err());
        let with_file = WorkspaceCommand::SendChatMessage {
            chat: ProviderResourceRef::local_preview("chat-42"),
            body: "".into(),
            importance: RemoteChatMessageImportance::Normal,
            attachment: Some(UploadedFileHandle::for_test(1, "x.txt", "ref")),
        };
        assert!(transport.dispatch(with_file).is_err());
        let share = WorkspaceCommand::ShareUploadedFileWithChat {
            chat: ProviderResourceRef::local_preview("chat-42"),
            upload: UploadedFileHandle::for_test(1, "x.txt", "ref"),
        };
        assert!(transport.dispatch(share).is_err());
    }

    #[test]
    fn local_adapter_reports_preview_status_and_completes_requested_poll() {
        let mut transport = LocalPreviewTransport::default();

        assert_eq!(
            transport.connection_status(),
            ConnectionStatus::LocalPreview
        );
        assert_eq!(
            transport
                .poll(SyncInterest::Teams)
                .expect("local poll must succeed")
                .completed,
            vec![SyncInterest::Teams]
        );
    }

    #[test]
    fn local_adapter_stages_valid_intent_with_monotonic_receipts() {
        let mut transport = LocalPreviewTransport::default();

        let first = transport
            .dispatch(WorkspaceCommand::SendChatMessage {
                chat: ProviderResourceRef::local_preview("chat-42"),
                body: "Hello".into(),
                importance: RemoteChatMessageImportance::Normal,
                attachment: None,
            })
            .expect("local intent must stage");
        let second = transport
            .dispatch(WorkspaceCommand::SetMessageReaction {
                message: ProviderResourceRef::local_preview("message-9"),
                reaction: "👍".into(),
                selected: true,
            })
            .expect("local intent must stage");

        assert_eq!(first.command_id, "local:1");
        assert_eq!(first.disposition, CommandDisposition::StagedLocally);
        assert_eq!(second.command_id, "local:2");
        assert!(matches!(
            transport.take_staged_commands().as_slice(),
            [
                WorkspaceCommand::SendChatMessage {
                    importance: RemoteChatMessageImportance::Normal,
                    ..
                },
                WorkspaceCommand::SetMessageReaction { .. }
            ]
        ));
        assert!(transport.take_staged_commands().is_empty());
    }

    #[test]
    fn local_adapter_rejects_quote_reply_even_when_all_targets_are_local() {
        let mut transport = LocalPreviewTransport::default();

        let error = transport
            .dispatch(WorkspaceCommand::ReplyToChatMessageWithQuote {
                chat: ProviderResourceRef::local_preview("chat-42"),
                messages: vec![ProviderResourceRef::local_preview("message-7")],
                body: "This cannot masquerade as a local reply".into(),
            })
            .expect_err("quote replies must remain remote-only");

        assert!(matches!(
            error,
            crate::integrations::TransportError::Unsupported(_)
        ));
        assert!(transport.take_staged_commands().is_empty());
    }
}
