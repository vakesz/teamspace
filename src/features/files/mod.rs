mod persistence;
#[cfg(feature = "fuzzing")]
pub(crate) use persistence::fuzz_snapshot;
mod remote;

use self::persistence::{FILES_SCHEMA_VERSION, MAX_LOCAL_UPLOADS, StoredLocalFile};
pub(crate) use self::persistence::{LocalFilesStore, StoredFiles};
pub(crate) use self::remote::{
    RemoteFileItem, RemoteFileKind, RemoteFilePreview, RemoteFilePreviewContent,
    RemoteFilesDirectory,
};
use crate::data::shared_files::{
    SharedFileDescriptor, SharedFileView, shared_file_for_name, shared_files,
};
use crate::features::search::workspace::{
    FileViewTarget, WorkspaceSearchResult, WorkspaceSearchTarget,
};
use crate::shared::file_preview::{
    MAX_IMAGE_PREVIEW_BYTES, MAX_IMAGE_PREVIEW_PIXELS, MAX_TEXT_PREVIEW_BYTES,
    next_preview_cache_key,
};
use crate::ui::a11y::clickable;
use crate::ui::icon::{Icon, IconSize};
use crate::ui::modal;
use crate::ui::shortcuts::take_escape;
use crate::ui::theme::{self, font_size};
use crate::ui::toast::ToastState;
use eframe::egui::{self, Align, Layout, RichText, Stroke, Vec2};
use std::fmt;
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) struct FilesState {
    query: String,
    view: FileView,
    picker_open: bool,
    uploaded_files: Vec<UploadedFile>,
    selected_file: Option<SelectedFile>,
    /// A provider-owned metadata view that never enters the local Files
    /// snapshot, upload list, or preview lifecycle.
    remote_files: remote::RemoteFilesState,
    persistence_dirty: bool,
}

