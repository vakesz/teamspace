//! Session-only Microsoft 365 OneDrive-root presentation.
//!
//! The local Files feature owns uploads, paths, and local previews. This
//! module keeps provider-normalized metadata and its separately requested,
//! bounded remote preview in a distinct session-only state, so a remote Drive
//! item can never be mistaken for a local file or written into the profile
//! snapshot.

use crate::integrations::{ProviderBrowserUrl, ProviderResourceRef};
use crate::shared::file_preview::{MAX_IMAGE_PREVIEW_BYTES, MAX_TEXT_PREVIEW_BYTES};
use crate::ui::shortcuts::take_escape;
use crate::ui::theme::{self, font_size};
use eframe::egui::{self, RichText, Stroke, Vec2};
use std::fmt;
use std::sync::Arc;

/// A normalized, bounded OneDrive-root directory supplied by an integration
/// adapter. Its construction carries no Graph route, raw DTO, or local path.
/// An item may retain one integration-normalized browser destination for an
/// explicit, session-only handoff.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RemoteFilesDirectory {
    availability: RemoteFilesAvailability,
    items: Vec<RemoteFileItem>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum RemoteFilesAvailability {
    #[default]
    Available,
    AccessDenied,
}

impl RemoteFilesDirectory {
    /// Creates the bounded, read-only metadata view after an integration
    /// adapter has normalized every returned Drive item.
    pub(crate) fn microsoft365(items: Vec<RemoteFileItem>) -> Self {
        Self {
            availability: RemoteFilesAvailability::Available,
            items,
        }
    }

    /// Represents a provider refusal without fabricating a local directory or
    /// implying that the account is disconnected.
    pub(crate) fn access_denied() -> Self {
        Self {
            availability: RemoteFilesAvailability::AccessDenied,
            items: Vec::new(),
        }
    }
}

/// The conservative type supplied by the provider metadata adapter.
///
/// A display label is not authoritative: only a normalized `File` may begin
/// the separately scoped content-preview flow. Folders and malformed or
/// ambiguous provider facets remain non-previewable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteFileKind {
    File,
    Folder,
    Unknown,
}

/// One presentation-safe OneDrive item from the current user's root folder.
///
/// The provider target remains opaque. A provider-normalized browser
/// destination may be retained only for an explicit Open in browser click; it
/// is never rendered, persisted, treated as a local path, or used for a
/// download. A typed kind gates the separate, session-only preview flow.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct RemoteFileItem {
    target: ProviderResourceRef,
    name: String,
    modified_at: String,
    size_bytes: Option<u64>,
    kind: RemoteFileKind,
    kind_label: String,
    web_url: Option<ProviderBrowserUrl>,
}

impl fmt::Debug for RemoteFileItem {
    /// Redacts the opaque browser destination so incidental diagnostic output
    /// cannot turn a session-only provider URL into a log value.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteFileItem")
            .field("target", &self.target)
            .field("name", &self.name)
            .field("modified_at", &self.modified_at)
            .field("size_bytes", &self.size_bytes)
            .field("kind", &self.kind)
            .field("kind_label", &self.kind_label)
            .field("has_web_url", &self.web_url.is_some())
            .finish()
    }
}

impl RemoteFileItem {
    /// Adapter-facing constructor for fields that were already bounded and
    /// normalized at the integration seam. The browser destination remains
    /// private to this session-only projection; it deliberately offers no
    /// local path, content URL, or persistence capability.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn microsoft365(
        target: ProviderResourceRef,
        name: String,
        modified_at: String,
        size_bytes: Option<u64>,
        kind: RemoteFileKind,
        kind_label: String,
        web_url: Option<ProviderBrowserUrl>,
    ) -> Self {
        Self {
            target,
            name,
            modified_at,
            size_bytes,
            kind,
            kind_label,
            web_url,
        }
    }
}

/// A target-bound, session-only result from a future Microsoft 365 content
/// read. The provider adapter owns route construction, redirects, and byte
/// limits; this feature owns only presentation and refuses a result whose
/// target is no longer a current known file.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct RemoteFilePreview {
    target: ProviderResourceRef,
    content: RemoteFilePreviewContent,
}

