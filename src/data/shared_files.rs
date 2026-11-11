//! Immutable descriptors for bundled files shared across preview surfaces.
//!
//! Chat, Teams, and Files use these stable descriptors instead of each
//! maintaining their own name, source, and byte-location tables. Runtime
//! uploads remain feature-owned; this module describes only bundled reference
//! files.

use crate::shared::model::MessageAttachment;

/// A stable descriptor for one bundled shared file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SharedFileDescriptor {
    /// Stable source-independent key suitable for future remote-ID mapping.
    pub(crate) id: &'static str,
    /// Display name shown in Teams, Chat, and Files.
    pub(crate) name: &'static str,
    /// Canonical source label for the Files surface.
    pub(crate) source: &'static str,
    /// Human-readable context for a shared-items surface.
    pub(crate) detail: &'static str,
    pub(crate) kind: SharedFileKind,
    pub(crate) view: SharedFileView,
    pub(crate) asset: SharedFileAsset,
    /// Optional seed attachment key used by the chat preview data.
    pub(crate) chat_attachment_key: Option<&'static str>,
    /// Team-channel keys in which the file is shared.
    pub(crate) team_channels: &'static [&'static str],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SharedFileKind {
    PngImage,
}

impl SharedFileKind {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::PngImage => "PNG image",
        }
    }
}

/// The Files view where a descriptor belongs by default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SharedFileView {
    Shared,
}

/// A bundled asset that can be previewed without accessing a user's file
/// system. The byte URI is stable within the local preview and lets egui
/// cache the image consistently across surfaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SharedFileAsset {
    pub(crate) byte_uri: &'static str,
    pub(crate) bytes: &'static [u8],
}

const BIKE_COMMUTERS_GENERAL: &str = "Bike Commuters · General";

const SHARED_FILES: [SharedFileDescriptor; 2] = [
    SharedFileDescriptor {
        id: "layout-mockup",
        name: "layout-mockup.png",
        source: BIKE_COMMUTERS_GENERAL,
        detail: "Shared in a channel post",
        kind: SharedFileKind::PngImage,
        view: SharedFileView::Shared,
        asset: SharedFileAsset {
            byte_uri: "bytes://teamspace/layout-mockup.png",
            bytes: include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/assets/layout-mockup.png"
            )),
        },
        chat_attachment_key: Some("layout-mockup"),
        team_channels: &[BIKE_COMMUTERS_GENERAL],
    },
    SharedFileDescriptor {
        id: "supply-costs",
        name: "supply-costs.png",
        source: BIKE_COMMUTERS_GENERAL,
        detail: "Shared in a channel post",
        kind: SharedFileKind::PngImage,
        view: SharedFileView::Shared,
        asset: SharedFileAsset {
            byte_uri: "bytes://teamspace/supply-costs.png",
            bytes: include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/assets/supply-costs.png"
            )),
        },
        chat_attachment_key: Some("spreadsheet"),
        team_channels: &[BIKE_COMMUTERS_GENERAL],
    },
];

pub(crate) fn shared_files() -> &'static [SharedFileDescriptor] {
    &SHARED_FILES
}

pub(crate) fn shared_file_for_name(name: &str) -> Option<&'static SharedFileDescriptor> {
    shared_files().iter().find(|file| file.name == name)
}

pub(crate) fn shared_file_for_chat_attachment(
    attachment_key: &str,
) -> Option<&'static SharedFileDescriptor> {
    shared_files()
        .iter()
        .find(|file| file.chat_attachment_key == Some(attachment_key))
}

/// Resolves the shared descriptor for a typed chat attachment.
pub(crate) fn shared_file_for_message_attachment(
    attachment: MessageAttachment,
) -> Option<&'static SharedFileDescriptor> {
    let attachment_key = match attachment {
        MessageAttachment::LayoutMockup => "layout-mockup",
        MessageAttachment::Spreadsheet => "spreadsheet",
    };
    shared_file_for_chat_attachment(attachment_key)
}

pub(crate) fn shared_files_for_channel(
    channel: &str,
) -> impl Iterator<Item = &'static SharedFileDescriptor> {
    shared_files()
        .iter()
        .filter(move |file| file.team_channels.contains(&channel))
}

#[cfg(test)]
mod tests {
    use super::{
        SharedFileKind, SharedFileView, shared_file_for_chat_attachment,
        shared_file_for_message_attachment, shared_file_for_name, shared_files,
        shared_files_for_channel,
    };
    use crate::shared::model::MessageAttachment;
    use std::collections::BTreeSet;

    #[test]
    fn descriptors_have_unique_stable_ids_names_and_chat_attachment_keys() {
        let mut ids = BTreeSet::new();
        let mut names = BTreeSet::new();
        let mut chat_keys = BTreeSet::new();

        for file in shared_files() {
            assert!(ids.insert(file.id));
            assert!(names.insert(file.name));
            assert!(!file.source.trim().is_empty());
            assert!(!file.detail.trim().is_empty());
            assert!(!file.asset.byte_uri.trim().is_empty());
            assert!(!file.asset.bytes.is_empty());
            assert!(file.team_channels.iter().all(|channel| !channel.is_empty()));
            if let Some(key) = file.chat_attachment_key {
                assert!(chat_keys.insert(key));
            }
        }
    }

    #[test]
    fn stable_lookups_resolve_each_bundled_asset() {
        let model = shared_files()
            .iter()
            .find(|file| file.id == "layout-mockup")
            .expect("model settings descriptor");

        assert_eq!(model.name, "layout-mockup.png");
        assert_eq!(model.kind, SharedFileKind::PngImage);
        assert_eq!(model.kind.label(), "PNG image");
        assert_eq!(model.view, SharedFileView::Shared);
        assert_eq!(
            shared_file_for_name("layout-mockup.png").map(|file| file.id),
            Some("layout-mockup")
        );
        assert_eq!(
            shared_file_for_chat_attachment("layout-mockup").map(|file| file.id),
            Some("layout-mockup")
        );
        assert_eq!(
            shared_file_for_message_attachment(MessageAttachment::LayoutMockup).map(|file| file.id),
            Some("layout-mockup")
        );
        assert_eq!(
            shared_file_for_message_attachment(MessageAttachment::Spreadsheet).map(|file| file.id),
            Some("supply-costs")
        );
        assert!(shared_file_for_name("missing.png").is_none());
        assert!(shared_file_for_chat_attachment("missing").is_none());
    }

    #[test]
    fn channel_lookup_returns_only_files_shared_in_that_channel() {
        let files = shared_files_for_channel("Bike Commuters · General")
            .map(|file| file.id)
            .collect::<Vec<_>>();

        assert_eq!(files, ["layout-mockup", "supply-costs"]);
        assert!(
            shared_files_for_channel("Juniper Row All Hands · Facilities")
                .next()
                .is_none()
        );
    }
}
