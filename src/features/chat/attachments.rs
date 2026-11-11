//! Attaching one file to a Microsoft 365 chat message.
//!
//! The user picks a file with the system dialog; the feature checks it
//! (a regular file, at most 100 MiB, a name OneDrive accepts), marks the
//! composer Uploading, and emits one [`ChatAction::UploadRemoteAttachment`].
//! The app turns that into an upload command, binds its intent, and reports its
//! end back through [`ChatState::finish_remote_upload`]. The picked path lives
//! only in the action and is never shown, logged, or stored. See
//! `remote/attachment.rs` for the composer state.

use super::remote::{RemoteUploadOutcome, UploadFollowUp};
use super::{ChatAction, ChatState, RemoteSubmissionId};
use crate::integrations::{
    LocalUploadSource, UPLOAD_NAME_CONFLICT, UploadPickError, WorkspaceIntentId,
};
use crate::ui::toast::ToastState;
use std::path::PathBuf;

impl ChatState {
    /// Whether the composer of this chat may offer "Attach file".
    pub(crate) fn file_attach_offered(&self) -> bool {
        self.effective_remote_capabilities()
            .file_attach
            .is_offered()
    }

    /// Opens the system file dialog and starts an upload for the choice.
    pub(super) fn pick_remote_attachment(&mut self, remote_index: usize, toast: &mut ToastState) {
        if !self.file_attach_offered() {
            toast.show_error(
                "Attaching files needs the optional Files.ReadWrite permission, which this sign-in did not grant.",
            );
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .set_title("Choose a file to attach")
            .pick_file()
        else {
            return;
        };
        if let Err(message) = self.attach_remote_file(remote_index, path) {
            toast.show_error(message);
        }
    }

    /// Starts an upload for a path the dialog returned.
    pub(super) fn attach_remote_file(
        &mut self,
        remote_index: usize,
        path: PathBuf,
    ) -> Result<(), &'static str> {
        if !self.file_attach_offered() {
            return Err("Attaching files is not available in this session.");
        }
        let source = LocalUploadSource::from_pick(path).map_err(UploadPickError::user_message)?;
        let request = self
            .remote_chats
            .begin_attachment_upload(remote_index, source)
            .map_err(|error| error.user_message())?;
        self.pending_actions
            .push(ChatAction::UploadRemoteAttachment {
                submission: request.submission,
                chat: request.target,
                source: request.source,
            });
        Ok(())
    }

    /// Correlates the upload command the app enqueued with its composer.
    pub(crate) fn bind_remote_upload(
        &mut self,
        submission: RemoteSubmissionId,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.remote_chats
            .bind_upload_submission(submission, intent_id)
    }

    /// Applies the end of an upload command. `false` if no composer still
    /// waits for it (its chat left the session).
    pub(crate) fn finish_remote_upload(
        &mut self,
        intent_id: WorkspaceIntentId,
        outcome: &RemoteUploadOutcome,
    ) -> bool {
        let Some(follow_up) = self.remote_chats.finish_upload(intent_id, outcome) else {
            return false;
        };
        match follow_up {
            UploadFollowUp::Finished => {}
            // A large file goes up one fragment per command: ask for the next.
            UploadFollowUp::Continue {
                target,
                session,
                submission,
            } => self.pending_actions.push(ChatAction::ContinueRemoteUpload {
                submission,
                chat: target,
                session,
            }),
            // The upload failed after its session opened: discard it.
            UploadFollowUp::Cancel { target, session } => {
                self.pending_actions.push(ChatAction::CancelRemoteUpload {
                    chat: target,
                    session: Some(session),
                    pending_intent: None,
                });
            }
        }
        true
    }

    /// The user cancels the running upload of a chat's composer: it is
    /// forgotten now, and whatever is still queued or open is stopped.
    pub(super) fn cancel_remote_upload(&mut self, remote_index: usize) {
        if let Some((chat, cancel)) = self.remote_chats.cancel_attachment_upload(remote_index) {
            self.pending_actions.push(ChatAction::CancelRemoteUpload {
                chat,
                session: cancel.session,
                pending_intent: cancel.pending_intent,
            });
        }
    }

    /// A `403` turns attaching off for the rest of the session.
    pub(crate) fn mark_file_attach_forbidden(&mut self) {
        self.remote_file_attach_forbidden = true;
    }
}

/// A short size for a chip: bytes, KiB or MiB.
pub(super) fn format_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if (bytes as f64) < MIB {
        format!("{:.1} KiB", bytes as f64 / KIB)
    } else {
        format!("{:.1} MiB", bytes as f64 / MIB)
    }
}

/// The short reason an upload that was refused or could not be confirmed is
/// shown with.
pub(crate) fn upload_failure_reason(
    error: &crate::integrations::TransportError,
) -> RemoteUploadOutcome {
    use crate::integrations::TransportError;
    match error {
        TransportError::InvalidCommand(message) if message == UPLOAD_NAME_CONFLICT => {
            RemoteUploadOutcome::NameConflict
        }
        TransportError::InvalidCommand(_) => {
            RemoteUploadOutcome::Failed("Microsoft 365 did not accept the file.")
        }
        TransportError::Throttled { .. } => RemoteUploadOutcome::Failed(
            "Microsoft 365 is busy right now. Wait a moment and attach the file again.",
        ),
        TransportError::Unauthorized => RemoteUploadOutcome::Failed(
            "Microsoft 365 did not allow the upload. Check the Files.ReadWrite permission.",
        ),
        TransportError::Cancelled => RemoteUploadOutcome::Cancelled,
        TransportError::Unavailable(_) => {
            RemoteUploadOutcome::Failed("The upload could not be confirmed. Pick the file again.")
        }
        _ => RemoteUploadOutcome::Failed("The upload failed. Pick the file again."),
    }
}
