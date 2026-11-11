use super::{ProviderBrowserUrl, ProviderResourceRef, RemoteMessageInstant, TransportError};
use std::fmt;
use std::sync::Arc;

/// A provider-normalized OneDrive-root metadata snapshot.
///
/// The metadata projection is session-only and deliberately does not carry a
/// local path, download URL, content bytes, sharing metadata, or a mutation
/// capability. It may carry an opaque, validated browser-display destination
/// for an explicit system-browser handoff. A separately scoped,
/// target-bound `RemoteFilePreviewProjection` is the only way a current file
/// can gain a bounded in-memory preview; metadata alone cannot become a local
/// upload or content source.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RemoteFilesProjection {
    availability: RemoteFilesAvailability,
    items: Vec<RemoteDriveItem>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum RemoteFilesAvailability {
    #[default]
    Available,
    AccessDenied,
}

impl RemoteFilesProjection {
    /// Constructs a bounded OneDrive-root view only inside a provider adapter
    /// after each returned item has been normalized.
    pub(in crate::integrations) fn microsoft365(items: Vec<RemoteDriveItem>) -> Self {
        Self {
            availability: RemoteFilesAvailability::Available,
            items,
        }
    }

    /// Replaces old remote metadata when Graph denies this optional read.
    /// A 403 can reflect delegated consent or account-specific authorization,
    /// so the normalized state does not guess which condition applies.
    pub(in crate::integrations) fn access_denied() -> Self {
        Self {
            availability: RemoteFilesAvailability::AccessDenied,
            items: Vec::new(),
        }
    }

    pub(crate) const fn is_access_denied(&self) -> bool {
        matches!(self.availability, RemoteFilesAvailability::AccessDenied)
    }

    /// Returns presentation-safe metadata from the signed-in user's OneDrive
    /// root only. The adapter never exposes provider DTOs or route details.
    pub(crate) fn items(&self) -> &[RemoteDriveItem] {
        &self.items
    }

    #[cfg(test)]
    pub(crate) fn for_test(items: Vec<RemoteDriveItem>) -> Self {
        Self::microsoft365(items)
    }
}

/// A bounded, session-only preview result for one explicitly selected
/// OneDrive-root file. The adapter has already followed its provider-owned
/// content route, enforced content limits, and discarded every route and
/// redirect URL before constructing this value.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct RemoteFilePreviewProjection {
    target: ProviderResourceRef,
    payload: RemoteFilePreviewPayload,
}

impl fmt::Debug for RemoteFilePreviewProjection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteFilePreviewProjection")
            .field("target", &self.target)
            .field("payload", &self.payload)
            .finish()
    }
}

impl RemoteFilePreviewProjection {
    /// Creates one normalized preview result only after a Microsoft 365
    /// adapter has validated its selected Drive item and discarded protocol
    /// details. This has no local path, browser URL, or mutation capability.
    pub(in crate::integrations) fn microsoft365(
        target: ProviderResourceRef,
        payload: RemoteFilePreviewPayload,
    ) -> Result<Self, TransportError> {
        target.require_microsoft365("OneDrive preview item")?;
        target.validate("OneDrive preview item id")?;
        Ok(Self { target, payload })
    }

    /// Returns the opaque item selection that owns this result.
    pub(crate) fn target(&self) -> ProviderResourceRef {
        self.target.clone()
    }

    /// Returns a normalized in-memory payload. Its custom debug rendering
    /// deliberately omits text and byte contents.
    pub(crate) fn payload(&self) -> &RemoteFilePreviewPayload {
        &self.payload
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        provider_id: impl Into<String>,
        payload: RemoteFilePreviewPayload,
    ) -> Self {
        Self::microsoft365(ProviderResourceRef::microsoft365(provider_id), payload)
            .expect("test OneDrive preview targets must be valid Microsoft 365 identifiers")
    }
}

/// The only normalized content forms that may leave the Microsoft 365 adapter
/// for a one-shot OneDrive preview. Raw response headers, content URLs, and
/// provider error bodies are deliberately excluded.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum RemoteFilePreviewPayload {
    Text(String),
    Image { bytes: Arc<[u8]>, cache_key: u64 },
    Unavailable(RemoteFilePreviewUnavailable),
}

impl fmt::Debug for RemoteFilePreviewPayload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(text) => formatter
                .debug_struct("Text")
                .field("byte_len", &text.len())
                .finish(),
            Self::Image { bytes, cache_key } => formatter
                .debug_struct("Image")
                .field("byte_len", &bytes.len())
                .field("cache_key", cache_key)
                .finish(),
            Self::Unavailable(reason) => {
                formatter.debug_tuple("Unavailable").field(reason).finish()
            }
        }
    }
}

