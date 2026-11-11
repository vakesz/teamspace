use eframe::egui;

/// Returns whether this frame's unmodified Escape press has not already been
/// handled by another transient surface.
pub(crate) fn take_escape(ctx: &egui::Context) -> bool {
    ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
}

/// egui's `command` modifier is Cmd on macOS and Ctrl elsewhere; display
/// text must agree.
const fn modifier_label(macos: bool) -> &'static str {
    if macos { "⌘" } else { "Ctrl" }
}

fn format_chord(macos: bool, key: &str) -> String {
    if macos {
        format!("{}{key}", modifier_label(true))
    } else {
        format!("{}+{key}", modifier_label(false))
    }
}

/// Platform-aware text for the primary modifier plus `key`, such as `⌘E` or
/// `Ctrl+E`.
pub(crate) fn primary_chord(key: &str) -> String {
    format_chord(cfg!(target_os = "macos"), key)
}

/// Platform-aware submit chord, e.g. `Ctrl+Enter` or `⌘Enter`.
pub(crate) fn submit_chord() -> String {
    primary_chord("Enter")
}

pub(crate) fn multiline_submit_pressed(ui: &egui::Ui) -> bool {
    ui.input(|input| {
        is_multiline_submit_shortcut(input.modifiers, input.key_pressed(egui::Key::Enter))
    })
}

pub(crate) fn singleline_submit_pressed(response: &egui::Response, ui: &egui::Ui) -> bool {
    let lost_focus = response.lost_focus();
    ui.input(|input| is_singleline_submit(lost_focus, input.key_pressed(egui::Key::Enter)))
}

const fn is_singleline_submit(lost_focus: bool, enter_pressed: bool) -> bool {
    lost_focus && enter_pressed
}

pub(crate) const fn is_multiline_submit_shortcut(
    modifiers: egui::Modifiers,
    enter_pressed: bool,
) -> bool {
    enter_pressed && (modifiers.command || modifiers.ctrl)
}

/// A titled list of (keys, action) rows shown by the shortcuts dialog and by
/// Settings, so the two cannot drift apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ShortcutGroup {
    pub(crate) title: String,
    pub(crate) rows: Vec<(String, String)>,
}

/// Every app-level shortcut. `apps` are the rail destinations as
/// (key name, label) pairs, which the shell owns.
pub(crate) fn groups(apps: &[(String, String)]) -> Vec<ShortcutGroup> {
    vec![
        ShortcutGroup {
            title: "Apps".into(),
            rows: apps
                .iter()
                .map(|(key, label)| (primary_chord(key), label.clone()))
                .collect(),
        },
        ShortcutGroup {
            title: "Search and compose".into(),
            rows: [
                (primary_chord("E"), "Focus workspace search"),
                (primary_chord("K"), "Open the command palette"),
                (primary_chord(","), "Open settings"),
                (
                    format!("F5 or {}", primary_chord("R")),
                    "Refresh Microsoft 365 data",
                ),
                ("Enter".into(), "Send a chat message or activity reply"),
                (submit_chord(), "Create a chat or post to a channel"),
                (
                    "Esc".into(),
                    "Close the current local dialog or detail view",
                ),
            ]
            .into_iter()
            .map(|(keys, action)| (keys, action.to_owned()))
            .collect(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::{is_multiline_submit_shortcut, take_escape};
    use eframe::egui;

    #[test]
    fn chords_are_platform_aware() {
        assert_eq!(super::format_chord(true, "E"), "⌘E");
        assert_eq!(super::format_chord(false, "E"), "Ctrl+E");
    }

    /// Display text must come from the helpers above, never a literal.
    #[test]
    fn no_hard_coded_modifier_text_outside_this_module() {
        fn visit(dir: &std::path::Path, offenders: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    visit(&path, offenders);
                } else if path.extension().is_some_and(|e| e == "rs")
                    && !path.ends_with("ui/shortcuts.rs")
                {
                    let text = std::fs::read_to_string(&path).unwrap();
                    for (n, line) in text.lines().enumerate() {
                        if line.trim_start().starts_with("//") {
                            continue;
                        }
                        if line.contains('⌘')
                            || line.contains("Cmd/Ctrl")
                            || line.contains("Ctrl+")
                            || line.contains("Cmd+")
                        {
                            offenders.push(format!("{}:{}", path.display(), n + 1));
                        }
                    }
                }
            }
        }
        let mut offenders = Vec::new();
        visit(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut offenders,
        );
        assert!(
            offenders.is_empty(),
            "hard-coded modifier text: {offenders:?}"
        );
    }

    #[test]
    fn multiline_submit_requires_command_or_control_enter() {
        let command = egui::Modifiers {
            command: true,
            ..Default::default()
        };
        let control = egui::Modifiers {
            ctrl: true,
            ..Default::default()
        };

        assert!(is_multiline_submit_shortcut(command, true));
        assert!(is_multiline_submit_shortcut(control, true));
        assert!(!is_multiline_submit_shortcut(command, false));
        assert!(!is_multiline_submit_shortcut(
            egui::Modifiers::default(),
            true
        ));
    }

    #[test]
    fn singleline_submit_requires_an_enter_keypress_after_focus_leaves() {
        assert!(super::is_singleline_submit(true, true));
        assert!(!super::is_singleline_submit(true, false));
        assert!(!super::is_singleline_submit(false, true));
    }

    #[test]
    fn escape_is_consumed_by_only_one_transient_surface_per_frame() {
        let ctx = egui::Context::default();
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

        assert!(take_escape(&ctx));
        assert!(!take_escape(&ctx));

        let _ = ctx.end_pass();
    }
}
