use crate::app::workspace_coordinator::WorkspaceState;
use crate::features::chat::{ChatSearchTarget, ChatState};
use crate::features::search::remote::{RemoteMessageSearchState, RemoteSearchPhase};
use crate::features::search::workspace::WorkspaceSearchTarget;
use crate::integrations::{RemoteMessageSearchProjection, RemoteSearchLocation, RemoteSearchQuery};
use crate::shared::local_time;
use crate::shared::model::Section;
use crate::ui::a11y;
use crate::ui::shortcuts::take_escape;
use crate::ui::text_match::{self, MatchRank};
use crate::ui::theme;
use eframe::egui::{self, Color32, RichText, Stroke};

#[derive(Clone)]
pub(crate) enum SearchAction {
    Navigate(Section),
    OpenChat(ChatSearchTarget),
    OpenWorkspace(WorkspaceSearchTarget),
    /// Search Microsoft 365 messages for this query. It keeps the overlay
    /// open: the results arrive in its Microsoft 365 group.
    RemoteSearch(String),
    /// Read the next page of the Microsoft 365 message search in progress.
    RemoteSearchMore,
    /// Open one Microsoft 365 message hit by its opaque target.
    OpenRemoteHit(RemoteSearchLocation),
}

impl SearchAction {
    /// Whether the overlay stays open after this action.
    const fn keeps_overlay_open(&self) -> bool {
        matches!(self, Self::RemoteSearch(_) | Self::RemoteSearchMore)
    }
}

/// Result groups in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SearchSection {
    Apps,
    Chat,
    Teams,
    Calendar,
    Calls,
    Files,
    Activity,
    /// Trailing group: the "Search Microsoft 365 messages" row and its hits.
    Remote,
}

impl SearchSection {
    const fn title(self) -> &'static str {
        match self {
            Self::Apps => "Apps",
            Self::Chat => "Chat",
            Self::Teams => "Teams",
            Self::Calendar => "Calendar",
            Self::Calls => "Calls",
            Self::Files => "Files",
            Self::Activity => "Activity",
            Self::Remote => "Microsoft 365",
        }
    }

    const fn for_workspace(target: &WorkspaceSearchTarget) -> Self {
        match target {
            WorkspaceSearchTarget::Activity(_) => Self::Activity,
            WorkspaceSearchTarget::TeamChannel(_) | WorkspaceSearchTarget::TeamPost { .. } => {
                Self::Teams
            }
            WorkspaceSearchTarget::CalendarEvent { .. } => Self::Calendar,
            WorkspaceSearchTarget::File { .. } => Self::Files,
            WorkspaceSearchTarget::CallContact(_) => Self::Calls,
        }
    }
}
#[derive(Default)]
pub(crate) struct SearchState {
    query: String,
    open: bool,
    selected_result: usize,
    remote: RemoteMessageSearchState,
}

impl SearchState {
    pub(crate) fn query_mut(&mut self) -> &mut String {
        &mut self.query
    }

    pub(crate) fn open(&mut self) {
        self.open = true;
        self.selected_result = 0;
    }

    pub(crate) fn dismiss(&mut self) {
        self.clear();
    }

    /// Starts the Microsoft 365 message search the user asked for.
    pub(crate) fn start_remote_search(&mut self, text: &str) {
        let _ = self.remote.start(text);
    }

    pub(crate) fn request_more_remote_results(&mut self) {
        let _ = self.remote.request_more();
    }

    pub(crate) fn apply_remote_projection(&mut self, projection: RemoteMessageSearchProjection) {
        let _ = self.remote.apply(projection);
    }

    pub(crate) fn take_remote_request(&mut self) -> Option<(RemoteSearchQuery, u8)> {
        self.remote.take_request()
    }

    pub(crate) fn take_remote_cancellation_requested(&mut self) -> bool {
        self.remote.take_cancellation_requested()
    }