impl Default for FilesState {
    fn default() -> Self {
        Self {
            query: String::new(),
            view: FileView::Recent,
            picker_open: false,
            uploaded_files: Vec::new(),
            selected_file: None,
            remote_files: remote::RemoteFilesState::default(),
            persistence_dirty: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileView {
    Recent,
    Shared,
    Downloads,
    OneDrive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FilePreviewKind {
    BundledImage,
    Document,
    Spreadsheet,
    Pdf,
    LocalImage,
    LocalText,
    Unavailable,
}

impl FileView {
    const fn label(self) -> &'static str {
        match self {
            Self::Recent => "Recent",
            Self::Shared => "Shared",
            Self::Downloads => "Downloads",
            Self::OneDrive => "OneDrive",
        }
    }

    fn local_search_target(self) -> FileViewTarget {
        match self {
            Self::Recent => FileViewTarget::Recent,
            Self::Shared => FileViewTarget::Shared,
            Self::Downloads => FileViewTarget::Downloads,
            // Remote OneDrive metadata is deliberately absent from global
            // search targets until it has an explicit remote-selection
            // contract. This function is called only for local file records.
            Self::OneDrive => unreachable!("remote OneDrive metadata has no local search target"),
        }
    }

    const fn from_search_target(value: FileViewTarget) -> Self {
        match value {
            FileViewTarget::Recent => Self::Recent,
            FileViewTarget::Shared => Self::Shared,
            FileViewTarget::Downloads => Self::Downloads,
        }
    }

    const fn from_shared_file_view(value: SharedFileView) -> Self {
        match value {
            SharedFileView::Shared => Self::Shared,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct UploadedFile {
    name: String,
    kind: String,
    view: FileView,
    local_path: Option<PathBuf>,
    size_bytes: Option<u64>,
    preview_kind: FilePreviewKind,
}

#[derive(Clone, Copy)]
struct SeededFile {
    name: &'static str,
    source: &'static str,
    when: &'static str,
    kind: &'static str,
    view: FileView,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SelectedFile {
    name: String,
    source: String,
    when: String,
    kind: String,
    local_path: Option<PathBuf>,
    size_bytes: Option<u64>,
    preview_kind: FilePreviewKind,
    local_preview: LocalPreviewState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LocalPreviewState {
    NotLoaded,
    Image { bytes: Arc<[u8]>, cache_key: u64 },
    Text(String),
    Unavailable(String),
    Error(String),
}

enum LocalFileAdd {
    Added(String),
    Duplicate(String),
}

#[derive(Debug)]
enum FileIngestionError {
    Unavailable { path: PathBuf, error: String },
    NotAFile(PathBuf),
    TooManyLocalFiles,
}

impl fmt::Display for FileIngestionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { path, error } => {
                write!(formatter, "could not read {}: {error}", path.display())
            }
            Self::NotAFile(path) => write!(formatter, "{} is not a file", path.display()),
            Self::TooManyLocalFiles => {
                formatter.write_str("the local Files list has reached its safe limit")
            }
        }
    }
}

impl UploadedFile {
    fn bundled_example(file: &SharedFileDescriptor) -> Self {
        Self {
            name: file.name.into(),
            kind: file.kind.label().into(),
            view: FileView::Recent,
            local_path: None,
            size_bytes: None,
            preview_kind: FilePreviewKind::BundledImage,
        }
    }

    fn local(path: PathBuf, size_bytes: u64) -> Self {
        Self {
            name: display_file_name(&path),
            kind: file_kind(&path),
            view: FileView::Recent,
            preview_kind: local_preview_kind(&path),
            local_path: Some(path),
            size_bytes: Some(size_bytes),
        }
    }

    fn source_label(&self) -> String {
        self.local_path.as_deref().map_or_else(
            || "You · Local preview".into(),
            |path| format!("You · {}", path.display()),
        )
    }

    fn selected_file(&self) -> SelectedFile {
        SelectedFile {
            name: self.name.clone(),
            source: self.source_label(),
            when: "Just now".into(),
            kind: self.kind.clone(),
            local_path: self.local_path.clone(),
            size_bytes: self.size_bytes,
            preview_kind: self.preview_kind,
            local_preview: LocalPreviewState::NotLoaded,
        }
    }
}

impl SelectedFile {
    fn referenced(name: String, source: String, when: String, kind: String) -> Self {
        Self {
            preview_kind: preview_kind(&name),
            name,
            source,
            when,
            kind,
            local_path: None,
            size_bytes: None,
            local_preview: LocalPreviewState::NotLoaded,
        }
    }

    fn prepare_local_preview(&mut self) -> Option<String> {
        if self.local_path.is_none() || !matches!(self.local_preview, LocalPreviewState::NotLoaded)
        {
            return None;
        }
        let path = self.local_path.as_deref()?;
        self.local_preview = load_local_preview(path, self.preview_kind);
        match &self.local_preview {
            LocalPreviewState::Error(error) => {
                Some(format!("Could not preview {}: {error}", self.name))
            }
            _ => None,
        }
    }
}

impl FilesState {
    /// Replaces the signed-in account's read-only OneDrive-root metadata.
    /// This is a session-only projection: local uploads, selected paths, and
    /// durable Files metadata remain completely separate.
    pub(crate) fn replace_remote_files_directory(&mut self, directory: RemoteFilesDirectory) {
        self.remote_files.replace(directory);
    }

    /// Drops the remote account's OneDrive projection at an account/profile
    /// boundary. If the remote-only tab was active, return to the local view
    /// rather than leaving the user on a no-longer-owned surface.
    pub(crate) fn clear_remote_files_directory(&mut self) {
        self.remote_files.clear();
        if self.view == FileView::OneDrive {
            self.view = FileView::Recent;
        }
    }

    /// Drains direct user requests to open one current Microsoft 365 OneDrive
    /// item in the system browser. Provider URLs remain session-only: they
    /// never enter local Files storage, previews, or uploads, and the app
    /// shell must revalidate each URL immediately before launch.
    pub(crate) fn take_remote_file_open_links(
        &mut self,
    ) -> Vec<crate::integrations::ProviderBrowserUrl> {
        self.remote_files.take_open_links()
    }

    /// Drains one explicit current OneDrive file-preview request. The opaque
    /// target remains provider-owned; the app passes it to the background
    /// transport without learning a route, URL, or local path.
    pub(crate) fn take_remote_file_preview_request(
        &mut self,
    ) -> Option<crate::integrations::ProviderResourceRef> {
        self.remote_files.take_preview_request()
    }

    /// Applies a target-bound, session-only preview result. A late result is
    /// ignored if its file disappeared, became non-previewable, or another
    /// preview superseded it while the provider worker was running.
    pub(crate) fn apply_remote_file_preview(&mut self, preview: RemoteFilePreview) -> bool {
        self.remote_files.apply_preview(preview)
    }

    /// Releases egui image-loader data for remote previews that were already
    /// dismissed from session state. The app shell calls this every frame so
    /// a navigation, account, or projection clear does not need the Files
    /// surface to stay visible just to evict its private image bytes.
    pub(crate) fn forget_discarded_remote_preview_images(&mut self, ctx: &egui::Context) {
        self.remote_files.forget_discarded_preview_images(ctx);
    }

    /// Drains the one-shot request to stop an obsolete selected-file content
    /// read. This is deliberately separate from account-wide transport
    /// cancellation so replacing a preview cannot disrupt other remote work.
    pub(crate) fn take_remote_file_preview_cancellation_requested(&mut self) -> bool {
        self.remote_files.take_preview_cancellation_requested()
    }

    #[cfg(test)]
    pub(crate) const fn has_remote_files_directory(&self) -> bool {
        self.remote_files.is_initialized()
    }

    #[cfg(test)]
    pub(crate) fn remote_file_count(&self) -> usize {
        self.remote_files.item_count()
    }

    #[cfg(test)]
    pub(crate) const fn remote_files_access_denied(&self) -> bool {
        self.remote_files.is_access_denied()
    }

    #[cfg(test)]
    pub(crate) fn select_one_drive_view_for_test(&mut self) -> bool {
        if !self.remote_files.is_initialized() {
            return false;
        }
        self.select_view(FileView::OneDrive);
        true
    }

    #[cfg(test)]
    pub(crate) fn is_one_drive_view(&self) -> bool {
        self.view == FileView::OneDrive
    }

    /// Rebuilds only local user-selected files. Bundled examples, selection,
    /// search text, dialogs, and previews deliberately start fresh.
    ///
    /// A successful storage load proves that the snapshot is structurally
    /// sound, but it cannot prove that every referenced path still names the
    /// same regular file. Drop entries that fail the restore-time ownership
    /// check and mark this local projection dirty so the normal atomic save
    /// lifecycle repairs only its metadata. Corrupt snapshots never reach this
    /// method, so they remain protected by the store's load failure path.
    pub(crate) fn from_stored(stored: StoredFiles) -> Self {
        let mut uploaded_files = Vec::new();
        let mut repaired_stale_metadata = false;
        for stored_file in stored.local_uploads {
            let Ok(file) = restore_local_file(stored_file.into_path()) else {
                // A removed, renamed, inaccessible, or replaced file is not
                // restored. Its app-owned metadata is removed by the pending
                // reconciliation save; the source filesystem is untouched.
                repaired_stale_metadata = true;
                continue;
            };
            let Some(path) = file.local_path.as_deref() else {
                repaired_stale_metadata = true;
                continue;
            };
            if !has_uploaded_path(&uploaded_files, path) {
                uploaded_files.push(file);
            } else {
                // Snapshot validation rejects exact duplicate strings, but
                // keep restore robust if distinct stored spellings resolve to
                // the same canonical local path.
                repaired_stale_metadata = true;
            }
        }
        Self {
            uploaded_files,
            persistence_dirty: repaired_stale_metadata,
            ..Self::default()
        }
    }

    /// Captures only canonical, UTF-8 local paths. Bundled registry entries
    /// are immutable app resources and must never enter a user snapshot.
    pub(crate) fn stored(&self) -> StoredFiles {
        StoredFiles {
            schema_version: FILES_SCHEMA_VERSION,
            local_uploads: self
                .uploaded_files
                .iter()
                .filter_map(|file| file.local_path.as_deref())
                .filter_map(StoredLocalFile::from_local_path)
                .collect(),
        }
    }

    pub(crate) const fn is_persistence_dirty(&self) -> bool {
        self.persistence_dirty
    }

    pub(crate) fn mark_persisted(&mut self) {
        self.persistence_dirty = false;
    }

    pub(crate) const fn has_keyboard_modal(&self) -> bool {
        self.picker_open || self.selected_file.is_some() || self.remote_files.has_preview()
    }

    pub(crate) fn dismiss_contextual_overlays(&mut self) {
        self.picker_open = false;
        self.selected_file = None;
        self.remote_files.dismiss_preview();
    }

    pub(crate) fn search_results(&self, query: &str) -> Vec<WorkspaceSearchResult> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return Vec::new();
        }

        let mut results = Vec::new();
        for file in &self.uploaded_files {
            let source = file.source_label();
            if file_matches(&query, &file.name, &source, &file.kind) {
                results.push(WorkspaceSearchResult {
                    label: file.name.clone(),
                    detail: format!("File · {source} · {}", file.kind),
                    target: WorkspaceSearchTarget::File {
                        name: file.name.clone(),
                        source,
                        when: "Just now".into(),
                        kind: file.kind.clone(),
                        view: file.view.local_search_target(),
                    },
                });
            }
        }
        for file in shared_files() {
            let kind = file.kind.label();
            if file_matches(&query, file.name, file.source, kind) {
                results.push(WorkspaceSearchResult {
                    label: file.name.into(),
                    detail: format!("File · {} · {kind}", file.source),
                    target: WorkspaceSearchTarget::File {
                        name: file.name.into(),
                        source: file.source.into(),
                        when: file.detail.into(),
                        kind: kind.into(),
                        view: FileView::from_shared_file_view(file.view).local_search_target(),
                    },
                });
            }
        }
        for file in seeded_files() {
            if file_matches(&query, file.name, file.source, file.kind) {
                results.push(WorkspaceSearchResult {
                    label: file.name.into(),
                    detail: format!("File · {} · {}", file.source, file.kind),
                    target: WorkspaceSearchTarget::File {
                        name: file.name.into(),
                        source: file.source.into(),
                        when: file.when.into(),
                        kind: file.kind.into(),
                        view: file.view.local_search_target(),
                    },
                });
            }
        }
        results
    }

    pub(crate) fn select_search_result(
        &mut self,
        name: String,
        source: String,
        when: String,
        kind: String,
        view: FileViewTarget,
    ) {
        self.view = FileView::from_search_target(view);
        let uploaded = self
            .uploaded_files
            .iter()
            .find(|file| file.name == name && file.source_label() == source)
            .map(UploadedFile::selected_file);
        if let Some(uploaded) = uploaded {
            self.select_file(uploaded);
        } else if source_refers_to_local_path(&source) {
            // Global search results are a snapshot. A user can forget a local
            // entry between search and activation, so never turn its old path
            // into a fabricated immutable/reference preview.
            self.picker_open = false;
            self.selected_file = None;
        } else {
            self.select_file(SelectedFile::referenced(name, source, when, kind));
        }
    }

    pub(crate) fn draw_sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(20.0);
        ui.horizontal(|ui| {
            ui.add_space(20.0);
            ui.label(
                RichText::new("Files")
                    .font(theme::semibold(font_size::TITLE))
                    .color(theme::fg1()),
            );
        });
        ui.add_space(20.0);
        ui.horizontal(|ui| {
            ui.add_space(16.0);
            ui.add_sized(
                [ui.available_width() - 30.0, 34.0],
                egui::TextEdit::singleline(&mut self.query).hint_text("Search files"),
            );
        });
        ui.add_space(16.0);
        for view in [FileView::Recent, FileView::Shared, FileView::Downloads] {
            ui.horizontal(|ui| {
                ui.add_space(20.0);
                if ui
                    .selectable_label(self.view == view, view.label())
                    .clicked()
                {
                    self.select_view(view);
                }
            });
        }
        if self.remote_files.is_initialized() {
            ui.horizontal(|ui| {
                ui.add_space(20.0);
                if ui
                    .selectable_label(self.view == FileView::OneDrive, FileView::OneDrive.label())
                    .clicked()
                {
                    self.select_view(FileView::OneDrive);
                }
            });
        }
    }

    pub(crate) fn draw_page(&mut self, ui: &mut egui::Ui, toast: &mut ToastState) {
        if self.view == FileView::OneDrive && !self.remote_files.is_initialized() {
            // A lifecycle clear normally makes this impossible. Retain the
            // guard so a future view action cannot leave the page pointed at
            // a remote surface after its account projection disappeared.
            self.view = FileView::Recent;
        }
        let showing_remote = self.view == FileView::OneDrive;
        ui.add_space(24.0);
        ui.horizontal(|ui| {
            ui.add_space(28.0);
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("Files")
                        .font(theme::semibold(font_size::TITLE))
                        .color(theme::fg1()),
                );
                ui.label(
                    RichText::new(if showing_remote {
                        "Files from your Microsoft 365 OneDrive root"
                    } else {
                        "Files shared across your conversations"
                    })
                    .font(theme::regular(font_size::CAPTION))
                    .color(theme::fg3()),
                );
            });
            if !showing_remote {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::image_and_text(
                                Icon::Add.image(IconSize::S16).tint(theme::fg_on_brand()),
                                RichText::new("Upload").color(theme::fg_on_brand()),
                            )
                            .fill(theme::brand())
                            .corner_radius(theme::radius::LG),
                        )
                        .clicked()
                    {
                        self.pick_local_file(toast);
                    }
                    if ui.button("Examples").clicked() {
                        self.open_picker();
                    }
                    ui.add_space(24.0);
                });
            }
        });
        ui.add_space(20.0);
        if showing_remote {
            if let Some(action) = remote::draw_root(ui, &self.remote_files, &self.query) {
                match action {
                    remote::RemoteFilesPageAction::Open(target) => {
                        let _ = self.request_remote_file_open(target);
                    }
                    remote::RemoteFilesPageAction::Preview(target) => {
                        let _ = self.request_remote_file_preview(target);
                    }
                }
            }
            self.remote_files.draw_preview_window(ui.ctx());
            return;
        }
        egui::Frame::new()
            .fill(theme::surface())
            .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
            .corner_radius(theme::radius::XL)
            .inner_margin(egui::Margin::symmetric(20, 18))
            .show(ui, |ui| {
                ui.label(
                    RichText::new(self.view.label())
                        .font(theme::semibold(font_size::SUBTITLE))
                        .color(theme::fg1()),
                );
                ui.add_space(8.0);
                let query = self.query.trim().to_lowercase();
                let mut shown = 0;
                let mut selection = None;
                for file in &self.uploaded_files {
                    if self.view != FileView::Recent && self.view != file.view {
                        continue;
                    }
                    if !query.is_empty()
                        && !format!("{} {}", file.name, file.kind)
                            .to_lowercase()
                            .contains(&query)
                    {
                        continue;
                    }
                    shown += 1;
                    if draw_file_row(ui, &file.name, &file.source_label(), "Just now", &file.kind)
                        .on_hover_text("Open local preview")
                        .clicked()
                    {
                        selection = Some(file.selected_file());
                    }
                }
                for file in seeded_files() {
                    if self.view != FileView::Recent && self.view != file.view {
                        continue;
                    }
                    if !query.is_empty() && !file_matches(&query, file.name, file.source, file.kind)
                    {
                        continue;
                    }
                    shown += 1;
                    if draw_file_row(ui, file.name, file.source, file.when, file.kind)
                        .on_hover_text("Open local preview")
                        .clicked()
                    {
                        selection = Some(SelectedFile::referenced(
                            file.name.into(),
                            file.source.into(),
                            file.when.into(),
                            file.kind.into(),
                        ));
                    }
                }
                for file in shared_files() {
                    let view = FileView::from_shared_file_view(file.view);
                    let kind = file.kind.label();
                    if self.view != FileView::Recent && self.view != view {
                        continue;
                    }
                    if !query.is_empty() && !file_matches(&query, file.name, file.source, kind) {
                        continue;
                    }
                    shown += 1;
                    if draw_file_row(ui, file.name, file.source, file.detail, kind)
                        .on_hover_text("Open bundled preview")
                        .clicked()
                    {
                        selection = Some(SelectedFile::referenced(
                            file.name.into(),
                            file.source.into(),
                            file.detail.into(),
                            kind.into(),
                        ));
                    }
                }
                if let Some(file) = selection {
                    self.select_file(file);
                }
                if shown == 0 {
                    ui.label(
                        RichText::new("No files match this search.")
                            .font(theme::regular(font_size::CAPTION))
                            .color(theme::fg3()),
                    );
                }
            });
        self.draw_picker(ui.ctx(), toast);
        self.draw_selected_file_window(ui.ctx(), toast);
    }

    fn open_picker(&mut self) {
        self.selected_file = None;
        self.picker_open = true;
    }

    /// Switches the presentation tab without allowing an already-open local
    /// picker or preview to overlap the remote OneDrive view.
    fn select_view(&mut self, view: FileView) {
        self.view = view;
        if view == FileView::OneDrive {
            self.dismiss_contextual_overlays();
        } else {
            self.remote_files.dismiss_preview();
        }
    }

    /// Converts only a direct Open in browser gesture into a one-shot
    /// provider URL handoff. `RemoteFilesState` rechecks the opaque target
    /// against the latest applied directory before accepting it.
    fn request_remote_file_open(
        &mut self,
        target: crate::integrations::ProviderResourceRef,
    ) -> bool {
        self.remote_files.request_open(target)
    }

    /// Starts a session-only preview for one exact current OneDrive file.
    /// It cannot derive a provider target from a name or accept a folder.
    fn request_remote_file_preview(
        &mut self,
        target: crate::integrations::ProviderResourceRef,
    ) -> bool {
        self.remote_files.request_preview(target)
    }

    #[cfg(test)]
    pub(crate) fn request_remote_file_open_for_test(
        &mut self,
        target: crate::integrations::ProviderResourceRef,
    ) -> bool {
        self.request_remote_file_open(target)
    }

    #[cfg(test)]
    pub(crate) fn request_remote_file_preview_for_test(
        &mut self,
        target: crate::integrations::ProviderResourceRef,
    ) -> bool {
        self.request_remote_file_preview(target)
    }

    #[cfg(test)]
    pub(crate) const fn remote_file_preview_is_loading_for_test(&self) -> bool {
        self.remote_files.preview_is_loading()
    }

    #[cfg(test)]
    pub(crate) const fn remote_file_preview_is_ready_for_test(&self) -> bool {
        self.remote_files.preview_is_ready()
    }

    fn select_file(&mut self, file: SelectedFile) {
        self.picker_open = false;
        self.selected_file = Some(file);
    }

    fn add_bundled_example(&mut self, name: &str) -> bool {
        let Some(file) = shared_file_for_name(name) else {
            return false;
        };
        if self
            .uploaded_files
            .iter()
            .any(|uploaded| uploaded.local_path.is_none() && uploaded.name == file.name)
        {
            return false;
        }
        self.uploaded_files
            .push(UploadedFile::bundled_example(file));
        true
    }

    fn pick_local_file(&mut self, toast: &mut ToastState) {
        self.selected_file = None;
        let Some(path) = rfd::FileDialog::new()
            .set_title("Choose a file to add")
            .pick_file()
        else {
            return;
        };

        match self.add_local_file(path) {
            Ok(LocalFileAdd::Added(name)) => {
                toast.show(format!("Added {name} to local Files"));
            }
            Ok(LocalFileAdd::Duplicate(name)) => {
                toast.show(format!("{name} is already in local Files"));
            }
            Err(error) => toast.show_error(format!("Couldn't add the selected file: {error}")),
        }
    }

    fn add_local_file(&mut self, path: PathBuf) -> Result<LocalFileAdd, FileIngestionError> {
        let file = ingest_local_file(path)?;
        let duplicate = file
            .local_path
            .as_ref()
            .is_some_and(|path| has_uploaded_path(&self.uploaded_files, path));
        if duplicate {
            return Ok(LocalFileAdd::Duplicate(file.name));
        }
        if self
            .uploaded_files
            .iter()
            .filter(|file| file.local_path.is_some())
            .count()
            >= MAX_LOCAL_UPLOADS
        {
            return Err(FileIngestionError::TooManyLocalFiles);
        }
        let name = file.name.clone();
        let persistable = file
            .local_path
            .as_deref()
            .is_some_and(|path| StoredLocalFile::from_local_path(path).is_some());
        self.uploaded_files.push(file);
        if persistable {
            self.persistence_dirty = true;
        }
        Ok(LocalFileAdd::Added(name))
    }

    /// Forgets one user-selected file from this profile's local Files list.
    ///
    /// This deliberately removes only the app's metadata. It never opens,
    /// changes, or deletes the source path supplied by the user.
    fn forget_local_file(&mut self, path: &Path) -> Option<String> {
        let mut forgotten_name = None;
        self.uploaded_files.retain(|file| {
            if file.local_path.as_deref() == Some(path) {
                forgotten_name.get_or_insert_with(|| file.name.clone());
                false
            } else {
                true
            }
        });

        let name = forgotten_name?;
        if self
            .selected_file
            .as_ref()
            .is_some_and(|file| file.local_path.as_deref() == Some(path))
        {
            self.selected_file = None;
        }
        if StoredLocalFile::from_local_path(path).is_some() {
            self.persistence_dirty = true;
        }
        Some(name)
    }

    fn forget_selected_local_file(&mut self) -> Option<String> {
        let path = self.selected_file.as_ref()?.local_path.clone()?;
        self.forget_local_file(&path)
    }

    #[cfg(test)]
    pub(crate) fn add_local_file_for_test(&mut self, path: PathBuf) -> bool {
        matches!(self.add_local_file(path), Ok(LocalFileAdd::Added(_)))
    }

    fn draw_picker(&mut self, ctx: &egui::Context, toast: &mut ToastState) {
        if !self.picker_open {
            return;
        }
        if take_escape(ctx) {
            self.picker_open = false;
            return;
        }

        let mut choice = None;
        let mut cancel = false;
        let _ = modal::show(ctx, "file_picker", 400.0, |ui| {
            modal::heading(ui, "Add bundled example");
            ui.add_space(4.0);
            ui.label(
                RichText::new("Add one of the included assets to the local Files list.")
                    .font(theme::regular(font_size::CAPTION))
                    .color(theme::fg1()),
            );
            ui.label(
                RichText::new("The selection is kept in this app session only.")
                    .font(theme::regular(font_size::CAPTION))
                    .color(theme::fg3()),
            );
            ui.add_space(10.0);
            for file in shared_files() {
                if ui
                    .add(egui::Button::image_and_text(
                        Icon::Add.image(IconSize::S16).tint(theme::fg2()),
                        file.name,
                    ))
                    .clicked()
                {
                    choice = Some(file.name);
                }
            }
            ui.add_space(10.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });

        if let Some(name) = choice {
            let added = self.add_bundled_example(name);
            toast.show(if added {
                format!("Added {name} to local Files")
            } else {
                format!("{name} is already in local Files")
            });
            cancel = true;
        }
        if cancel {
            self.picker_open = false;
        }
    }

    fn draw_selected_file_window(&mut self, ctx: &egui::Context, toast: &mut ToastState) {
        if take_escape(ctx) {
            self.selected_file = None;
            return;
        }
        let mut open = true;
        let mut forget_requested = false;
        {
            let Some(file) = self.selected_file.as_mut() else {
                return;
            };
            if let Some(message) = file.prepare_local_preview() {
                toast.show(message);
            }

            egui::Window::new("File preview")
                .open(&mut open)
                .collapsible(false)
                .resizable(true)
                .default_width(420.0)
                .show(ctx, |ui| {
                ui.label(RichText::new(&file.name).font(theme::semibold(font_size::SUBTITLE)).color(theme::fg1()));
                ui.label(
                    RichText::new(format!("{} · {} · {}", file.kind, file.source, file.when))
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg3()),
                );
                if let Some(path) = &file.local_path {
                    ui.label(
                        RichText::new(format!(
                            "{} · {}",
                            path.display(),
                            file.size_bytes.map_or_else(
                                || "size unavailable".into(),
                                format_file_size,
                            )
                        ))
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg3()),
                    );
                }
                ui.add_space(12.0);
                match file.preview_kind {
                    FilePreviewKind::BundledImage => {
                        if let Some(shared_file) = shared_file_for_name(&file.name) {
                            ui.add(
                                egui::Image::from_bytes(
                                    shared_file.asset.byte_uri,
                                    shared_file.asset.bytes,
                                )
                                .fit_to_exact_size(egui::vec2(360.0, 212.0))
                                .corner_radius(theme::radius::MD),
                            );
                        } else {
                            draw_unavailable_preview(
                                ui,
                                "This bundled file is no longer available in the local preview.",
                            );
                        }
                    }
                    FilePreviewKind::Document => {
                        let (title, summary, sections) = match file.name.as_str() {
                            "Showroom brief.docx" => (
                                "Showroom brief",
                                "A short scope for the spring showroom refit.",
                                &[
                                    ("Objective", "Agree display layouts and the signage order."),
                                    ("Owner", "Tove Ashworth · Showroom design"),
                                    ("Next step", "Walk the floor with the fitting team."),
                                ][..],
                            ),
                            "Walkthrough notes.docx" => (
                                "Walkthrough notes",
                                "Spring showroom · General",
                                &[
                                    (
                                        "Decision",
                                        "Put the oak table by the entrance and the lamps at the back.",
                                    ),
                                    (
                                        "Action",
                                        "Send the revised floor sketch once the carpenters confirm.",
                                    ),
                                    ("Follow-up", "Book the van for Thursday morning."),
                                ][..],
                            ),
                            _ => ("Document", "Local document preview", &[][..]),
                        };
                        draw_document_preview(ui, "DOC", title, summary, sections);
                    }
                    FilePreviewKind::Spreadsheet => draw_spreadsheet_preview(ui),
                    FilePreviewKind::Pdf => draw_document_preview(
                        ui,
                        "PDF",
                        "Moodboard",
                        "A local board preview for the finishing team.",
                        &[
                            (
                                "Direction",
                                "Warm oak against soft grey walls, with brass details.",
                            ),
                            (
                                "Reference",
                                "Match the stain samples to the photographed chairs.",
                            ),
                            ("Review", "Leave remarks on the pinned thread."),
                        ],
                    ),
                    FilePreviewKind::LocalImage | FilePreviewKind::LocalText => {
                        draw_local_preview(ui, file)
                    }
                    FilePreviewKind::Unavailable => {
                        if file.local_path.is_some() {
                            draw_local_preview(ui, file);
                        } else {
                            draw_unavailable_preview(
                                ui,
                                "Preview unavailable for this local sample",
                            );
                        }
                    }
                }
                ui.add_space(10.0);
                ui.label(
                    RichText::new(if file.local_path.is_some() {
                        "This session-local preview reads only the selected file; it does not upload or share it."
                    } else {
                        "This session-local preview does not open or access an external file."
                    })
                    .font(theme::regular(font_size::CAPTION))
                    .color(theme::fg3()),
                );
                if file.local_path.is_some() {
                    ui.add_space(10.0);
                    if ui
                        .button(RichText::new("Forget from local Files").color(theme::danger()))
                        .on_hover_text(
                            "Remove this app's local metadata without changing the source file",
                        )
                        .clicked()
                    {
                        forget_requested = true;
                    }
                }
            });
        }

        if forget_requested {
            if let Some(name) = self.forget_selected_local_file() {
                toast.show(format!(
                    "Forgot {name} from local Files; the source file was not changed"
                ));
            }
            return;
        }

        if !open {
            self.selected_file = None;
        }
    }
}

