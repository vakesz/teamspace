//! Renders the real `TeamsApp` through egui_kittest from isolated local-preview
//! state. These tests need no GPU: they assert on the accessibility tree.

use super::TeamsApp;
use crate::shared::model::Section;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

fn render(section: Section) -> Harness<'static, TeamsApp> {
    let mut harness = Harness::builder()
        .with_size(eframe::egui::vec2(1440.0, 900.0))
        .build_eframe(|cc| TeamsApp::create(cc, TeamsApp::local_preview(section)));
    harness.run();
    harness
}

/// Each section has a sidebar landmark the others never render, so the tests
/// prove the requested section is the one on screen (the rail labels are
/// always present and cannot distinguish them).
const LANDMARKS: [(Section, &str); 3] = [
    (Section::Chat, "Favorites"),
    (Section::Teams, "Teams and channels"),
    (Section::Calendar, "My calendars"),
];

fn assert_only_landmark_of(section: Section) {
    let harness = render(section);
    for (other, landmark) in LANDMARKS {
        let found = harness
            .query_all_by_label_contains(landmark)
            .next()
            .is_some();
        assert_eq!(
            found,
            other == section,
            "{section:?} preview: landmark `{landmark}` of {other:?} found={found}"
        );
    }
}

#[test]
fn chat_preview_renders_the_chat_surface() {
    assert_only_landmark_of(Section::Chat);
}

#[test]
fn teams_preview_renders_the_teams_surface() {
    assert_only_landmark_of(Section::Teams);
}

#[test]
fn calendar_preview_renders_the_calendar_surface() {
    assert_only_landmark_of(Section::Calendar);
}

fn focused_search(query: &str) -> Harness<'static, TeamsApp> {
    let mut harness = render(Section::Chat);
    harness.state_mut().search.query_mut().push_str(query);
    harness.state_mut().search.open();
    harness.run_steps(3);
    // The topbar search box is the first text input in the tree.
    harness
        .query_all_by_role(eframe::egui::accesskit::Role::TextInput)
        .next()
        .expect("search input")
        .focus();
    harness.run_steps(3);
    harness
}

#[test]
fn escape_in_the_global_search_box_clears_the_search() {
    let mut harness = focused_search("calendar");
    assert!(harness.state().search.is_open());

    harness.key_press(eframe::egui::Key::Escape);
    harness.run_steps(3);

    assert!(harness.state_mut().search.query_mut().is_empty());
}