impl fmt::Debug for RemoteFilePreview {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteFilePreview")
            .field("target", &self.target)
            .field("content", &self.content)
            .finish()
    }
}

impl RemoteFilePreview {
    /// Constructs a feature-facing result only after a provider adapter has
    /// normalized a content read. The opaque target is intentionally retained
    /// so stale worker results can be rejected without comparing file names.
    pub(crate) fn microsoft365(
        target: ProviderResourceRef,
        content: RemoteFilePreviewContent,
    ) -> Self {
        Self { target, content }
    }

    fn fits_preview_bounds(&self) -> bool {
        match &self.content {
            RemoteFilePreviewContent::Text(value) => value.len() as u64 <= MAX_TEXT_PREVIEW_BYTES,
            RemoteFilePreviewContent::Image { bytes, .. } => {
                bytes.len() as u64 <= MAX_IMAGE_PREVIEW_BYTES
            }
            RemoteFilePreviewContent::Unavailable => true,
        }
    }
}

/// Content that is safe for the Files surface to render in memory.
///
/// `Image` bytes never enter a local file path or persistence projection. Its
/// debug form deliberately reports only the allocation size, not user content.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum RemoteFilePreviewContent {
    Text(String),
    Image { bytes: Arc<[u8]>, cache_key: u64 },
    Unavailable,
}

impl fmt::Debug for RemoteFilePreviewContent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(_) => formatter.write_str("Text(<redacted>)"),
            Self::Image { bytes, .. } => formatter
                .debug_struct("Image")
                .field("byte_len", &bytes.len())
                .finish(),
            Self::Unavailable => formatter.write_str("Unavailable"),
        }
    }
}

#[derive(Clone, Default)]
enum RemoteFilePreviewState {
    #[default]
    None,
    Loading {
        target: ProviderResourceRef,
    },
    Ready(RemoteFilePreview),
}

/// Feature-owned ephemeral state for one account's remote OneDrive view.
#[derive(Default)]
pub(super) struct RemoteFilesState {
    availability: RemoteFilesAvailability,
    items: Vec<RemoteFileItem>,
    /// Direct user requests to hand an integration-normalized item URL to the
    /// app shell. These never enter the local Files snapshot or a preview.
    pending_open_links: Vec<ProviderBrowserUrl>,
    /// A single current file-content request. The app drains it into the
    /// serial remote worker; the feature keeps Loading until an exact target
    /// result arrives or the view is dismissed.
    pending_preview_request: Option<ProviderResourceRef>,
    preview: RemoteFilePreviewState,
    /// Cache keys for images that were rendered by egui and then discarded
    /// from the session-only preview. The app shell drains these with its UI
    /// context even when Files is no longer the active section, ensuring the
    /// image loader cannot retain bytes or decoded textures after feature
    /// state drops them.
    discarded_image_cache_keys: Vec<u64>,
    /// A one-shot signal for the app/controller. It is set only when an
    /// in-flight preview becomes obsolete, never for already rendered bytes.
    preview_cancellation_requested: bool,
    initialized: bool,
}

impl RemoteFilesState {
    pub(super) fn replace(&mut self, directory: RemoteFilesDirectory) {
        // A replacement can change the content behind a stable provider ID.
        // Until a future version/eTag contract exists, drop every preview
        // rather than presenting stale bytes under refreshed metadata.
        self.dismiss_preview();
        self.availability = directory.availability;
        self.items = directory.items;
        // A freshly applied provider projection establishes the only current
        // target set. Do not let a link clicked before that replacement escape
        // after its item has disappeared or belongs to another account.
        self.pending_open_links.clear();
        self.initialized = true;
    }

    pub(super) fn clear(&mut self) {
        self.dismiss_preview();
        self.availability = RemoteFilesAvailability::Available;
        self.items.clear();
        self.pending_open_links.clear();
        self.initialized = false;
    }

    /// Stages one browser handoff only when the explicitly clicked opaque
    /// target is still part of this current available provider projection.
    /// The URL itself stays private until the Files feature drains it for the
    /// app shell's final browser validation.
    pub(super) fn request_open(&mut self, target: ProviderResourceRef) -> bool {
        if !self.initialized || !matches!(self.availability, RemoteFilesAvailability::Available) {
            return false;
        }
        let Some(web_url) = self
            .items
            .iter()
            .find(|item| item.target == target)
            .and_then(|item| item.web_url.clone())
        else {
            return false;
        };
        self.pending_open_links.push(web_url);
        true
    }