fn seeded_files() -> [SeededFile; 4] {
    [
        SeededFile {
            name: "Showroom brief.docx",
            source: "Tove Ashworth · Chat",
            when: "Today, 8:50 AM",
            kind: "DOC",
            view: FileView::Recent,
        },
        SeededFile {
            name: "Fitting schedule.xlsx",
            source: "Studio Moves · Planning",
            when: "Yesterday",
            kind: "XLS",
            view: FileView::Recent,
        },
        SeededFile {
            name: "Moodboard.pdf",
            source: "Finish Shop · Shared",
            when: "Monday",
            kind: "PDF",
            view: FileView::Shared,
        },
        SeededFile {
            name: "Walkthrough notes.docx",
            source: "Spring showroom · General",
            when: "Oct 23",
            kind: "DOC",
            view: FileView::Downloads,
        },
    ]
}

fn preview_kind(name: &str) -> FilePreviewKind {
    if shared_file_for_name(name).is_some() {
        FilePreviewKind::BundledImage
    } else {
        match name {
            "Showroom brief.docx" | "Walkthrough notes.docx" => FilePreviewKind::Document,
            "Fitting schedule.xlsx" => FilePreviewKind::Spreadsheet,
            "Moodboard.pdf" => FilePreviewKind::Pdf,
            _ => FilePreviewKind::Unavailable,
        }
    }
}

