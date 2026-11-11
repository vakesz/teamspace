use super::{
    FilePreviewKind, FileView, FilesState, LocalFilesStore, LocalPreviewState, RemoteFileItem,
    RemoteFileKind, RemoteFilePreview, RemoteFilePreviewContent, RemoteFilesDirectory,
    SelectedFile, StoredFiles, StoredLocalFile, UploadedFile, file_kind, format_file_size,
    has_uploaded_path, local_image_preview, local_preview_kind, local_text_preview, preview_kind,
    seeded_files,
};
use crate::features::search::workspace::{FileViewTarget, WorkspaceSearchTarget};
use crate::integrations::{ProviderBrowserUrl, ProviderResourceRef};
use crate::testing::{render_headlessly, with_escape_press};
use crate::ui::toast::ToastState;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};

static LOCAL_UPLOAD_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn safe_browser_url(value: &str) -> ProviderBrowserUrl {
    ProviderBrowserUrl::for_test(value)
}

struct TemporaryLocalUploadDirectory(PathBuf);

impl TemporaryLocalUploadDirectory {
    fn with_file(name: &str) -> (Self, PathBuf) {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock must be after the Unix epoch")
            .as_nanos();
        let sequence = LOCAL_UPLOAD_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "teamspace-files-state-test-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("test directory must be creatable");
        let path = directory.join(name);
        fs::write(&path, b"persisted local file").expect("test local file must write");
        (
            Self(directory),
            fs::canonicalize(path).expect("test local file must canonicalize"),
        )
    }
}

impl Drop for TemporaryLocalUploadDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn selected_sample_file() -> SelectedFile {
    SelectedFile::referenced(
        "layout-mockup.png".into(),
        "You · Local preview".into(),
        "Just now".into(),
        "PNG".into(),
    )
}

fn remote_files_directory() -> RemoteFilesDirectory {
    RemoteFilesDirectory::microsoft365(vec![RemoteFileItem::microsoft365(
        ProviderResourceRef::local_preview("test-onedrive-item"),
        "Roadmap.xlsx".into(),
        "2026-11-06T10:30:00Z".into(),
        Some(1_536),
        RemoteFileKind::File,
        "FILE".into(),
        Some(safe_browser_url(
            "https://contoso.sharepoint.com/sites/project/Roadmap.xlsx",
        )),
    )])
}

fn remote_file_item(target: ProviderResourceRef, kind: RemoteFileKind) -> RemoteFileItem {
    RemoteFileItem::microsoft365(
        target,
        "Previewable file.txt".into(),
        "2026-11-06T10:30:00Z".into(),
        Some(128),
        kind,
        "FILE".into(),
        None,
    )
}

#[test]
fn remote_onedrive_item_debug_redacts_its_browser_url() {
    let browser_destination = "https://contoso.sharepoint.com/:x:/r/opaque-debug-sentinel";
    let item = RemoteFileItem::microsoft365(
        ProviderResourceRef::local_preview("test-onedrive-debug-item"),
        "Roadmap.xlsx".into(),
        "2026-11-06T10:30:00Z".into(),
        Some(1_536),
        RemoteFileKind::File,
        "FILE".into(),
        Some(safe_browser_url(browser_destination)),
    );

    let debug = format!("{item:?}");
    assert!(debug.contains("has_web_url: true"));
    assert!(!debug.contains(browser_destination));
}

#[test]
fn remote_onedrive_metadata_is_session_only_and_clears_back_to_a_local_view() {
    let mut files = FilesState::default();
    assert!(files.add_bundled_example("layout-mockup.png"));
    files.select_file(selected_sample_file());
    files.mark_persisted();
    let stored_before = files.stored();

    files.replace_remote_files_directory(remote_files_directory());

    assert!(files.has_remote_files_directory());
    assert_eq!(files.remote_file_count(), 1);
    assert_eq!(files.stored(), stored_before);
    assert!(!files.is_persistence_dirty());

    assert!(files.select_one_drive_view_for_test());
    assert!(files.is_one_drive_view());
    assert!(files.selected_file.is_none());
    assert!(!files.picker_open);
    assert_eq!(files.stored(), stored_before);
    assert!(!files.is_persistence_dirty());

    files.clear_remote_files_directory();

    assert!(!files.has_remote_files_directory());
    assert_eq!(files.remote_file_count(), 0);
    assert!(!files.is_one_drive_view());
    assert_eq!(files.view, FileView::Recent);
    assert_eq!(files.stored(), stored_before);
    assert!(!files.is_persistence_dirty());
}