    /// Queues a content request only for an exact currently visible provider
    /// file. A display label, folder, unknown item, or stale target cannot
    /// reach a future remote content route.
    pub(super) fn request_preview(&mut self, target: ProviderResourceRef) -> bool {
        if !self.is_current_file(&target) {
            return false;
        }
        if self.preview_targets(&target) {
            return false;
        }
        self.dismiss_preview();
        self.pending_preview_request = Some(target.clone());
        self.preview = RemoteFilePreviewState::Loading { target };
        true
    }

    pub(super) fn take_preview_request(&mut self) -> Option<ProviderResourceRef> {
        self.pending_preview_request.take()
    }

    /// Applies a provider-normalized result only when the same known file is
    /// still loading. This rejects late, unknown, folder, or oversized data
    /// without turning it into a local preview or a user-visible raw error.
    pub(super) fn apply_preview(&mut self, preview: RemoteFilePreview) -> bool {
        if !preview.fits_preview_bounds() || !self.is_current_file(&preview.target) {
            return false;
        }
        let matches_loading = matches!(
            &self.preview,
            RemoteFilePreviewState::Loading { target } if *target == preview.target
        );
        if !matches_loading {
            return false;
        }
        self.pending_preview_request = None;
        self.preview = RemoteFilePreviewState::Ready(preview);
        true
    }

    pub(super) fn take_open_links(&mut self) -> Vec<ProviderBrowserUrl> {
        std::mem::take(&mut self.pending_open_links)
    }

    pub(super) fn take_preview_cancellation_requested(&mut self) -> bool {
        std::mem::take(&mut self.preview_cancellation_requested)
    }

    pub(super) const fn is_initialized(&self) -> bool {
        self.initialized
    }

    pub(super) const fn has_preview(&self) -> bool {
        !matches!(self.preview, RemoteFilePreviewState::None)
    }

    pub(super) fn dismiss_preview(&mut self) {
        let preview = std::mem::replace(&mut self.preview, RemoteFilePreviewState::None);
        if matches!(preview, RemoteFilePreviewState::Loading { .. }) {
            self.preview_cancellation_requested = true;
        }
        if let RemoteFilePreviewState::Ready(RemoteFilePreview {
            content: RemoteFilePreviewContent::Image { cache_key, .. },
            ..
        }) = preview
        {
            self.discarded_image_cache_keys.push(cache_key);
        }
        self.pending_preview_request = None;
    }

    /// Releases image-loader state after the matching preview state has been
    /// dropped. This remains an app-context operation, rather than storing an
    /// egui context in session data, so account/profile clears work even when
    /// the Files surface is not currently visible.
    pub(super) fn forget_discarded_preview_images(&mut self, ctx: &egui::Context) {
        for cache_key in std::mem::take(&mut self.discarded_image_cache_keys) {
            ctx.forget_image(&preview_image_uri(cache_key));
        }
    }

    fn is_current_file(&self, target: &ProviderResourceRef) -> bool {
        self.initialized
            && matches!(self.availability, RemoteFilesAvailability::Available)
            && self
                .items
                .iter()
                .any(|item| item.target == *target && matches!(item.kind, RemoteFileKind::File))
    }

    fn preview_targets(&self, target: &ProviderResourceRef) -> bool {
        match &self.preview {
            RemoteFilePreviewState::Loading {
                target: active_target,
            } => active_target == target,
            RemoteFilePreviewState::Ready(preview) => preview.target == *target,
            RemoteFilePreviewState::None => false,
        }
    }

    fn preview_target(&self) -> Option<&ProviderResourceRef> {
        match &self.preview {
            RemoteFilePreviewState::None => None,
            RemoteFilePreviewState::Loading { target } => Some(target),
            RemoteFilePreviewState::Ready(preview) => Some(&preview.target),
        }
    }