    #[cfg(test)]
    pub(crate) const fn is_open(&self) -> bool {
        self.open
    }

    pub(crate) fn draw_results(
        &mut self,
        ctx: &egui::Context,
        chat: &ChatState,
        workspace: &WorkspaceState,
        remote_available: bool,
        search_input_focused: bool,
    ) -> Option<SearchAction> {
        // Results answer one typed query: changing it, closing the overlay,
        // or losing the Microsoft 365 session discards them.
        if self.remote.is_active()
            && (!self.open
                || !remote_available
                || self.remote.query() != RemoteSearchQuery::new(&self.query).as_ref())
        {
            self.remote.clear();
        }
        if !self.open || self.query.trim().is_empty() {
            return None;
        }
        if search_input_focused && take_escape(ctx) {
            self.clear();
            return None;
        }

        let mut results = matching_results(&self.query, chat, workspace);
        if remote_available {
            results.extend(remote_rows(&self.query, &self.remote));
        }
        let mut action = None;
        if search_input_focused {
            self.move_result_selection(results.len(), ctx);
        }
        if should_activate_selection(
            search_input_focused,
            ctx.input(|input| input.key_pressed(egui::Key::Enter)),
        ) {
            action = results
                .get(self.selected_result)
                .map(|result| result.action.clone());
        }
        egui::Area::new(egui::Id::new("workspace_search_results"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 64.0))
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(theme::surface())
                    .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
                    .corner_radius(10.0)
                    .shadow(egui::Shadow::NONE)
                    .inner_margin(egui::Margin::symmetric(12, 10))
                    .show(ui, |ui| {
                        ui.set_width(520.0);
                        if results.is_empty() {
                            ui.label(
                                RichText::new("No workspace results. Try a chat or app name.")
                                    .font(theme::regular(theme::font_size::CAPTION))
                                    .color(theme::fg3()),
                            );
                        } else {
                            // Search has no arbitrary result cap. Keep the
                            // overlay usable by scrolling its result region,
                            // while keyboard navigation still reaches every
                            // matching local destination.
                            egui::ScrollArea::vertical()
                                .max_height(360.0)
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    ui.spacing_mut().item_spacing.y = 2.0;
                                    let mut last_section = None;
                                    for (index, result) in results.iter().enumerate() {
                                        if last_section != Some(result.section) {
                                            last_section = Some(result.section);
                                            ui.add_space(theme::space::SM);
                                            ui.label(
                                                RichText::new(result.section.title())
                                                    .font(theme::semibold(theme::font_size::MICRO))
                                                    .color(theme::fg3()),
                                            );
                                        }
                                        let response = draw_result_row(
                                            ui,
                                            result,
                                            index == self.selected_result,
                                        );
                                        if index == self.selected_result {
                                            ui.scroll_to_rect(
                                                response.rect,
                                                Some(egui::Align::Center),
                                            );
                                        }
                                        if response.clicked() {
                                            self.selected_result = index;
                                            action = Some(result.action.clone());
                                        }
                                    }
                                });
                        }
                        ui.add_space(3.0);
                        ui.label(
                            RichText::new("Esc closes · Up/Down moves · Enter opens")
                                .font(theme::regular(theme::font_size::MICRO))
                                .color(theme::brand_fg()),
                        );
                    });
            });
        if action
            .as_ref()
            .is_some_and(|action| !action.keeps_overlay_open())
        {
            self.clear();
        }
        action
    }

    fn clear(&mut self) {
        self.remote.clear();
        self.query.clear();
        self.open = false;
        self.selected_result = 0;
    }

    fn move_result_selection(&mut self, result_count: usize, ctx: &egui::Context) {
        let (move_down, move_up) = ctx.input(|input| {
            (
                input.key_pressed(egui::Key::ArrowDown),
                input.key_pressed(egui::Key::ArrowUp),
            )
        });
        self.selected_result =
            next_result_index(self.selected_result, result_count, move_down, move_up);
    }
}