#[test]
fn remote_onedrive_access_denial_never_fabricates_a_local_file() {
    let mut files = FilesState::default();
    let stored_before = files.stored();

    files.replace_remote_files_directory(RemoteFilesDirectory::access_denied());

    assert!(files.has_remote_files_directory());
    assert!(files.remote_files_access_denied());
    assert_eq!(files.remote_file_count(), 0);
    assert!(files.select_one_drive_view_for_test());
    assert_eq!(files.stored(), stored_before);
    assert!(!files.is_persistence_dirty());
}

#[test]
fn remote_onedrive_metadata_panel_has_no_local_preview_or_upload_lifecycle() {
    let mut files = FilesState::default();
    files.replace_remote_files_directory(remote_files_directory());
    assert!(files.select_one_drive_view_for_test());
    let stored_before = files.stored();
    let mut toast = ToastState::default();

    render_headlessly(|ui| {
        files.draw_sidebar(ui);
        files.draw_page(ui, &mut toast);
    });

    assert!(files.is_one_drive_view());
    assert!(files.selected_file.is_none());
    assert!(!files.picker_open);
    assert!(files.take_remote_file_open_links().is_empty());
    assert_eq!(files.stored(), stored_before);
    assert!(!files.is_persistence_dirty());
}

#[test]
fn remote_onedrive_browser_link_is_explicit_session_only_and_target_bound() {
    let target = ProviderResourceRef::local_preview("remote-onedrive-open-current");
    let stale_target = ProviderResourceRef::local_preview("remote-onedrive-open-stale");
    let url = "https://contoso.sharepoint.com/sites/project/Roadmap.xlsx";
    let directory = RemoteFilesDirectory::microsoft365(vec![RemoteFileItem::microsoft365(
        target.clone(),
        "Roadmap.xlsx".into(),
        "2026-11-06T10:30:00Z".into(),
        Some(1_536),
        RemoteFileKind::File,
        "FILE".into(),
        Some(safe_browser_url(url)),
    )]);
    let mut files = FilesState::default();
    let stored_before = files.stored();
    files.replace_remote_files_directory(directory);

    // Only the exact opaque target rendered from the latest projection can
    // stage a browser handoff. A stale target must not substitute its name or
    // cause any URL to leave the session-only projection.
    assert!(!files.request_remote_file_open(stale_target));
    assert!(files.take_remote_file_open_links().is_empty());
    assert!(files.request_remote_file_open(target));
    assert_eq!(files.take_remote_file_open_links(), [safe_browser_url(url)]);
    assert!(files.take_remote_file_open_links().is_empty());
    assert_eq!(files.stored(), stored_before);
    assert!(!files.is_persistence_dirty());
}

#[test]
fn replaced_or_cleared_remote_onedrive_projection_drops_unopened_browser_links() {
    let first_target = ProviderResourceRef::local_preview("remote-onedrive-open-first");
    let second_target = ProviderResourceRef::local_preview("remote-onedrive-open-second");
    let first = RemoteFilesDirectory::microsoft365(vec![RemoteFileItem::microsoft365(
        first_target.clone(),
        "First.xlsx".into(),
        "2026-11-06T10:30:00Z".into(),
        None,
        RemoteFileKind::File,
        "FILE".into(),
        Some(safe_browser_url(
            "https://contoso.sharepoint.com/sites/project/First.xlsx",
        )),
    )]);
    let replacement = RemoteFilesDirectory::microsoft365(vec![RemoteFileItem::microsoft365(
        second_target,
        "Second.xlsx".into(),
        "2026-11-06T10:31:00Z".into(),
        None,
        RemoteFileKind::File,
        "FILE".into(),
        Some(safe_browser_url(
            "https://contoso.sharepoint.com/sites/project/Second.xlsx",
        )),
    )]);
    let mut files = FilesState::default();
    files.replace_remote_files_directory(first);
    assert!(files.request_remote_file_open(first_target));

    files.replace_remote_files_directory(replacement);
    assert!(files.take_remote_file_open_links().is_empty());

    files.clear_remote_files_directory();
    assert!(files.take_remote_file_open_links().is_empty());
}

#[test]
fn remote_onedrive_item_without_a_browser_link_cannot_stage_an_open() {
    let target = ProviderResourceRef::local_preview("remote-onedrive-no-browser-link");
    let mut files = FilesState::default();
    files.replace_remote_files_directory(RemoteFilesDirectory::microsoft365(vec![
        RemoteFileItem::microsoft365(
            target.clone(),
            "No link.txt".into(),
            "2026-11-06T10:30:00Z".into(),
            None,
            RemoteFileKind::File,
            "FILE".into(),
            None,
        ),
    ]));

    assert!(!files.request_remote_file_open(target));
    assert!(files.take_remote_file_open_links().is_empty());
}

