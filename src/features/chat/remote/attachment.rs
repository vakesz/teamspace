//! The one file a Microsoft 365 chat composer may carry.
//!
//! A picked file is uploaded to the user's OneDrive by a separate command, so
//! the composer holds a small state machine beside the draft:
//! Uploading, then Ready (with the adapter's opaque [`UploadedFileHandle`]),
//! or Conflict (the name is taken) or Failed. Sending takes the Ready handle
//! with the draft; a rejected, cancelled or uncertain send restores it with
//! the same draft so nothing is uploaded twice. A file whose upload outcome
//! was unknown but was found afterwards is Ready only after the user accepts
//! it. The state is session-only and never persisted.

use super::{RemoteChatState, RemoteConversation, RemoteSendError, RemoteSubmissionId};
use crate::integrations::{
    LocalUploadSource, ProviderResourceRef, UploadProgress, UploadSessionRef, UploadedFileHandle,
    WorkspaceIntentId,
};

/// What the composer holds for its attachment.
pub(in crate::features::chat) enum ComposerAttachment {
    /// The upload command is queued or running.
    Uploading {
        submission: RemoteSubmissionId,
        intent_id: Option<WorkspaceIntentId>,
        name: String,
        size: u64,
        /// Bytes OneDrive has confirmed so far (a large file goes up by
        /// session, one fragment per command).
        sent: u64,
        /// The open upload session, once a large file has one.
        session: Option<UploadSessionRef>,
    },
    /// Uploaded (or found after an unknown outcome and not yet accepted).
    Ready {
        handle: UploadedFileHandle,
    },
    /// A file with that name is already in the user's chat files.
    Conflict {
        name: String,
    },
    Failed {
        name: String,
        reason: &'static str,
    },
}

/// How the composer shows its attachment.
pub(in crate::features::chat) enum AttachmentView<'a> {
    Uploading {
        name: &'a str,
        size: u64,
        sent: u64,
    },
    Ready {
        name: &'a str,
        size: u64,
    },
    /// Found after an unknown upload outcome; the user may accept it.
    Recovered {
        name: &'a str,
        size: u64,
    },
    Conflict {
        name: &'a str,
    },
    Failed {
        name: &'a str,
        reason: &'static str,
    },
}

/// What the chat feature does after an upload command ends.
pub(in crate::features::chat) enum UploadStep {
    /// Nothing more: a file is ready, or the upload failed with no session.
    Done,
    /// The session took a fragment: send the next one.
    Continue(UploadSessionRef),
    /// The upload failed after a session opened: discard that session.
    CancelSession(UploadSessionRef),
}

/// What the user's cancel leaves for the app to stop.
pub(in crate::features::chat) struct UploadCancel {
    /// The upload command that is queued or running, if bound.
    pub(in crate::features::chat) pending_intent: Option<WorkspaceIntentId>,
    /// The open session to discard, if any.
    pub(in crate::features::chat) session: Option<UploadSessionRef>,
}

/// The upload the app shell must turn into a command.
pub(in crate::features::chat) struct RemoteUploadRequest {
    pub(in crate::features::chat) submission: RemoteSubmissionId,
    pub(in crate::features::chat) target: ProviderResourceRef,
    pub(in crate::features::chat) source: LocalUploadSource,
}

/// How a finished upload command ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RemoteUploadOutcome {
    Uploaded(UploadedFileHandle),
    /// A large file's session took a fragment; the next is a separate job.
    Progress(UploadProgress),
    NameConflict,
    /// Refused, or its outcome could not be confirmed.
    Failed(&'static str),
    /// The command never started (its chat left the sync).
    Cancelled,
}

impl RemoteConversation {
    pub(in crate::features::chat) fn begin_attachment_upload(
        &mut self,
        submission: RemoteSubmissionId,
        source: LocalUploadSource,
    ) -> Result<RemoteUploadRequest, RemoteSendError> {
        if self.has_pending_send() {
            return Err(RemoteSendError::Pending);
        }
        if self.is_editing() || self.has_quote_reply() {
            return Err(RemoteSendError::AttachmentBlockedByMode);
        }
        if matches!(
            self.composer.attachment,
            Some(ComposerAttachment::Uploading { .. })
        ) {
            return Err(RemoteSendError::AttachmentUploading);
        }
        self.composer.attachment = Some(ComposerAttachment::Uploading {
            submission,
            intent_id: None,
            name: source.name().to_owned(),
            size: source.size(),
            sent: 0,
            session: None,
        });
        Ok(RemoteUploadRequest {
            submission,
            target: self.target.clone(),
            source,
        })
    }

    pub(in crate::features::chat) fn bind_upload(
        &mut self,
        submission: RemoteSubmissionId,
        intent: WorkspaceIntentId,
    ) -> bool {
        match &mut self.composer.attachment {
            Some(ComposerAttachment::Uploading {
                submission: current,
                intent_id,
                ..
            }) if *current == submission => {
                *intent_id = Some(intent);
                true
            }
            _ => false,
        }
    }