const ROW_HEIGHT: f32 = 46.0;
const ROW_PADDING: f32 = 10.0;

/// A compact two-line row: the highlighted label over a muted detail line,
/// both left-aligned inside one selectable, hoverable area.
fn draw_result_row(ui: &mut egui::Ui, result: &SearchResult, selected: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), ROW_HEIGHT),
        egui::Sense::click(),
    );
    let response = a11y::clickable(
        ui,
        response,
        &format!("{}, {}", result.label, result.detail),
    );
    if ui.is_rect_visible(rect) {
        let fill = if selected {
            theme::brand_tint_subtle()
        } else if response.hovered() {
            theme::surface_hover()
        } else {
            Color32::TRANSPARENT
        };
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(theme::radius::MD), fill);
        let width = rect.width() - 2.0 * ROW_PADDING;
        let mut label = text_match::highlighted(
            &result.label,
            &result.ranges,
            theme::fg1(),
            theme::font_size::CAPTION,
        );
        label.wrap = egui::text::TextWrapping::truncate_at_width(width);
        let mut detail = egui::text::LayoutJob::simple_singleline(
            result.detail.clone(),
            theme::regular(theme::font_size::CAPTION),
            theme::fg3(),
        );
        detail.wrap = egui::text::TextWrapping::truncate_at_width(width);
        let label = ui.painter().layout_job(label);
        let detail = ui.painter().layout_job(detail);
        let gap = 2.0;
        let top = rect.center().y - (label.size().y + gap + detail.size().y) / 2.0;
        ui.painter().galley(
            egui::pos2(rect.left() + ROW_PADDING, top),
            label.clone(),
            theme::fg1(),
        );
        ui.painter().galley(
            egui::pos2(rect.left() + ROW_PADDING, top + label.size().y + gap),
            detail,
            theme::fg3(),
        );
    }
    response
}

const fn should_activate_selection(search_input_focused: bool, enter_pressed: bool) -> bool {
    search_input_focused && enter_pressed
}

fn next_result_index(current: usize, result_count: usize, move_down: bool, move_up: bool) -> usize {
    if result_count == 0 {
        return 0;
    }
    let mut selected = current.min(result_count - 1);
    if move_down {
        selected = (selected + 1) % result_count;
    }
    if move_up {
        selected = if selected == 0 {
            result_count - 1
        } else {
            selected - 1
        };
    }
    selected
}

struct SearchResult {
    label: String,
    detail: String,
    action: SearchAction,
    section: SearchSection,
    rank: MatchRank,
    ranges: Vec<std::ops::Range<usize>>,
}

impl SearchResult {
    fn new(
        query: &str,
        section: SearchSection,
        label: String,
        detail: String,
        action: SearchAction,
    ) -> Self {
        Self {
            rank: text_match::rank(&label, query),
            ranges: text_match::match_ranges(&label, query),
            section,
            label,
            detail,
            action,
        }
    }
}