#[test]
fn remote_onedrive_preview_requests_are_target_bound_and_file_only() {
    let file_target = ProviderResourceRef::local_preview("remote-preview-file");
    let folder_target = ProviderResourceRef::local_preview("remote-preview-folder");
    let unknown_target = ProviderResourceRef::local_preview("remote-preview-unknown");
    let stale_target = ProviderResourceRef::local_preview("remote-preview-stale");
    let mut files = FilesState::default();
    let stored_before = files.stored();
    files.replace_remote_files_directory(RemoteFilesDirectory::microsoft365(vec![
        remote_file_item(file_target.clone(), RemoteFileKind::File),
        remote_file_item(folder_target.clone(), RemoteFileKind::Folder),
        remote_file_item(unknown_target.clone(), RemoteFileKind::Unknown),
    ]));

    assert!(!files.request_remote_file_preview_for_test(stale_target));
    assert!(!files.request_remote_file_preview_for_test(folder_target));
    assert!(!files.request_remote_file_preview_for_test(unknown_target));
    assert!(files.take_remote_file_preview_request().is_none());

    assert!(files.request_remote_file_preview_for_test(file_target.clone()));
    assert!(!files.request_remote_file_preview_for_test(file_target.clone()));
    assert_eq!(
        files.take_remote_file_preview_request(),
        Some(file_target.clone())
    );
    assert!(files.remote_file_preview_is_loading_for_test());
    assert!(!files.take_remote_file_preview_cancellation_requested());
    assert_eq!(files.stored(), stored_before);
    assert!(!files.is_persistence_dirty());
}

#[test]
fn remote_onedrive_preview_accepts_only_its_current_loading_file() {
    let file_target = ProviderResourceRef::local_preview("remote-preview-current");
    let folder_target = ProviderResourceRef::local_preview("remote-preview-folder-result");
    let stale_target = ProviderResourceRef::local_preview("remote-preview-stale-result");
    let mut files = FilesState::default();
    let stored_before = files.stored();
    files.replace_remote_files_directory(RemoteFilesDirectory::microsoft365(vec![
        remote_file_item(file_target.clone(), RemoteFileKind::File),
        remote_file_item(folder_target.clone(), RemoteFileKind::Folder),
    ]));
    assert!(files.request_remote_file_preview_for_test(file_target.clone()));
    let _ = files.take_remote_file_preview_request();

    assert!(
        !files.apply_remote_file_preview(RemoteFilePreview::microsoft365(
            stale_target,
            RemoteFilePreviewContent::Text("stale text must not render".into()),
        ))
    );
    assert!(
        !files.apply_remote_file_preview(RemoteFilePreview::microsoft365(
            folder_target,
            RemoteFilePreviewContent::Text("folder content must not render".into()),
        ))
    );
    assert!(files.remote_file_preview_is_loading_for_test());

    assert!(
        files.apply_remote_file_preview(RemoteFilePreview::microsoft365(
            file_target,
            RemoteFilePreviewContent::Text("A safe session-only preview.".into()),
        ))
    );
    assert!(files.remote_file_preview_is_ready_for_test());
    assert!(files.has_keyboard_modal());
    assert_eq!(files.stored(), stored_before);
    assert!(!files.is_persistence_dirty());
}

#[test]
fn replaced_or_cleared_remote_onedrive_projection_cancels_loading_preview() {
    let first_target = ProviderResourceRef::local_preview("remote-preview-first");
    let second_target = ProviderResourceRef::local_preview("remote-preview-second");
    let mut files = FilesState::default();
    let stored_before = files.stored();
    files.replace_remote_files_directory(RemoteFilesDirectory::microsoft365(vec![
        remote_file_item(first_target.clone(), RemoteFileKind::File),
    ]));
    assert!(files.request_remote_file_preview_for_test(first_target.clone()));
    assert_eq!(
        files.take_remote_file_preview_request(),
        Some(first_target.clone())
    );

    files.replace_remote_files_directory(RemoteFilesDirectory::microsoft365(vec![
        remote_file_item(second_target.clone(), RemoteFileKind::File),
    ]));
    assert!(!files.has_keyboard_modal());
    assert!(files.take_remote_file_preview_cancellation_requested());
    assert!(!files.take_remote_file_preview_cancellation_requested());
    assert!(files.take_remote_file_preview_request().is_none());
    assert!(
        !files.apply_remote_file_preview(RemoteFilePreview::microsoft365(
            first_target,
            RemoteFilePreviewContent::Text("late response".into()),
        ))
    );

    assert!(files.request_remote_file_preview_for_test(second_target));
    let _ = files.take_remote_file_preview_request();
    files.clear_remote_files_directory();
    assert!(files.take_remote_file_preview_cancellation_requested());
    assert!(!files.has_keyboard_modal());
    assert_eq!(files.stored(), stored_before);
    assert!(!files.is_persistence_dirty());
}