fn ingest_local_file(path: PathBuf) -> Result<UploadedFile, FileIngestionError> {
    let (path, size_bytes) = canonical_regular_local_file(path, false)?;
    Ok(UploadedFile::local(path, size_bytes))
}

/// Restores only a path that is still the canonical regular file the profile
/// originally recorded. A saved path replaced by a symlink must not silently
/// grant the app access to its new target on the next launch.
fn restore_local_file(path: PathBuf) -> Result<UploadedFile, FileIngestionError> {
    let (path, size_bytes) = canonical_regular_local_file(path, true)?;
    Ok(UploadedFile::local(path, size_bytes))
}

fn canonical_regular_local_file(
    path: PathBuf,
    require_original_canonical_path: bool,
) -> Result<(PathBuf, u64), FileIngestionError> {
    let canonical = fs::canonicalize(&path).map_err(|error| FileIngestionError::Unavailable {
        path: path.clone(),
        error: error.to_string(),
    })?;
    if require_original_canonical_path && canonical != path {
        return Err(FileIngestionError::Unavailable {
            path,
            error: "the saved path no longer resolves to the originally selected file".into(),
        });
    }
    let metadata = fs::metadata(&canonical).map_err(|error| FileIngestionError::Unavailable {
        path: canonical.clone(),
        error: error.to_string(),
    })?;
    if !metadata.is_file() {
        return Err(FileIngestionError::NotAFile(canonical));
    }
    Ok((canonical, metadata.len()))
}