    pub(super) fn draw_preview_window(&mut self, ctx: &egui::Context) {
        if !self.has_preview() {
            return;
        }
        if take_escape(ctx) {
            self.dismiss_preview();
            return;
        }

        // Borrow the name and preview for this frame instead of cloning them:
        // a ready text preview can be hundreds of kilobytes and this runs on
        // every repaint. Nothing in the window mutates `self`; dismissal is
        // applied after the borrows end.
        let Some(name) = self.preview_target().and_then(|target| {
            self.items
                .iter()
                .find(|item| item.target == *target)
                .map(|item| item.name.as_str())
        }) else {
            self.dismiss_preview();
            return;
        };

        let mut open = true;
        egui::Window::new("Microsoft 365 file preview")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(440.0)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(name)
                        .font(theme::semibold(font_size::SUBTITLE))
                        .color(theme::fg1()),
                );
                ui.label(
                    RichText::new("Session-only Microsoft 365 preview")
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg3()),
                );
                ui.add_space(12.0);
                match &self.preview {
                    RemoteFilePreviewState::Loading { .. } => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                RichText::new("Preparing a safe preview…")
                                    .font(theme::regular(font_size::BODY))
                                    .color(theme::fg3()),
                            );
                        });
                    }
                    RemoteFilePreviewState::Ready(RemoteFilePreview { content, .. }) => {
                        draw_preview_content(ui, content);
                    }
                    RemoteFilePreviewState::None => unreachable!("preview state was checked"),
                }
            });

        if !open {
            self.dismiss_preview();
        }
    }

    #[cfg(test)]
    pub(super) const fn preview_is_loading(&self) -> bool {
        matches!(self.preview, RemoteFilePreviewState::Loading { .. })
    }

    #[cfg(test)]
    pub(super) const fn preview_is_ready(&self) -> bool {
        matches!(self.preview, RemoteFilePreviewState::Ready(_))
    }

    #[cfg(test)]
    pub(super) const fn is_access_denied(&self) -> bool {
        matches!(self.availability, RemoteFilesAvailability::AccessDenied)
    }

    #[cfg(test)]
    pub(super) fn item_count(&self) -> usize {
        self.items.len()
    }
}

fn draw_preview_content(ui: &mut egui::Ui, content: &RemoteFilePreviewContent) {
    match content {
        RemoteFilePreviewContent::Text(value) => {
            egui::ScrollArea::vertical()
                .max_height(360.0)
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(value)
                            .font(egui::FontId::monospace(font_size::CAPTION))
                            .color(theme::fg1()),
                    );
                });
        }
        RemoteFilePreviewContent::Image { bytes, cache_key } => {
            ui.add(
                egui::Image::from_bytes(preview_image_uri(*cache_key), Arc::clone(bytes))
                    .max_width(500.0)
                    .max_height(360.0)
                    .corner_radius(theme::radius::MD),
            );
        }
        RemoteFilePreviewContent::Unavailable => {
            ui.label(
                RichText::new(
                    "Microsoft 365 could not produce a safe text or image preview for this file.",
                )
                .font(theme::regular(font_size::BODY))
                .color(theme::fg3()),
            );
        }
    }
}

fn preview_image_uri(cache_key: u64) -> String {
    format!("bytes://teamspace/microsoft365-preview/{cache_key:016x}")
}

/// A direct row action. The Files feature, rather than the app shell, decides
/// whether a user gesture begins a browser handoff or a content preview.
pub(super) enum RemoteFilesPageAction {
    Open(ProviderResourceRef),
    Preview(ProviderResourceRef),
}