#[test]
fn remote_onedrive_preview_debug_redacts_text_and_image_bytes() {
    let target = ProviderResourceRef::local_preview("remote-preview-debug");
    let text = "private preview text";
    let preview = RemoteFilePreview::microsoft365(
        target.clone(),
        RemoteFilePreviewContent::Text(text.into()),
    );
    let image: Arc<[u8]> = Arc::from(&b"private image bytes"[..]);
    let image_preview = RemoteFilePreview::microsoft365(
        target,
        RemoteFilePreviewContent::Image {
            bytes: image,
            cache_key: 9,
        },
    );

    let text_debug = format!("{preview:?}");
    let image_debug = format!("{image_preview:?}");
    assert!(text_debug.contains("Text(<redacted>)"));
    assert!(!text_debug.contains(text));
    assert!(image_debug.contains("byte_len"));
    assert!(!image_debug.contains("private image bytes"));
}

#[test]
fn dismissed_replaced_or_cleared_remote_images_are_evicted_from_egui() {
    // A valid, tiny image makes the test exercise egui's bytes/image/texture
    // loader path rather than only the feature's in-memory state transition.
    const ONE_PIXEL_PNG: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 8, 215, 99, 248, 207, 192, 240, 31,
        0, 5, 0, 1, 255, 137, 153, 61, 29, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];
    let target = ProviderResourceRef::local_preview("remote-preview-image-eviction");
    let mut files = FilesState::default();
    files.replace_remote_files_directory(RemoteFilesDirectory::microsoft365(vec![
        remote_file_item(target.clone(), RemoteFileKind::File),
    ]));
    let ctx = crate::ui::testing::themed_context();

    let load_image = |files: &mut FilesState, cache_key| {
        assert!(files.request_remote_file_preview_for_test(target.clone()));
        let _ = files.take_remote_file_preview_request();
        assert!(
            files.apply_remote_file_preview(RemoteFilePreview::microsoft365(
                target.clone(),
                RemoteFilePreviewContent::Image {
                    bytes: Arc::from(ONE_PIXEL_PNG),
                    cache_key,
                },
            ))
        );
        crate::ui::testing::run_ui(&ctx, |ui| files.remote_files.draw_preview_window(ui.ctx()));
    };

    let first_key = 0x9f01_u64;
    let first_uri = format!("bytes://teamspace/microsoft365-preview/{first_key:016x}");
    load_image(&mut files, first_key);
    assert!(ctx.try_load_bytes(&first_uri).is_ok());
    files.dismiss_contextual_overlays();
    files.forget_discarded_remote_preview_images(&ctx);
    assert!(ctx.try_load_bytes(&first_uri).is_err());

    let second_key = 0x9f02_u64;
    let second_uri = format!("bytes://teamspace/microsoft365-preview/{second_key:016x}");
    load_image(&mut files, second_key);
    assert!(ctx.try_load_bytes(&second_uri).is_ok());
    files.clear_remote_files_directory();
    files.forget_discarded_remote_preview_images(&ctx);
    assert!(ctx.try_load_bytes(&second_uri).is_err());

    files.replace_remote_files_directory(RemoteFilesDirectory::microsoft365(vec![
        remote_file_item(target.clone(), RemoteFileKind::File),
    ]));
    let third_key = 0x9f03_u64;
    let third_uri = format!("bytes://teamspace/microsoft365-preview/{third_key:016x}");
    load_image(&mut files, third_key);
    assert!(ctx.try_load_bytes(&third_uri).is_ok());
    files.replace_remote_files_directory(RemoteFilesDirectory::microsoft365(vec![
        remote_file_item(target, RemoteFileKind::File),
    ]));
    files.forget_discarded_remote_preview_images(&ctx);
    assert!(ctx.try_load_bytes(&third_uri).is_err());
}

#[test]
fn escape_closes_a_loading_remote_preview_and_requests_cancellation() {
    let target = ProviderResourceRef::local_preview("remote-preview-escape");
    let mut files = FilesState::default();
    files.replace_remote_files_directory(RemoteFilesDirectory::microsoft365(vec![
        remote_file_item(target, RemoteFileKind::File),
    ]));
    assert!(
        files.request_remote_file_preview_for_test(ProviderResourceRef::local_preview(
            "remote-preview-escape",
        ))
    );
    let _ = files.take_remote_file_preview_request();

    with_escape_press(|ctx| files.remote_files.draw_preview_window(ctx));

    assert!(!files.has_keyboard_modal());
    assert!(files.take_remote_file_preview_cancellation_requested());
}

#[test]
fn preview_file_upload_is_session_local_and_deduplicated() {
    let mut files = FilesState::default();

    assert!(files.add_bundled_example("layout-mockup.png"));
    assert!(!files.add_bundled_example("layout-mockup.png"));
    assert!(!files.add_bundled_example("unregistered-preview.png"));
    assert_eq!(files.uploaded_files.len(), 1);
    assert_eq!(files.uploaded_files[0].view, FileView::Recent);
    assert_eq!(files.uploaded_files[0].kind, "PNG image");
    assert_eq!(
        files.uploaded_files[0].preview_kind,
        FilePreviewKind::BundledImage
    );
}