    pub(in crate::features::chat) fn has_upload_submission(
        &self,
        submission: RemoteSubmissionId,
    ) -> bool {
        matches!(
            &self.composer.attachment,
            Some(ComposerAttachment::Uploading { submission: current, .. }) if *current == submission
        )
    }

    pub(in crate::features::chat) fn has_upload_intent(&self, intent: WorkspaceIntentId) -> bool {
        matches!(
            &self.composer.attachment,
            Some(ComposerAttachment::Uploading { intent_id: Some(current), .. }) if *current == intent
        )
    }

    /// Applies the end of this conversation's upload command, and says what
    /// the app must do next.
    pub(in crate::features::chat) fn finish_upload(
        &mut self,
        intent: WorkspaceIntentId,
        outcome: RemoteUploadOutcome,
    ) -> Option<UploadStep> {
        if !self.has_upload_intent(intent) {
            return None;
        }
        let Some(ComposerAttachment::Uploading {
            name,
            session,
            sent,
            intent_id,
            ..
        }) = &mut self.composer.attachment
        else {
            return None;
        };
        let name = name.clone();
        // An upload that ends badly after a session opened discards it.
        let after_failure = session.map_or(UploadStep::Done, UploadStep::CancelSession);
        let (next, step) = match outcome {
            RemoteUploadOutcome::Progress(progress) => {
                *sent = progress.sent();
                *session = Some(progress.session());
                *intent_id = None;
                return Some(UploadStep::Continue(progress.session()));
            }
            RemoteUploadOutcome::Uploaded(handle) => {
                (Some(ComposerAttachment::Ready { handle }), UploadStep::Done)
            }
            RemoteUploadOutcome::NameConflict => {
                (Some(ComposerAttachment::Conflict { name }), after_failure)
            }
            RemoteUploadOutcome::Failed(reason) => (
                Some(ComposerAttachment::Failed { name, reason }),
                after_failure,
            ),
            RemoteUploadOutcome::Cancelled => (None, after_failure),
        };
        self.composer.attachment = next;
        Some(step)
    }

    /// Gives the next fragment's command a fresh submission.
    fn set_upload_submission(&mut self, submission: RemoteSubmissionId) {
        if let Some(ComposerAttachment::Uploading {
            submission: current,
            intent_id,
            ..
        }) = &mut self.composer.attachment
        {
            *current = submission;
            *intent_id = None;
        }
    }

    /// The user cancels an upload that is running. The composer forgets it at
    /// once; what the app must still stop (a queued command, an open session)
    /// comes back.
    pub(in crate::features::chat) fn cancel_upload(&mut self) -> Option<UploadCancel> {
        let Some(ComposerAttachment::Uploading {
            intent_id, session, ..
        }) = &self.composer.attachment
        else {
            return None;
        };
        let cancel = UploadCancel {
            pending_intent: *intent_id,
            session: *session,
        };
        self.composer.attachment = None;
        Some(cancel)
    }

    /// The user accepts a file that was found after an unknown outcome.
    pub(in crate::features::chat) fn accept_recovered_attachment(&mut self) -> bool {
        match self.composer.attachment.take() {
            Some(ComposerAttachment::Ready { handle }) if handle.is_recovered() => {
                self.composer.attachment = Some(ComposerAttachment::Ready {
                    handle: handle.accepted(),
                });
                true
            }
            other => {
                self.composer.attachment = other;
                false
            }
        }
    }

    /// Drops the attachment from the composer. The file stays in OneDrive.
    /// Not while the upload runs or a send holds it.
    pub(in crate::features::chat) fn remove_attachment(&mut self) -> bool {
        if self.has_pending_send()
            || matches!(
                self.composer.attachment,
                Some(ComposerAttachment::Uploading { .. }) | None
            )
        {
            return false;
        }
        self.composer.attachment = None;
        true
    }