/// The trailing Microsoft 365 group: the search row (or its state) and the
/// hits of the search the user started. Empty for a query Graph cannot take.
fn remote_rows(query: &str, remote: &RemoteMessageSearchState) -> Vec<SearchResult> {
    let Some(valid) = RemoteSearchQuery::new(query) else {
        return Vec::new();
    };
    let typed = query.trim();
    let row = |label: String, detail: &str, action: SearchAction| SearchResult {
        label,
        detail: detail.into(),
        action,
        section: SearchSection::Remote,
        rank: MatchRank::Contains,
        ranges: Vec::new(),
    };
    // Search the text as typed, not the lowercased copy used for matching.
    let search = || SearchAction::RemoteSearch(typed.to_owned());
    let shown = truncate_chars(typed, 48);
    let mut rows = Vec::new();
    let searched_this = remote.query() == Some(&valid);
    match remote.phase() {
        RemoteSearchPhase::Idle => rows.push(row(
            format!("Search Microsoft 365 messages for \u{201c}{shown}\u{201d}"),
            "Press Enter to search your Teams chats",
            search(),
        )),
        _ if !searched_this => rows.push(row(
            format!("Search Microsoft 365 messages for \u{201c}{shown}\u{201d}"),
            "Press Enter to search your Teams chats",
            search(),
        )),
        RemoteSearchPhase::Searching { .. } if remote.hits().is_empty() => rows.push(row(
            "Searching Microsoft 365 messages\u{2026}".into(),
            "Results appear here",
            search(),
        )),
        RemoteSearchPhase::Denied => rows.push(row(
            "Microsoft 365 message search is not available".into(),
            "Your account or tenant does not allow it. Enter tries again",
            search(),
        )),
        RemoteSearchPhase::Failed => rows.push(row(
            "Microsoft 365 message search failed".into(),
            "Press Enter to try again",
            search(),
        )),
        RemoteSearchPhase::Ready if remote.hits().is_empty() => rows.push(row(
            "No Microsoft 365 messages found".into(),
            "Press Enter to search again",
            search(),
        )),
        _ => {}
    }
    if searched_this {
        let clock = local_time::clock();
        for hit in remote.hits() {
            let sent = local_time::label_or(hit.sent_instant(), hit.sent_at(), &clock);
            let label = match (hit.sender(), sent.as_str()) {
                ("", "") => "Microsoft 365 message".to_owned(),
                (sender, "") => sender.to_owned(),
                ("", time) => time.to_owned(),
                (sender, time) => format!("{sender} \u{b7} {time}"),
            };
            rows.push(row(
                label,
                if hit.snippet().is_empty() {
                    "Open the conversation"
                } else {
                    hit.snippet()
                },
                SearchAction::OpenRemoteHit(hit.location().clone()),
            ));
        }
        match remote.phase() {
            RemoteSearchPhase::Searching { .. } if !remote.hits().is_empty() => rows.push(row(
                "Loading more results\u{2026}".into(),
                "",
                SearchAction::RemoteSearchMore,
            )),
            RemoteSearchPhase::Ready if remote.has_more() => rows.push(row(
                "Show more results".into(),
                "Press Enter to read the next page",
                SearchAction::RemoteSearchMore,
            )),
            _ => {}
        }
    }
    rows
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max).collect();
    cut.push('\u{2026}');
    cut
}

fn matching_results(
    query: &str,
    chat: &ChatState,
    workspace: &WorkspaceState,
) -> Vec<SearchResult> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }

    let mut results = Vec::new();
    for (section, name) in [
        (Section::Activity, "Activity"),
        (Section::Chat, "Chat"),
        (Section::Teams, "Teams"),
        (Section::Calendar, "Calendar"),
        (Section::Calls, "Calls"),
        (Section::Files, "Files"),
    ] {
        if name.to_lowercase().contains(&query) {
            results.push(SearchResult::new(
                &query,
                SearchSection::Apps,
                name.into(),
                "Open app".into(),
                SearchAction::Navigate(section),
            ));
        }
    }
    for chat_result in chat.search_results(&query) {
        results.push(SearchResult::new(
            &query,
            SearchSection::Chat,
            chat_result.label,
            chat_result.detail,
            SearchAction::OpenChat(chat_result.target),
        ));
    }
    for result in workspace.search_results(&query) {
        results.push(SearchResult::new(
            &query,
            SearchSection::for_workspace(&result.target),
            result.label,
            result.detail,
            SearchAction::OpenWorkspace(result.target),
        ));
    }
    // Stable: ties keep the order each feature produced them in.
    results.sort_by_key(|result| (result.section, result.rank));
    results
}