#[test]
fn selected_file_keeps_its_local_metadata() {
    let file = selected_sample_file();
    let files = FilesState {
        selected_file: Some(file.clone()),
        ..Default::default()
    };

    assert_eq!(files.selected_file, Some(file));
}

#[test]
fn every_seeded_file_has_a_local_preview() {
    for file in seeded_files() {
        assert_ne!(
            preview_kind(file.name),
            FilePreviewKind::Unavailable,
            "{} should not open an unavailable placeholder",
            file.name
        );
    }
}

#[test]
fn search_finds_seeded_and_uploaded_files() {
    let mut files = FilesState::default();
    assert!(files.add_bundled_example("layout-mockup.png"));

    let seeded = files.search_results("fitting");
    assert!(matches!(
        seeded[0].target,
        crate::features::search::workspace::WorkspaceSearchTarget::File { .. }
    ));
    let uploaded = files.search_results("layout mockup");
    assert_eq!(uploaded.len(), 2);
    assert!(uploaded.iter().any(|result| {
        matches!(
            result.target,
            WorkspaceSearchTarget::File {
                view: FileViewTarget::Recent,
                ..
            }
        )
    }));
    assert!(uploaded.iter().any(|result| {
        matches!(
            result.target,
            WorkspaceSearchTarget::File {
                view: FileViewTarget::Shared,
                ..
            }
        )
    }));

    files.select_search_result(
        "layout-mockup.png".into(),
        "You · Local preview".into(),
        "Just now".into(),
        "PNG".into(),
        FileViewTarget::Recent,
    );
    assert_eq!(
        files.selected_file.as_ref().map(|file| file.name.as_str()),
        Some("layout-mockup.png")
    );

    files.select_search_result(
        "Walkthrough notes.docx".into(),
        "Spring showroom · General".into(),
        "Oct 23".into(),
        "DOC".into(),
        FileViewTarget::Downloads,
    );
    assert_eq!(files.view, FileView::Downloads);
}

#[test]
fn bundled_shared_files_search_and_open_in_the_shared_view() {
    let mut files = FilesState::default();
    let result = files
        .search_results("supply costs")
        .into_iter()
        .find(|result| result.label == "supply-costs.png")
        .expect("bundled shared file search result");

    let WorkspaceSearchTarget::File {
        name,
        source,
        when,
        kind,
        view,
    } = result.target
    else {
        panic!("shared file result should route to Files");
    };
    assert_eq!(view, FileViewTarget::Shared);
    files.select_search_result(name, source, when, kind, view);

    assert_eq!(files.view, FileView::Shared);
    assert_eq!(
        files.selected_file.as_ref().map(|file| file.name.as_str()),
        Some("supply-costs.png")
    );
    assert_eq!(
        files.selected_file.as_ref().map(|file| file.preview_kind),
        Some(FilePreviewKind::BundledImage)
    );
}

#[test]
fn opening_a_file_picker_dismisses_the_existing_preview() {
    let mut files = FilesState {
        selected_file: Some(selected_sample_file()),
        ..Default::default()
    };

    files.open_picker();

    assert!(files.picker_open);
    assert!(files.selected_file.is_none());
}

#[test]
fn dismissing_contextual_overlays_keeps_uploaded_files() {
    let mut files = FilesState {
        picker_open: true,
        selected_file: Some(selected_sample_file()),
        ..Default::default()
    };
    assert!(files.add_bundled_example("supply-costs.png"));

    files.dismiss_contextual_overlays();

    assert!(!files.picker_open);
    assert!(files.selected_file.is_none());
    assert_eq!(files.uploaded_files.len(), 1);
}

#[test]
fn escape_closes_file_dialogs_without_removing_uploaded_files() {
    let mut files = FilesState::default();
    files.open_picker();
    assert!(files.add_bundled_example("supply-costs.png"));
    let mut toast = ToastState::default();

    with_escape_press(|ctx| files.draw_picker(ctx, &mut toast));
    assert!(!files.picker_open);
    assert_eq!(files.uploaded_files.len(), 1);

    files.select_file(selected_sample_file());
    with_escape_press(|ctx| files.draw_selected_file_window(ctx, &mut toast));
    assert!(files.selected_file.is_none());
    assert_eq!(files.uploaded_files.len(), 1);
}

#[test]
fn file_dialogs_claim_keyboard_priority() {
    let mut files = FilesState::default();

    assert!(!files.has_keyboard_modal());
    files.open_picker();
    assert!(files.has_keyboard_modal());
    files.dismiss_contextual_overlays();
    assert!(!files.has_keyboard_modal());

    files.select_file(selected_sample_file());
    assert!(files.has_keyboard_modal());
}

