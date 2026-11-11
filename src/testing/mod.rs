use eframe::egui;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Unique scratch directory under the system temp dir, removed on drop. All
/// persistence tests share this so none of them can touch a real profile.
pub(crate) struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    pub(crate) fn new(label: &str) -> Self {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock must be after the Unix epoch")
            .as_nanos();
        let sequence = DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "teamspace-{label}-test-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("test directory must be creatable");
        Self(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    /// `<dir>/<subdirectory>/<name>`; an empty subdirectory is skipped.
    pub(crate) fn file(&self, subdirectory: &str, name: &str) -> PathBuf {
        if subdirectory.is_empty() {
            self.0.join(name)
        } else {
            self.0.join(subdirectory).join(name)
        }
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(crate) fn render_headlessly(draw: impl FnOnce(&mut egui::Ui)) {
    let ctx = crate::ui::testing::themed_context();
    ctx.begin_pass(egui::RawInput::default());
    egui::CentralPanel::default().show(&ctx, draw);
    let _ = ctx.end_pass();
}

/// Renders a UI tree at a deliberate logical viewport size without creating a
/// native window. Use this for responsive layout branches that a default
/// headless context would otherwise never exercise.
pub(crate) fn render_at_size(size: egui::Vec2, draw: impl FnOnce(&mut egui::Ui)) {
    let ctx = crate::ui::testing::themed_context();
    ctx.begin_pass(egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
        ..Default::default()
    });
    egui::CentralPanel::default().show(&ctx, draw);
    let _ = ctx.end_pass();
}

pub(crate) fn with_escape_press(run: impl FnOnce(&egui::Context)) {
    let ctx = crate::ui::testing::themed_context();
    ctx.begin_pass(egui::RawInput {
        events: vec![egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }],
        ..Default::default()
    });
    run(&ctx);
    let _ = ctx.end_pass();
}