#[cfg(test)]
mod tests {
    use super::{
        MatchRank, SearchAction, SearchSection, matching_results, remote_rows,
        should_activate_selection,
    };
    use crate::app::workspace_coordinator::WorkspaceState;
    use crate::features::chat::{ChatSearchTarget, ChatState};
    use crate::features::search::remote::RemoteMessageSearchState;
    use crate::features::search::workspace::WorkspaceSearchTarget;
    use crate::integrations::{
        ProviderResourceRef, RemoteMessageSearchProjection, RemoteSearchHit, RemoteSearchLocation,
        RemoteSearchQuery,
    };
    use crate::shared::model::Section;
    use crate::ui::shortcuts::take_escape;
    use eframe::egui;

    #[test]
    fn search_results_include_matching_apps_and_chats() {
        let chat = ChatState::default();
        let views = WorkspaceState::default();
        let calendar = matching_results("calendar", &chat, &views);
        assert!(
            calendar
                .iter()
                .any(|result| matches!(result.action, SearchAction::Navigate(Section::Calendar)))
        );

        let person = matching_results("wren", &chat, &views);
        assert!(person.iter().any(|result| matches!(
            result.action,
            SearchAction::OpenChat(ChatSearchTarget::Workspace(0))
        )));

        let favorite = matching_results("ines", &chat, &views);
        assert!(favorite.iter().any(|result| matches!(
            result.action,
            SearchAction::OpenChat(ChatSearchTarget::Favorite(0))
        )));
    }

    #[test]
    fn results_are_grouped_apps_first_then_ranked_within_a_group() {
        let chat = ChatState::default();
        let views = WorkspaceState::default();
        let results = matching_results("a", &chat, &views);
        let sections: Vec<_> = results.iter().map(|result| result.section).collect();
        assert!(sections.windows(2).all(|pair| pair[0] <= pair[1]));
        assert_eq!(sections.first(), Some(&SearchSection::Apps));
        for pair in results.windows(2) {
            if pair[0].section == pair[1].section {
                assert!(pair[0].rank <= pair[1].rank);
            }
        }
        // An exact app title outranks other apps that merely contain it.
        let apps = matching_results("chat", &chat, &views);
        assert_eq!(apps[0].label, "Chat");
        assert_eq!(apps[0].rank, MatchRank::Exact);
    }

    #[test]
    fn highlight_ranges_cover_the_query_in_labels() {
        let chat = ChatState::default();
        let views = WorkspaceState::default();
        let result = &matching_results("calend", &chat, &views)[0];
        assert_eq!(result.ranges, vec![0..6]);
    }

    fn hit(location: RemoteSearchLocation, snippet: &str) -> RemoteSearchHit {
        RemoteSearchHit::for_test(location, "Goncalo", "2022-07-14T07:01:01Z", snippet)
    }

    fn chat_location(id: &str) -> RemoteSearchLocation {
        RemoteSearchLocation::Chat {
            chat: ProviderResourceRef::local_preview(id),
            message: None,
        }
    }

    fn ready(query: &str, hits: Vec<RemoteSearchHit>, more: bool) -> RemoteMessageSearchState {
        let mut remote = RemoteMessageSearchState::default();
        assert!(remote.start(query));
        let query = RemoteSearchQuery::new(query).unwrap();
        assert!(
            remote.apply(RemoteMessageSearchProjection::available_for_test(
                query, 0, hits, more
            ))
        );
        remote
    }

    #[test]
    fn local_results_never_include_the_remote_group() {
        let chat = ChatState::default();
        let views = WorkspaceState::default();
        let results = matching_results("a", &chat, &views);
        assert!(
            results
                .iter()
                .all(|result| result.section != SearchSection::Remote)
        );
    }