#[test]
fn local_file_types_and_safe_preview_modes_are_extension_based() {
    let image = PathBuf::from("/tmp/reference.JpEg");
    let text = PathBuf::from("/tmp/notes.MD");
    let other = PathBuf::from("/tmp/archive.zip");

    assert_eq!(file_kind(&image), "JPEG");
    assert_eq!(local_preview_kind(&image), FilePreviewKind::LocalImage);
    assert_eq!(local_preview_kind(&text), FilePreviewKind::LocalText);
    assert_eq!(local_preview_kind(&other), FilePreviewKind::Unavailable);
}

#[test]
fn local_file_metadata_retains_path_size_and_source() {
    let path = PathBuf::from("/tmp/design notes.txt");
    let file = UploadedFile::local(path.clone(), 1_536);

    assert_eq!(file.name, "design notes.txt");
    assert_eq!(file.local_path.as_deref(), Some(path.as_path()));
    assert_eq!(file.size_bytes, Some(1_536));
    assert_eq!(file.source_label(), "You · /tmp/design notes.txt");
    assert_eq!(format_file_size(1_536), "1.5 KB");
}

#[test]
fn local_upload_projection_restores_existing_files_without_bundled_entries() {
    let (_directory, path) = TemporaryLocalUploadDirectory::with_file("remembered.txt");
    let mut files = FilesState::default();

    assert!(matches!(
        files.add_local_file(path.clone()),
        Ok(super::LocalFileAdd::Added(name)) if name == "remembered.txt"
    ));
    assert!(files.add_bundled_example("layout-mockup.png"));
    assert!(files.is_persistence_dirty());

    let stored = files.stored();
    assert_eq!(stored.local_uploads.len(), 1);
    assert_eq!(stored.local_uploads[0].path, path.to_string_lossy());

    files.mark_persisted();
    assert!(!files.is_persistence_dirty());
    let restored = FilesState::from_stored(stored);
    assert_eq!(restored.uploaded_files.len(), 1);
    assert_eq!(
        restored.uploaded_files[0].local_path.as_deref(),
        Some(path.as_path())
    );
    assert!(!restored.is_persistence_dirty());
    assert!(restored.selected_file.is_none());
}

#[test]
fn restoring_a_missing_local_upload_skips_the_stale_path() {
    let (directory, path) = TemporaryLocalUploadDirectory::with_file("removed.txt");
    let stored = StoredFiles {
        schema_version: super::FILES_SCHEMA_VERSION,
        local_uploads: vec![
            StoredLocalFile::from_local_path(&path).expect("test path must be persistable"),
        ],
    };
    fs::remove_file(&path).expect("test local file must remove");

    let restored = FilesState::from_stored(stored);

    assert!(restored.uploaded_files.is_empty());
    assert!(restored.is_persistence_dirty());
    assert!(restored.stored().local_uploads.is_empty());
    drop(directory);
}

#[test]
fn restoring_stale_paths_repairs_only_local_metadata_with_an_atomic_snapshot() {
    let (directory, retained_path) = TemporaryLocalUploadDirectory::with_file("keep.txt");
    let stale_path = directory.0.join("removed.txt");
    fs::write(&stale_path, b"remove this metadata only").expect("stale fixture must write");
    let stale_path = fs::canonicalize(&stale_path).expect("stale fixture must canonicalize");
    let store = LocalFilesStore::at(directory.0.join("profile").join("files-v1.json"));
    let snapshot = StoredFiles {
        schema_version: super::FILES_SCHEMA_VERSION,
        local_uploads: vec![
            StoredLocalFile::from_local_path(&retained_path)
                .expect("retained fixture must be persistable"),
            StoredLocalFile::from_local_path(&stale_path)
                .expect("stale fixture must be persistable"),
        ],
    };
    store
        .save(&snapshot)
        .expect("initial Files metadata must save");
    fs::remove_file(&stale_path).expect("stale source must be removable");

    let loaded = store
        .load()
        .expect("structurally valid metadata must load")
        .expect("saved metadata must exist");
    let mut restored = FilesState::from_stored(loaded);

    assert_eq!(restored.uploaded_files.len(), 1);
    assert_eq!(
        restored.uploaded_files[0].local_path.as_deref(),
        Some(retained_path.as_path())
    );
    assert!(restored.is_persistence_dirty());
    assert_eq!(restored.stored().local_uploads.len(), 1);
    assert!(retained_path.exists());
    assert!(
        !stale_path.exists(),
        "reconciliation must not recreate or otherwise modify the source"
    );

    // The usual debounced lifecycle saves this exact reduced projection.
    // `LocalFilesStore::save` atomically replaces the old snapshot, so a
    // future restore cannot resurrect the unavailable metadata.
    store
        .save(&restored.stored())
        .expect("repaired metadata must save atomically");
    restored.mark_persisted();
    assert!(!restored.is_persistence_dirty());
    assert_eq!(
        store
            .load()
            .expect("repaired metadata must load")
            .expect("repaired metadata must exist"),
        restored.stored()
    );
    assert_eq!(
        store
            .load()
            .expect("repaired metadata must load")
            .expect("repaired metadata must exist")
            .local_uploads,
        vec![
            StoredLocalFile::from_local_path(&retained_path)
                .expect("retained fixture must remain persistable"),
        ]
    );
}