/// Presentation-safe reasons an explicitly selected remote file is not
/// previewable. The adapter maps all raw HTTP and content details into one of
/// these stable categories before crossing the integration seam.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteFilePreviewUnavailable {
    AccessDenied,
    TooLarge,
    UnsupportedContent,
    InvalidContent,
    Unavailable,
}

/// The conservative presentation class for a Microsoft 365 drive item.
///
/// A provider response with ambiguous or malformed facets stays `Unknown`;
/// callers must not infer that an unknown item is downloadable, writable, or
/// a local folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteDriveItemKind {
    File,
    Folder,
    Unknown,
}

/// One bounded, presentation-safe item from a Microsoft 365 drive folder.
///
/// The signed-in user's OneDrive root and a selected Teams channel's backing
/// folder use the same metadata shape, but retain distinct enclosing
/// projections and lifecycle contracts. The provider-issued target remains
/// opaque outside the integrations seam.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteDriveItem {
    target: ProviderResourceRef,
    name: String,
    modified_at: String,
    /// Parsed once at the seam, so a feature can show the time in the user's
    /// zone without parsing display text. `None` when unknown.
    modified_instant: Option<RemoteMessageInstant>,
    size_bytes: Option<u64>,
    kind: RemoteDriveItemKind,
    // A provider-normalized, optional browser destination. It remains
    // session-only and is validated again at the explicit browser handoff.
    web_url: Option<ProviderBrowserUrl>,
}

impl RemoteDriveItem {
    pub(in crate::integrations) fn microsoft365(
        target: ProviderResourceRef,
        name: String,
        modified_at: String,
        size_bytes: Option<u64>,
        kind: RemoteDriveItemKind,
    ) -> Result<Self, TransportError> {
        Self::microsoft365_with_web_url(target, name, modified_at, size_bytes, kind, None)
    }

    /// Constructs a Microsoft 365 item with an optional opaque browser URL.
    /// Raw provider strings cannot reach this seam: only the integration-owned
    /// [`ProviderBrowserUrl`] constructor may establish that invariant.
    pub(in crate::integrations) fn microsoft365_with_web_url(
        target: ProviderResourceRef,
        name: String,
        modified_at: String,
        size_bytes: Option<u64>,
        kind: RemoteDriveItemKind,
        web_url: Option<ProviderBrowserUrl>,
    ) -> Result<Self, TransportError> {
        target.require_microsoft365("drive item")?;
        target.validate("drive item id")?;
        Ok(Self {
            target,
            name,
            modified_instant: RemoteMessageInstant::parse(&modified_at),
            modified_at,
            size_bytes,
            kind,
            web_url,
        })
    }

    /// An opaque provider-issued ID for a future provider-specific file
    /// action. It is not a local path, a content location, or a URL accessor.
    pub(crate) fn target(&self) -> ProviderResourceRef {
        self.target.clone()
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn modified_at(&self) -> &str {
        &self.modified_at
    }

    pub(crate) const fn modified_instant(&self) -> Option<RemoteMessageInstant> {
        self.modified_instant
    }

    pub(crate) const fn size_bytes(&self) -> Option<u64> {
        self.size_bytes
    }

    pub(crate) const fn kind(&self) -> RemoteDriveItemKind {
        self.kind
    }

    /// Returns an optional provider-normalized browser destination. This is
    /// not a local path or content URL, and callers must still use the app's
    /// explicit HTTPS browser boundary before opening it.
    pub(crate) fn web_url(&self) -> Option<&ProviderBrowserUrl> {
        self.web_url.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        provider_id: impl Into<String>,
        name: impl Into<String>,
        modified_at: impl Into<String>,
        size_bytes: Option<u64>,
        kind: RemoteDriveItemKind,
    ) -> Self {
        Self::microsoft365(
            ProviderResourceRef::microsoft365(provider_id),
            name.into(),
            modified_at.into(),
            size_bytes,
            kind,
        )
        .expect("test drive item targets must be valid Microsoft 365 identifiers")
    }

    #[cfg(test)]
    pub(crate) fn for_test_with_web_url(
        provider_id: impl Into<String>,
        name: impl Into<String>,
        modified_at: impl Into<String>,
        size_bytes: Option<u64>,
        kind: RemoteDriveItemKind,
        web_url: &str,
    ) -> Self {
        Self::microsoft365_with_web_url(
            ProviderResourceRef::microsoft365(provider_id),
            name.into(),
            modified_at.into(),
            size_bytes,
            kind,
            Some(ProviderBrowserUrl::for_test(web_url)),
        )
        .expect("test drive item targets must be valid Microsoft 365 identifiers")
    }
}