fn has_uploaded_path(files: &[UploadedFile], path: &Path) -> bool {
    files
        .iter()
        .any(|file| file.local_path.as_deref() == Some(path))
}

fn source_refers_to_local_path(source: &str) -> bool {
    source
        .strip_prefix("You · ")
        .is_some_and(|path| Path::new(path).is_absolute())
}

fn display_file_name(path: &Path) -> String {
    path.file_name()
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn file_kind(path: &Path) -> String {
    path.extension()
        .filter(|extension| !extension.is_empty())
        .map(|extension| extension.to_string_lossy().to_uppercase())
        .unwrap_or_else(|| "FILE".into())
}

fn local_preview_kind(path: &Path) -> FilePreviewKind {
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase());
    match extension.as_deref() {
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp") => FilePreviewKind::LocalImage,
        Some(extension) if crate::shared::file_preview::is_text_preview_extension(extension) => {
            FilePreviewKind::LocalText
        }
        _ => FilePreviewKind::Unavailable,
    }
}

fn load_local_preview(path: &Path, kind: FilePreviewKind) -> LocalPreviewState {
    let file = match open_selected_local_preview_file(path) {
        Ok(file) => file,
        Err(error) => return LocalPreviewState::Error(error.to_string()),
    };
    match kind {
        FilePreviewKind::LocalImage => match read_preview_bytes(file, MAX_IMAGE_PREVIEW_BYTES) {
            Ok(bytes) => local_image_preview(bytes),
            Err(error) => LocalPreviewState::Error(error),
        },
        FilePreviewKind::LocalText => match read_preview_bytes(file, MAX_TEXT_PREVIEW_BYTES) {
            Ok(bytes) => local_text_preview(bytes),
            Err(error) => LocalPreviewState::Error(error),
        },
        _ => LocalPreviewState::Unavailable(
            "A safe preview is available for common image and UTF-8 text files.".into(),
        ),
    }
}