#[cfg(unix)]
#[test]
fn restoring_a_symlink_replacement_skips_the_original_local_path() {
    let (_directory, path) = TemporaryLocalUploadDirectory::with_file("remembered.txt");
    let mut files = FilesState::default();
    assert!(matches!(
        files.add_local_file(path.clone()),
        Ok(super::LocalFileAdd::Added(_))
    ));
    let stored = files.stored();
    let replacement = path
        .parent()
        .expect("test path has a parent")
        .join("replacement.txt");
    fs::write(&replacement, b"different local file").expect("replacement must write");
    fs::remove_file(&path).expect("original fixture must be replaceable");
    symlink(&replacement, &path).expect("symlink replacement must be creatable");

    let restored = FilesState::from_stored(stored);

    assert!(restored.uploaded_files.is_empty());
    assert!(restored.is_persistence_dirty());
    assert!(restored.stored().local_uploads.is_empty());
}

#[test]
fn forgetting_a_local_file_removes_only_its_metadata_and_clears_selection() {
    let (_directory, path) =
        TemporaryLocalUploadDirectory::with_file("forget-this-local-entry.txt");
    let mut files = FilesState::default();
    assert!(matches!(
        files.add_local_file(path.clone()),
        Ok(super::LocalFileAdd::Added(_))
    ));
    assert!(files.add_bundled_example("layout-mockup.png"));
    let selected = files
        .uploaded_files
        .iter()
        .find(|file| file.local_path.as_deref() == Some(path.as_path()))
        .expect("local fixture must be present")
        .selected_file();
    files.select_file(selected);
    files.mark_persisted();

    assert_eq!(
        files.forget_selected_local_file(),
        Some("forget-this-local-entry.txt".into())
    );

    assert!(path.exists(), "forgetting must not delete the source file");
    assert!(files.selected_file.is_none());
    assert!(files.is_persistence_dirty());
    assert!(files.search_results("forget-this-local-entry").is_empty());
    assert!(files.stored().local_uploads.is_empty());
    assert_eq!(files.uploaded_files.len(), 1);
    assert_eq!(files.uploaded_files[0].local_path, None);
    assert_eq!(files.uploaded_files[0].name, "layout-mockup.png");
}

#[test]
fn stale_local_search_result_does_not_open_a_fabricated_reference_preview() {
    let (_directory, path) = TemporaryLocalUploadDirectory::with_file("stale-search.txt");
    let mut files = FilesState::default();
    assert!(matches!(
        files.add_local_file(path.clone()),
        Ok(super::LocalFileAdd::Added(_))
    ));
    let result = files
        .search_results("stale-search")
        .into_iter()
        .next()
        .expect("local file must be searchable");
    let WorkspaceSearchTarget::File {
        name,
        source,
        when,
        kind,
        view,
    } = result.target
    else {
        panic!("local search result must target Files");
    };

    assert_eq!(
        files.forget_local_file(&path),
        Some("stale-search.txt".into())
    );
    files.select_search_result(name, source, when, kind, view);

    assert!(files.selected_file.is_none());
}

#[cfg(unix)]
#[test]
fn local_preview_rejects_a_symlink_replacement_before_reading_it() {
    let (_directory, path) = TemporaryLocalUploadDirectory::with_file("preview.txt");
    let mut files = FilesState::default();
    assert!(matches!(
        files.add_local_file(path.clone()),
        Ok(super::LocalFileAdd::Added(_))
    ));
    let mut selected = files.uploaded_files[0].selected_file();
    let replacement = path
        .parent()
        .expect("test path has a parent")
        .join("preview-replacement.txt");
    fs::write(&replacement, b"do not preview this replacement").expect("replacement must write");
    fs::remove_file(&path).expect("original fixture must be replaceable");
    symlink(&replacement, &path).expect("symlink replacement must be creatable");

    let error = selected
        .prepare_local_preview()
        .expect("replacement must be rejected before reading");

    assert!(error.contains("Could not preview preview.txt"));
    assert!(matches!(
        selected.local_preview,
        LocalPreviewState::Error(_)
    ));
}