    pub(in crate::features::chat) fn attachment_view(&self) -> Option<AttachmentView<'_>> {
        Some(match self.composer.attachment.as_ref()? {
            ComposerAttachment::Uploading {
                name, size, sent, ..
            } => AttachmentView::Uploading {
                name,
                size: *size,
                sent: *sent,
            },
            ComposerAttachment::Ready { handle } if handle.is_recovered() => {
                AttachmentView::Recovered {
                    name: handle.name(),
                    size: handle.size(),
                }
            }
            ComposerAttachment::Ready { handle } => AttachmentView::Ready {
                name: handle.name(),
                size: handle.size(),
            },
            ComposerAttachment::Conflict { name } => AttachmentView::Conflict { name },
            ComposerAttachment::Failed { name, reason } => AttachmentView::Failed { name, reason },
        })
    }

    /// The handle a send may use, or why it may not yet.
    pub(super) fn sendable_attachment(
        &self,
    ) -> Result<Option<UploadedFileHandle>, RemoteSendError> {
        match &self.composer.attachment {
            None
            | Some(ComposerAttachment::Conflict { .. } | ComposerAttachment::Failed { .. }) => {
                Ok(None)
            }
            Some(ComposerAttachment::Uploading { .. }) => Err(RemoteSendError::AttachmentUploading),
            Some(ComposerAttachment::Ready { handle }) if handle.is_recovered() => {
                Err(RemoteSendError::AttachmentNotAccepted)
            }
            Some(ComposerAttachment::Ready { handle }) => Ok(Some(handle.clone())),
        }
    }

    /// A file that is ready to send as is (uploaded and accepted).
    pub(in crate::features::chat) fn has_sendable_attachment(&self) -> bool {
        matches!(
            &self.composer.attachment,
            Some(ComposerAttachment::Ready { handle }) if !handle.is_recovered()
        )
    }

    /// Whether the composer holds an attachment in any state.
    pub(super) const fn has_attachment(&self) -> bool {
        self.composer.attachment.is_some()
    }
}

impl RemoteChatState {
    pub(in crate::features::chat) fn begin_attachment_upload(
        &mut self,
        index: usize,
        source: LocalUploadSource,
    ) -> Result<RemoteUploadRequest, RemoteSendError> {
        if self.conversations.get(index).is_none() {
            return Err(RemoteSendError::Missing);
        }
        let submission = self.allocate_submission_id()?;
        self.conversations
            .get_mut(index)
            .ok_or(RemoteSendError::Missing)?
            .begin_attachment_upload(submission, source)
    }

    fn every_conversation(&self) -> impl Iterator<Item = &RemoteConversation> {
        self.conversations.iter().chain(&self.hidden_conversations)
    }

    fn every_conversation_mut(&mut self) -> impl Iterator<Item = &mut RemoteConversation> {
        self.conversations
            .iter_mut()
            .chain(self.hidden_conversations.iter_mut())
    }

    pub(in crate::features::chat) fn bind_upload_submission(
        &mut self,
        submission: RemoteSubmissionId,
        intent: WorkspaceIntentId,
    ) -> bool {
        self.every_conversation_mut()
            .any(|conversation| conversation.bind_upload(submission, intent))
    }

    pub(in crate::features::chat) fn has_upload_submission(
        &self,
        submission: RemoteSubmissionId,
    ) -> bool {
        self.every_conversation()
            .any(|conversation| conversation.has_upload_submission(submission))
    }

    /// Applies an upload command's end. A fragment taken by a session asks
    /// for the next fragment's command, with its own submission.
    pub(in crate::features::chat) fn finish_upload(
        &mut self,
        intent: WorkspaceIntentId,
        outcome: &RemoteUploadOutcome,
    ) -> Option<UploadFollowUp> {
        let visible = self.conversations.len();
        let index = self
            .conversations
            .iter()
            .chain(&self.hidden_conversations)
            .position(|conversation| conversation.has_upload_intent(intent))?;
        let target;
        let step;
        {
            let conversation = self.conversation_at_mut(index, visible);
            target = conversation.target.clone();
            step = conversation.finish_upload(intent, outcome.clone())?;
        }
        Some(match step {
            UploadStep::Done => UploadFollowUp::Finished,
            UploadStep::CancelSession(session) => UploadFollowUp::Cancel { target, session },
            UploadStep::Continue(session) => {
                let Ok(submission) = self.allocate_submission_id() else {
                    return Some(UploadFollowUp::Cancel { target, session });
                };
                self.conversation_at_mut(index, visible)
                    .set_upload_submission(submission);
                UploadFollowUp::Continue {
                    target,
                    session,
                    submission,
                }
            }
        })
    }

    fn conversation_at_mut(&mut self, index: usize, visible: usize) -> &mut RemoteConversation {
        if index < visible {
            &mut self.conversations[index]
        } else {
            &mut self.hidden_conversations[index - visible]
        }
    }

    /// The user cancels the running upload of one chat's composer.
    pub(in crate::features::chat) fn cancel_attachment_upload(
        &mut self,
        index: usize,
    ) -> Option<(ProviderResourceRef, UploadCancel)> {
        let conversation = self.conversations.get_mut(index)?;
        let target = conversation.target.clone();
        conversation.cancel_upload().map(|cancel| (target, cancel))
    }
}

/// What the chat feature does after an upload command ends.
pub(in crate::features::chat) enum UploadFollowUp {
    Finished,
    Continue {
        target: ProviderResourceRef,
        session: UploadSessionRef,
        submission: RemoteSubmissionId,
    },
    Cancel {
        target: ProviderResourceRef,
        session: UploadSessionRef,
    },
}