/// Why a selected local file could not be opened for preview.
#[derive(Debug)]
enum LocalPreviewOpenError {
    /// The persisted canonical path no longer resolves to itself (for example
    /// a symlink swap), so it is rejected before opening.
    PathChanged,
    NotRegularFile,
    Io(std::io::Error),
}

impl fmt::Display for LocalPreviewOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PathChanged => formatter
                .write_str("the selected path no longer resolves to the originally selected file"),
            Self::NotRegularFile => {
                formatter.write_str("the selected path is no longer a regular file")
            }
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for LocalPreviewOpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for LocalPreviewOpenError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Opens a preview only when the persisted canonical path still names the
/// same regular file. The Unix flags also make a symlink/FIFO swap between
/// validation and open fail safely instead of blocking the UI thread.
fn open_selected_local_preview_file(path: &Path) -> Result<fs::File, LocalPreviewOpenError> {
    let canonical = fs::canonicalize(path)?;
    if canonical != path {
        return Err(LocalPreviewOpenError::PathChanged);
    }
    let checked = fs::metadata(&canonical)?;
    if !checked.is_file() {
        return Err(LocalPreviewOpenError::NotRegularFile);
    }

    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    // Open the path that was just checked, not the caller's spelling.
    let file = options.open(&canonical)?;
    let opened_metadata = file.metadata()?;
    if !opened_metadata.is_file() {
        return Err(LocalPreviewOpenError::NotRegularFile);
    }
    // A parent directory can be swapped between the check and the open, which
    // `O_NOFOLLOW` (final component only) does not catch. The opened
    // descriptor must be the very file that was checked and that the path
    // still names.
    if !same_file(&checked, &opened_metadata)
        || !same_file(&fs::metadata(&canonical)?, &opened_metadata)
    {
        return Err(LocalPreviewOpenError::PathChanged);
    }
    Ok(file)
}