#[test]
fn reference_files_cannot_be_forgotten_from_local_files() {
    let mut files = FilesState::default();
    assert!(files.add_bundled_example("layout-mockup.png"));
    files.select_file(files.uploaded_files[0].selected_file());

    assert_eq!(files.forget_selected_local_file(), None);
    assert_eq!(files.uploaded_files.len(), 1);
    assert!(files.selected_file.is_some());
    assert!(!files.is_persistence_dirty());
}

#[test]
fn duplicate_local_paths_are_detected_without_blocking_same_named_files() {
    let mut files = FilesState::default();
    let path = PathBuf::from("/tmp/first/report.txt");
    files
        .uploaded_files
        .push(UploadedFile::local(path.clone(), 10));
    let second_path = PathBuf::from("/tmp/second/report.txt");

    assert!(has_uploaded_path(&files.uploaded_files, &path));
    assert!(!has_uploaded_path(&files.uploaded_files, &second_path));
    assert_eq!(
        UploadedFile::local(second_path, 10).name,
        files.uploaded_files[0].name
    );
}

#[test]
fn text_preview_accepts_utf8_and_rejects_invalid_bytes() {
    assert_eq!(
        local_text_preview(b"A small local preview".to_vec()),
        LocalPreviewState::Text("A small local preview".into())
    );
    assert!(matches!(
        local_text_preview(vec![0xff, 0xfe]),
        LocalPreviewState::Unavailable(_)
    ));
    assert!(matches!(
        local_image_preview(b"not an image".to_vec()),
        LocalPreviewState::Unavailable(_)
    ));
}

#[test]
fn local_preview_open_errors_keep_their_user_visible_text() {
    use super::{LocalPreviewOpenError, open_selected_local_preview_file};

    assert_eq!(
        LocalPreviewOpenError::PathChanged.to_string(),
        "the selected path no longer resolves to the originally selected file"
    );
    assert_eq!(
        LocalPreviewOpenError::NotRegularFile.to_string(),
        "the selected path is no longer a regular file"
    );

    let missing = std::env::temp_dir().join("teamspace-r3-missing-preview-file");
    let _ = fs::remove_file(&missing);
    let io_error = fs::canonicalize(&missing).expect_err("the path must not exist");
    let expected = io_error.to_string();
    let error =
        open_selected_local_preview_file(&missing).expect_err("a missing file must not open");
    assert!(matches!(error, LocalPreviewOpenError::Io(_)));
    assert_eq!(error.to_string(), expected);

    let directory = fs::canonicalize(std::env::temp_dir()).expect("temp dir must resolve");
    assert!(matches!(
        open_selected_local_preview_file(&directory),
        Err(LocalPreviewOpenError::NotRegularFile)
    ));
}

#[test]
fn local_preview_open_returns_the_checked_regular_file() {
    use super::{LocalPreviewOpenError, open_selected_local_preview_file, same_file};
    use std::io::Read;

    let directory = fs::canonicalize(std::env::temp_dir()).expect("temp dir must resolve");
    let path = directory.join(format!("teamspace-r9-preview-{}.txt", std::process::id()));
    fs::write(&path, b"checked contents").expect("fixture must be writable");
    let mut opened = open_selected_local_preview_file(&path).expect("a regular file must open");
    let mut contents = String::new();
    opened
        .read_to_string(&mut contents)
        .expect("opened fixture must be readable");
    assert_eq!(contents, "checked contents");

    let other = directory.join(format!("teamspace-r9-other-{}.txt", std::process::id()));
    fs::write(&other, b"other").expect("fixture must be writable");
    let checked = fs::metadata(&path).expect("metadata");
    assert!(same_file(&checked, &opened.metadata().expect("metadata")));
    #[cfg(unix)]
    assert!(!same_file(
        &checked,
        &fs::metadata(&other).expect("metadata")
    ));

    // A non-canonical spelling of the same file is still refused. Windows
    // canonicalizes to a `\\?\` path, where joining `..` pops a component
    // instead of spelling it, so the alias starts from the plain form.
    let plain_directory = PathBuf::from(directory.to_string_lossy().trim_start_matches(r"\\?\"));
    let alias = plain_directory
        .join("..")
        .join(directory.file_name().expect("temp dir name"))
        .join(path.file_name().expect("file name"));
    assert!(matches!(
        open_selected_local_preview_file(&alias),
        Err(LocalPreviewOpenError::PathChanged)
    ));
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(&other);
}

#[test]
fn file_search_folds_case_for_non_ascii_names() {
    assert!(super::file_matches(
        "árvíz",
        "ÁRVÍZTŰRŐ.docx",
        "OneDrive",
        "Word"
    ));
    assert!(super::file_matches(
        "ÉVES",
        "éves-jelentés.pdf",
        "Local",
        "PDF"
    ));
    assert!(!super::file_matches("zzz", "éves.pdf", "Local", "PDF"));
}
