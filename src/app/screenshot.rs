use super::TeamsApp;
use crate::features::settings::SettingsSection;
use crate::shared::model::Section;
use crate::ui::theme::ThemePreference;
use egui_kittest::Harness;
use egui_kittest::wgpu::WgpuTestRenderer;
use std::path::Path;

const LOGICAL_SIZE: [f32; 2] = [1440.0, 900.0];
/// Forces the colour scheme of a screenshot: `light`, `dark` or `system`.
/// Honoured only by the screenshot renderer; the real app never reads it.
const THEME_VARIABLE: &str = "TEAMSPACE_THEME";
/// Opens one overlay before the frame is rendered: `settings` (or
/// `settings:<section>`), `palette` or `search`. Honoured only by the screenshot renderer.
const OVERLAY_VARIABLE: &str = "TEAMSPACE_OVERLAY";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Overlay {
    Settings(SettingsSection),
    Palette,
    Search,
}

fn parse_theme(value: Option<&str>) -> Result<ThemePreference, String> {
    match value {
        None | Some("" | "light") => Ok(ThemePreference::Light),
        Some("dark") => Ok(ThemePreference::Dark),
        Some("system") => Ok(ThemePreference::System),
        Some(other) => Err(format!(
            "{THEME_VARIABLE} must be light, dark or system, not {other:?}"
        )),
    }
}

fn parse_overlay(value: Option<&str>) -> Result<Option<Overlay>, String> {
    match value {
        None | Some("") => Ok(None),
        Some("settings" | "settings:appearance") => {
            Ok(Some(Overlay::Settings(SettingsSection::Appearance)))
        }
        Some("settings:notifications") => {
            Ok(Some(Overlay::Settings(SettingsSection::Notifications)))
        }
        Some("settings:microsoft365") => Ok(Some(Overlay::Settings(SettingsSection::Microsoft365))),
        Some("settings:privacy") => Ok(Some(Overlay::Settings(SettingsSection::Privacy))),
        Some("settings:shortcuts") => Ok(Some(Overlay::Settings(SettingsSection::Shortcuts))),
        Some("settings:about") => Ok(Some(Overlay::Settings(SettingsSection::About))),
        Some("palette") => Ok(Some(Overlay::Palette)),
        Some("search") => Ok(Some(Overlay::Search)),
        Some(other) => Err(format!(
            "{OVERLAY_VARIABLE} must be settings[:section], palette or search, not {other:?}"
        )),
    }
}

fn open_overlay(app: &mut TeamsApp, overlay: Overlay) {
    match overlay {
        Overlay::Settings(section) => app.settings.open_section(section),
        Overlay::Palette => app.palette.open(),
        Overlay::Search => {
            app.search.query_mut().push('a');
            app.search.open();
        }
    }
}

/// Builds the off-screen real-UI harness shared by the screenshot command and
/// the snapshot tests: 1440x900 logical points at 1.0 pixels per point,
/// rendered on the wgpu test renderer from isolated local-preview data (no
/// stored profile, no network). The frame is laid out and settled.
pub(super) fn harness(
    section: Section,
    theme: ThemePreference,
    overlay: Option<Overlay>,
) -> Harness<'static, TeamsApp> {
    let mut harness = Harness::builder()
        .with_size(eframe::egui::vec2(LOGICAL_SIZE[0], LOGICAL_SIZE[1]))
        .with_pixels_per_point(1.0)
        .renderer(WgpuTestRenderer::new())
        .build_eframe(move |cc| {
            let mut app = TeamsApp::local_preview(section);
            app.app_prefs.theme = theme;
            if let Some(overlay) = overlay {
                open_overlay(&mut app, overlay);
            }
            TeamsApp::create(cc, app)
        });
    if overlay.is_some() {
        // An open overlay keeps a caret or focus ring repainting, so `run`
        // would never settle; a few frames are enough to lay it out.
        harness.run_steps(6);
    } else {
        harness.run();
    }
    harness
}

/// Renders the real app for `section` off-screen and replaces `path` with the
/// resulting PNG.
pub(crate) fn render_screenshot(section: Section, path: &Path) -> Result<(), String> {
    let theme = parse_theme(std::env::var(THEME_VARIABLE).ok().as_deref())?;
    let overlay = parse_overlay(std::env::var(OVERLAY_VARIABLE).ok().as_deref())?;
    let mut harness = harness(section, theme, overlay);
    let image = harness.render()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    image.save(path).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_theme_variable_defaults_to_light_and_rejects_unknown_values() {
        assert_eq!(parse_theme(None), Ok(ThemePreference::Light));
        assert_eq!(parse_theme(Some("dark")), Ok(ThemePreference::Dark));
        assert_eq!(parse_theme(Some("system")), Ok(ThemePreference::System));
        assert!(parse_theme(Some("sepia")).is_err());
    }

    #[test]
    fn the_overlay_variable_names_one_overlay() {
        assert_eq!(parse_overlay(None), Ok(None));
        assert_eq!(parse_overlay(Some("palette")), Ok(Some(Overlay::Palette)));
        assert!(parse_overlay(Some("nope")).is_err());
    }
}