/// Compares device and inode identity on Unix. Other platforms have no stable
/// equivalent in `std`, so they rely on the canonical-path check alone.
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        left.dev() == right.dev() && left.ino() == right.ino()
    }
    #[cfg(not(unix))]
    {
        let _ = (left, right);
        true
    }
}

fn local_image_preview(bytes: Vec<u8>) -> LocalPreviewState {
    let reader = image::ImageReader::new(Cursor::new(&bytes)).with_guessed_format();
    let dimensions = reader.and_then(|reader| {
        reader
            .into_dimensions()
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    });
    match dimensions {
        Ok((width, height))
            if u64::from(width).saturating_mul(u64::from(height)) <= MAX_IMAGE_PREVIEW_PIXELS =>
        {
            let cache_key = next_preview_cache_key(&bytes);
            LocalPreviewState::Image {
                bytes: bytes.into(),
                cache_key,
            }
        }
        Ok(_) => LocalPreviewState::Unavailable(format!(
            "This image exceeds the {} pixel local preview limit.",
            MAX_IMAGE_PREVIEW_PIXELS
        )),
        Err(_) => LocalPreviewState::Unavailable(
            "This image could not be safely decoded for a local preview.".into(),
        ),
    }
}

fn read_preview_bytes(mut file: fs::File, max_bytes: u64) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::with_capacity((max_bytes.min(64 * 1024)) as usize);
    file.by_ref()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!(
            "the file is larger than the {} preview limit",
            format_file_size(max_bytes)
        ));
    }
    Ok(bytes)
}

fn local_text_preview(bytes: Vec<u8>) -> LocalPreviewState {
    match String::from_utf8(bytes) {
        Ok(text) => LocalPreviewState::Text(text),
        Err(_) => LocalPreviewState::Unavailable(
            "This text preview requires a UTF-8 encoded file.".into(),
        ),
    }
}

fn format_file_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / KIB)
    } else {
        format!("{:.1} MB", bytes as f64 / (KIB * KIB))
    }
}