    #[test]
    fn the_remote_row_offers_the_typed_text_and_needs_a_valid_query() {
        let remote = RemoteMessageSearchState::default();
        let rows = remote_rows("  Budget Q3  ", &remote);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].section, SearchSection::Remote);
        assert!(
            rows[0]
                .label
                .starts_with("Search Microsoft 365 messages for")
        );
        assert!(rows[0].label.contains("Budget Q3"));
        assert!(matches!(
            &rows[0].action,
            SearchAction::RemoteSearch(text) if text == "Budget Q3"
        ));
        assert!(remote_rows("   ", &remote).is_empty());
        assert!(remote_rows(&"x".repeat(300), &remote).is_empty());
    }

    #[test]
    fn a_long_query_is_truncated_in_the_row_but_searched_in_full() {
        let long = "y".repeat(200);
        let rows = remote_rows(&long, &RemoteMessageSearchState::default());
        assert!(rows[0].label.chars().count() < 100);
        assert!(matches!(
            &rows[0].action,
            SearchAction::RemoteSearch(text) if *text == long
        ));
    }

    #[test]
    fn a_hit_without_a_parsable_time_keeps_the_provider_text() {
        let remote = ready(
            "budget",
            vec![RemoteSearchHit::for_test(
                chat_location("c1"),
                "Goncalo",
                "yesterday-ish",
                "x",
            )],
            false,
        );
        assert_eq!(
            remote_rows("budget", &remote)[0].label,
            "Goncalo \u{b7} yesterday-ish"
        );
    }

    #[test]
    fn remote_hits_follow_the_row_and_open_by_target() {
        let remote = ready(
            "budget",
            vec![
                hit(chat_location("c1"), "the budget plan"),
                hit(chat_location("c2"), ""),
            ],
            true,
        );
        let rows = remote_rows("budget", &remote);
        assert_eq!(rows.len(), 3);
        assert!(matches!(rows[0].action, SearchAction::OpenRemoteHit(_)));
        assert_eq!(rows[0].detail, "the budget plan");
        assert!(rows[0].ranges.is_empty());
        assert_eq!(rows[1].detail, "Open the conversation");
        // The provider time is shown as a local date (the test clock is in 2026).
        assert_eq!(rows[0].label, "Goncalo \u{b7} Jul 14, 2022 07:01");
        assert!(matches!(rows[2].action, SearchAction::RemoteSearchMore));
        assert!(rows.iter().all(|row| row.section == SearchSection::Remote));
    }

    #[test]
    fn no_more_row_without_more_results_and_an_empty_page_says_so() {
        let remote = ready("budget", vec![hit(chat_location("c1"), "x")], false);
        let rows = remote_rows("budget", &remote);
        assert_eq!(rows.len(), 1);
        let empty = ready("budget", Vec::new(), false);
        let rows = remote_rows("budget", &empty);
        assert_eq!(rows[0].label, "No Microsoft 365 messages found");
    }

    #[test]
    fn denied_failed_and_searching_states_are_one_row_each() {
        let mut remote = RemoteMessageSearchState::default();
        remote.start("budget");
        assert_eq!(
            remote_rows("budget", &remote)[0].label,
            "Searching Microsoft 365 messages\u{2026}"
        );
        let query = RemoteSearchQuery::new("budget").unwrap();
        remote.apply(RemoteMessageSearchProjection::unavailable(query, 0));
        assert_eq!(
            remote_rows("budget", &remote)[0].label,
            "Microsoft 365 message search failed"
        );
    }

    #[test]
    fn hits_for_another_typed_query_are_not_shown() {
        let remote = ready("budget", vec![hit(chat_location("c1"), "x")], false);
        let rows = remote_rows("budget plan", &remote);
        assert_eq!(rows.len(), 1);
        assert!(matches!(rows[0].action, SearchAction::RemoteSearch(_)));
    }

    #[test]
    fn only_search_actions_keep_the_overlay_open() {
        assert!(SearchAction::RemoteSearch("x".into()).keeps_overlay_open());
        assert!(SearchAction::RemoteSearchMore.keeps_overlay_open());
        assert!(!SearchAction::OpenRemoteHit(chat_location("c")).keeps_overlay_open());
        assert!(!SearchAction::Navigate(Section::Chat).keeps_overlay_open());
    }

    fn drawn(search: &mut super::SearchState, remote_available: bool) {
        let ctx = crate::ui::testing::themed_context();
        let chat = ChatState::default();
        let workspace = WorkspaceState::default();
        ctx.begin_pass(egui::RawInput::default());
        let _ = search.draw_results(&ctx, &chat, &workspace, remote_available, false);
        let _ = ctx.end_pass();
    }

    #[test]
    fn changing_the_typed_query_discards_remote_results_and_cancels() {
        let mut search = super::SearchState::default();
        search.query_mut().push_str("budget");
        search.open();
        search.start_remote_search("budget");
        let _ = search.take_remote_request();
        drawn(&mut search, true);
        assert!(search.remote.is_active());
        search.query_mut().push_str(" plan");
        drawn(&mut search, true);
        assert!(!search.remote.is_active());
        assert!(search.take_remote_cancellation_requested());
    }

    #[test]
    fn losing_the_session_or_closing_discards_remote_results() {
        let mut search = super::SearchState::default();
        search.query_mut().push_str("budget");
        search.open();
        search.start_remote_search("budget");
        drawn(&mut search, false);
        assert!(!search.remote.is_active());

        search.query_mut().push_str("budget");
        search.open();
        search.start_remote_search("budget");
        search.dismiss();
        assert!(!search.remote.is_active());
        assert!(search.take_remote_cancellation_requested());
    }

    #[test]
    fn search_results_are_empty_for_blank_or_unknown_terms() {
        let chat = ChatState::default();
        let views = WorkspaceState::default();
        assert!(matching_results("   ", &chat, &views).is_empty());
        assert!(matching_results("no such local item", &chat, &views).is_empty());
    }

    #[test]
    fn search_results_include_workspace_destinations() {
        let chat = ChatState::default();
        let views = WorkspaceState::default();

        let activity = matching_results("packing slip", &chat, &views);
        assert!(activity.iter().any(|result| matches!(
            result.action,
            SearchAction::OpenWorkspace(WorkspaceSearchTarget::Activity(2))
        )));
        let team = matching_results("facilities", &chat, &views);
        assert!(team.iter().any(|result| matches!(
            result.action,
            SearchAction::OpenWorkspace(WorkspaceSearchTarget::TeamChannel(_))
        )));
    }

    #[test]
    fn search_keeps_all_matching_local_results_reachable() {
        let chat = ChatState::default();
        let views = WorkspaceState::default();

        // A broad query intentionally matches more than the old eight-row
        // cap across apps, chats, and workspace-owned entities.
        assert!(matching_results("a", &chat, &views).len() > 8);
    }

    #[test]
    fn selection_wraps_to_a_valid_result_when_result_count_changes() {
        assert_eq!(super::next_result_index(4, 2, false, false), 1);
        assert_eq!(super::next_result_index(1, 2, true, false), 0);
        assert_eq!(super::next_result_index(0, 2, false, true), 1);
        assert_eq!(super::next_result_index(2, 0, false, false), 0);
    }

    #[test]
    fn keyboard_result_activation_requires_the_search_input_focus() {
        assert!(should_activate_selection(true, true));
        assert!(!should_activate_selection(false, true));
        assert!(!should_activate_selection(true, false));
    }

    #[test]
    fn an_unfocused_search_overlay_does_not_claim_the_keyboard() {
        let ctx = crate::ui::testing::themed_context();
        let chat = ChatState::default();
        let workspace = WorkspaceState::default();
        let mut search = super::SearchState::default();
        search.query_mut().push_str("calendar");
        search.open();
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

        assert!(
            search
                .draw_results(&ctx, &chat, &workspace, true, false)
                .is_none()
        );
        assert!(search.is_open());
        assert!(take_escape(&ctx));

        let _ = ctx.end_pass();
    }
}
pub(crate) mod remote;
pub(crate) mod workspace;