/// Draws the separate OneDrive-root metadata panel. Browser handoffs and
/// content previews remain separate, target-bound, session-only actions.
pub(super) fn draw_root(
    ui: &mut egui::Ui,
    state: &RemoteFilesState,
    query: &str,
) -> Option<RemoteFilesPageAction> {
    debug_assert!(state.initialized);
    let mut requested_action = None;
    egui::Frame::new()
        .fill(theme::surface())
        .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
        .corner_radius(theme::radius::XL)
        .inner_margin(egui::Margin::symmetric(20, 18))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Microsoft 365 OneDrive root")
                        .font(theme::semibold(font_size::SUBTITLE))
                        .color(theme::brand_fg()),
                );
                ui.label(
                    RichText::new("Read-only metadata")
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg3()),
                );
            });
            ui.add_space(5.0);
            match state.availability {
                RemoteFilesAvailability::AccessDenied => {
                    ui.label(
                        RichText::new("Microsoft 365 denied access to OneDrive metadata.")
                            .font(theme::regular(font_size::CAPTION))
                            .color(theme::fg1()),
                    );
                    ui.label(
                        RichText::new(
                            "Verify delegated Files.Read access and your account configuration, then reconnect.",
                        )
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg3()),
                    );
                }
                RemoteFilesAvailability::Available => {
                    let query = query.trim().to_lowercase();
                    let mut shown = 0_usize;
                    for item in state.items.iter().filter(|item| item_matches(item, &query)) {
                        shown = shown.saturating_add(1);
                        if let Some(action) = draw_item_row(ui, item) {
                            // Egui can only deliver one pointer click in a
                            // frame, but retaining the first action makes the
                            // direct-user-gesture boundary explicit.
                            requested_action.get_or_insert(action);
                        }
                    }
                    if shown == 0 {
                        ui.label(
                            RichText::new(if state.items.is_empty() {
                                "No OneDrive root items were returned in this bounded view."
                            } else {
                                "No OneDrive root items match this search."
                            })
                            .font(theme::regular(font_size::CAPTION))
                            .color(theme::fg3()),
                        );
                    }
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(
                            "Preview is available only for eligible text and image files. Downloading, uploading, renaming, moving, sharing, and deletion remain unavailable.",
                        )
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg3()),
                    );
                }
            }
        });
    requested_action
}

fn item_matches(item: &RemoteFileItem, query: &str) -> bool {
    query.is_empty()
        || item.name.to_lowercase().contains(query)
        || item.kind_label.to_lowercase().contains(query)
        || item.modified_at.to_lowercase().contains(query)
}

fn draw_item_row(ui: &mut egui::Ui, item: &RemoteFileItem) -> Option<RemoteFilesPageAction> {
    let mut preview_requested = false;
    let mut open_requested = false;
    ui.horizontal(|ui| {
        let (icon, _) = ui.allocate_exact_size(Vec2::new(40.0, 40.0), egui::Sense::hover());
        ui.painter().rect_filled(icon, 6.0, theme::brand_tint());
        ui.painter().text(
            icon.center(),
            egui::Align2::CENTER_CENTER,
            item.kind_label
                .split_whitespace()
                .next()
                .unwrap_or(&item.kind_label),
            theme::semibold(font_size::CAPTION),
            theme::brand_fg(),
        );
        ui.vertical(|ui| {
            ui.label(
                RichText::new(&item.name)
                    .font(theme::semibold(font_size::BODY))
                    .color(theme::fg1()),
            );
            let size = item
                .size_bytes
                .map_or_else(|| "Size unavailable".into(), format_size);
            ui.label(
                RichText::new(format!("{} · {size}", item.modified_at))
                    .font(theme::regular(font_size::CAPTION))
                    .color(theme::fg3()),
            );
        });
        let preview_available = matches!(item.kind, RemoteFileKind::File);
        let preview = ui
            .add_enabled(preview_available, egui::Button::new("Preview"))
            .on_hover_text(if preview_available {
                "Preview safe text or image content in Teamspace"
            } else {
                "Only a provider-normalized file can be previewed"
            });
        preview_requested = preview.clicked();
        let open_available = item.web_url.is_some();
        let open = ui
            .add_enabled(open_available, egui::Button::new("Open in browser"))
            .on_hover_text(if open_available {
                "Open this Microsoft 365 item in your system browser"
            } else {
                "Microsoft 365 did not supply a browser link for this item"
            });
        open_requested = open.clicked();
    });
    ui.separator();
    if preview_requested {
        Some(RemoteFilesPageAction::Preview(item.target.clone()))
    } else if open_requested {
        Some(RemoteFilesPageAction::Open(item.target.clone()))
    } else {
        None
    }
}

fn format_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / KIB)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (KIB * KIB))
    } else {
        format!("{:.1} GB", bytes as f64 / (KIB * KIB * KIB))
    }
}