fn draw_local_preview(ui: &mut egui::Ui, file: &SelectedFile) {
    match &file.local_preview {
        LocalPreviewState::NotLoaded => {
            draw_unavailable_preview(ui, "Preparing the local preview…");
        }
        LocalPreviewState::Image { bytes, cache_key } => {
            let uri = format!(
                "bytes://teamspace/local/{cache_key:016x}/{}",
                file.local_path
                    .as_deref()
                    .map_or_else(|| file.name.clone(), |path| path.display().to_string()),
            );
            ui.add(
                egui::Image::from_bytes(uri, Arc::clone(bytes))
                    .max_width(360.0)
                    .max_height(260.0)
                    .corner_radius(theme::radius::MD),
            );
        }
        LocalPreviewState::Text(text) => {
            egui::Frame::new()
                .fill(theme::surface_subtle())
                .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
                .corner_radius(theme::radius::LG)
                .inner_margin(egui::Margin::symmetric(12, 10))
                .show(ui, |ui| {
                    ui.set_min_width(330.0);
                    egui::ScrollArea::vertical()
                        .max_height(280.0)
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new(text)
                                    .monospace()
                                    .size(font_size::CAPTION)
                                    .color(theme::fg1()),
                            );
                        });
                });
        }
        LocalPreviewState::Unavailable(message) | LocalPreviewState::Error(message) => {
            draw_unavailable_preview(ui, message);
        }
    }
}

fn draw_unavailable_preview(ui: &mut egui::Ui, message: &str) {
    egui::Frame::new()
        .fill(theme::surface())
        .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
        .corner_radius(theme::radius::LG)
        .inner_margin(egui::Margin::symmetric(14, 20))
        .show(ui, |ui| {
            ui.set_min_width(330.0);
            ui.label(
                RichText::new(message)
                    .font(theme::regular(font_size::CAPTION))
                    .color(theme::fg1()),
            );
        });
}

fn draw_document_preview(
    ui: &mut egui::Ui,
    kind: &str,
    title: &str,
    summary: &str,
    sections: &[(&str, &str)],
) {
    egui::Frame::new()
        .fill(theme::surface_subtle())
        .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
        .corner_radius(theme::radius::LG)
        .inner_margin(egui::Margin::symmetric(16, 14))
        .show(ui, |ui| {
            ui.set_min_width(330.0);
            ui.horizontal(|ui| {
                let (badge, _) = ui.allocate_exact_size(Vec2::splat(30.0), egui::Sense::hover());
                ui.painter().rect_filled(badge, 6.0, theme::brand_tint());
                ui.painter().text(
                    badge.center(),
                    egui::Align2::CENTER_CENTER,
                    badge_label(kind),
                    theme::semibold(font_size::CAPTION),
                    theme::brand_fg(),
                );
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(title)
                            .font(theme::semibold(font_size::BODY))
                            .color(theme::fg1()),
                    );
                    ui.label(
                        RichText::new("Local file preview")
                            .font(theme::regular(font_size::CAPTION))
                            .color(theme::fg3()),
                    );
                });
            });
            ui.add_space(12.0);
            ui.label(
                RichText::new(summary)
                    .font(theme::regular(font_size::CAPTION))
                    .color(theme::fg1()),
            );
            for (heading, body) in sections {
                ui.add_space(9.0);
                ui.label(
                    RichText::new(*heading)
                        .font(theme::semibold(font_size::CAPTION))
                        .color(theme::fg1()),
                );
                ui.label(
                    RichText::new(*body)
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg3()),
                );
            }
        });
}

fn draw_spreadsheet_preview(ui: &mut egui::Ui) {
    egui::Frame::new()
        .fill(theme::surface_subtle())
        .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
        .corner_radius(theme::radius::LG)
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_min_width(330.0);
            ui.label(
                RichText::new("Fitting schedule · local spreadsheet preview")
                    .font(theme::semibold(font_size::BODY))
                    .color(theme::fg1()),
            );
            ui.add_space(8.0);
            egui::Grid::new("fitting_schedule_preview")
                .num_columns(3)
                .spacing([16.0, 7.0])
                .striped(true)
                .show(ui, |ui| {
                    for label in ["Milestone", "Owner", "Status"] {
                        ui.label(
                            RichText::new(label)
                                .font(theme::semibold(font_size::CAPTION))
                                .color(theme::fg3()),
                        );
                    }
                    ui.end_row();
                    for row in [
                        ["Measure", "Tove", "Complete"],
                        ["Stain test", "Finish shop", "This week"],
                        ["Install", "Crew", "Planned"],
                    ] {
                        for cell in row {
                            ui.label(
                                RichText::new(cell)
                                    .font(theme::regular(font_size::CAPTION))
                                    .color(theme::fg1()),
                            );
                        }
                        ui.end_row();
                    }
                });
        });
}

fn file_matches(query: &str, name: &str, source: &str, kind: &str) -> bool {
    normalized_search_text(&format!("{name} {source} {kind}"))
        .contains(&normalized_search_text(query))
}

fn normalized_search_text(value: &str) -> String {
    let mut folded = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_alphanumeric() {
            folded.extend(character.to_lowercase());
        } else {
            folded.push(' ');
        }
    }
    folded
}

fn draw_file_row(
    ui: &mut egui::Ui,
    file: &str,
    where_from: &str,
    when: &str,
    kind: &str,
) -> egui::Response {
    let response = ui
        .horizontal(|ui| {
            let (icon, _) = ui.allocate_exact_size(Vec2::new(40.0, 40.0), egui::Sense::hover());
            ui.painter().rect_filled(icon, 6.0, theme::brand_tint());
            ui.painter().text(
                icon.center(),
                egui::Align2::CENTER_CENTER,
                badge_label(kind),
                theme::semibold(font_size::CAPTION),
                theme::brand_fg(),
            );
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(file)
                        .font(theme::semibold(font_size::BODY))
                        .color(theme::fg1()),
                );
                ui.label(
                    RichText::new(where_from)
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg3()),
                );
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(when)
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg3()),
                );
            });
        })
        .response;
    ui.separator();
    clickable(ui, response, &format!("{file}, {where_from}, {when}"))
}

#[cfg(test)]
mod tests;

/// First word of a file-kind label ("PNG image" -> "PNG") so it fits a badge.
fn badge_label(kind: &str) -> &str {
    kind.split_whitespace().next().unwrap_or(kind)
}
