//! Golden-image tests of the real UI, one per section and theme plus the
//! overlays. Goldens live in `tests/snapshots/sections/`; regenerate them with
//! `UPDATE_SNAPSHOTS=force cargo test --features ui-snapshots snapshot_tests`
//! on macOS, the only platform they are compared on.

use super::screenshot::{Overlay, harness};
use crate::features::settings::SettingsSection;
use crate::shared::model::Section;
use crate::ui::theme::ThemePreference;
use egui_kittest::{OsThreshold, SnapshotOptions};

const GOLDEN_DIRECTORY: &str = "tests/snapshots/sections";
/// A golden above this is almost certainly an accident (wrong size, noise).
const MAX_GOLDEN_BYTES: u64 = 400 * 1024;

fn options() -> SnapshotOptions {
    SnapshotOptions::new()
        .output_path(GOLDEN_DIRECTORY)
        .threshold(0.6_f32)
        .failed_pixel_count_threshold(OsThreshold::new(0).macos(64))
}

fn check(name: &str, section: Section, theme: ThemePreference, overlay: Option<Overlay>) {
    harness(section, theme, overlay).snapshot_options(name, &options());
}

macro_rules! section_snapshots {
    ($($light:ident, $dark:ident => $section:expr;)*) => {
        $(
            #[test]
            fn $light() {
                check(stringify!($light), $section, ThemePreference::Light, None);
            }

            #[test]
            fn $dark() {
                check(stringify!($dark), $section, ThemePreference::Dark, None);
            }
        )*
    };
}

section_snapshots! {
    chat_light, chat_dark => Section::Chat;
    teams_light, teams_dark => Section::Teams;
    calendar_light, calendar_dark => Section::Calendar;
    activity_light, activity_dark => Section::Activity;
    calls_light, calls_dark => Section::Calls;
    files_light, files_dark => Section::Files;
}

#[test]
fn settings_overlay_light() {
    check(
        "settings_overlay_light",
        Section::Chat,
        ThemePreference::Light,
        Some(Overlay::Settings(SettingsSection::Appearance)),
    );
}

#[test]
fn palette_overlay_light() {
    check(
        "palette_overlay_light",
        Section::Chat,
        ThemePreference::Light,
        Some(Overlay::Palette),
    );
}

#[test]
fn search_overlay_light() {
    check(
        "search_overlay_light",
        Section::Chat,
        ThemePreference::Light,
        Some(Overlay::Search),
    );
}

/// Runs after the goldens exist: every committed golden stays under the cap.
#[test]
fn every_golden_is_under_the_size_cap() {
    let Ok(entries) = std::fs::read_dir(GOLDEN_DIRECTORY) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.ends_with(".png")
            || [".new.png", ".diff.png", ".old.png"]
                .iter()
                .any(|s| name.ends_with(s))
        {
            continue;
        }
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        assert!(
            size <= MAX_GOLDEN_BYTES,
            "{name} is {size} bytes (cap {MAX_GOLDEN_BYTES})"
        );
    }
}
