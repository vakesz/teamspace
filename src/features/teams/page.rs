//! Channel page, tab, composer, and secondary-view rendering.
//!
//! Mutable state stays in the parent Teams module. This view module renders
//! and coordinates it without widening the feature's public interface.

use super::thread::{TeamPost, draw_inline_post_thread, draw_team_post};
use super::{NO_REACTIONS, TeamTab, TeamsAction, TeamsState, local_post_key, post_matches};
use crate::data::shared_files::shared_files_for_channel;
use crate::data::team_catalog::{SourceReply, channel_for_key, team_for_channel};
use crate::features::threads::{ThreadStore, ThreadTarget};
use crate::ui::a11y::clickable;
use crate::ui::avatar::{Avatar, AvatarSize};
use crate::ui::icon::{Icon, IconSize, icon_button};
use crate::ui::link_preview;
use crate::ui::message_nav::{Entry, MessageNav, NavOutcome};

/// A thread to open: post key, title, source reply count and source replies.
type ThreadRequest<'a> = (String, &'a str, usize, &'static [SourceReply]);
use crate::ui::reactions::ReactionSeed;
use crate::ui::shortcuts::{multiline_submit_pressed, take_escape};
use crate::ui::tabs::tab_strip;
use crate::ui::theme::{self, font_size, radius, space};
use crate::ui::toast::{ToastState, UndoKind};
use eframe::egui::{self, Align, CornerRadius, Layout, RichText, Stroke};

pub(crate) fn draw_page(
    ui: &mut egui::Ui,
    state: &mut TeamsState,
    discussions: &mut ThreadStore,
    can_start_meeting: bool,
    toast: &mut ToastState,
) -> Option<TeamsAction> {
    let action = draw_teams_posts_page(ui, state, discussions, can_start_meeting, toast);
    link_preview::draw(ui.ctx(), &mut state.link_preview, "Shared in this channel");
    action
}

pub(super) fn draw_teams_posts_page(
    ui: &mut egui::Ui,
    state: &mut TeamsState,
    discussions: &mut ThreadStore,
    can_start_meeting: bool,
    toast: &mut ToastState,
) -> Option<TeamsAction> {
    // A remote directory channel displays session-owned root/reply
    // projections, a narrowly scoped text-only root composer, and a
    // text-only composer for a selected root post. It never creates a local
    // feed, thread, reaction, files, notes, or meeting path.
    // Short-circuit before constructing any local action path so an opaque
    // provider selection cannot be mistaken for `selected_channel`.
    if state.remote.has_selection() {
        match super::remote::draw_selected_channel(ui, &mut state.remote) {
            Some(super::remote::RemoteChannelPageAction::RefreshPosts) => {
                // A refresh preserves the exact opaque selection target. The
                // workspace coordinator turns this into the same bounded
                // remote read that initial selection uses; it never becomes
                // a local post, mutation, or persistence operation.
                return state.remote_channel_selection_action();
            }
            Some(super::remote::RemoteChannelPageAction::SelectPost(post)) => {
                // The selected root post starts a bounded reply read. Its
                // next render may expose the separate text-only remote reply
                // composer, never a local discussion or reaction action.
                return state.remote_channel_post_selection_action(post);
            }
            Some(super::remote::RemoteChannelPageAction::ToggleReaction {
                post,
                reaction,
                selected,
            }) => {
                // The selected root-post projection owns reaction state. This
                // path only emits its exact opaque target for the app-owned
                // command queue; it never falls through to local ThreadStore.
                return state.remote_channel_post_reaction_action(post, reaction, selected);
            }
            Some(super::remote::RemoteChannelPageAction::ToggleReplyReaction {
                reply,
                reaction,
                selected,
            }) => {
                // The selected reply projection owns reaction state. This
                // emits every opaque route segment for the app-owned command
                // queue and cannot fall through to a local thread reaction.
                return state.remote_channel_reply_reaction_action(reply, reaction, selected);
            }
            Some(super::remote::RemoteChannelPageAction::RefreshReplies) => {
                // Retrying a terminal reply read retains the exact selected
                // root post and emits the same read-only action. It does not
                // clear or cancel an active same-target reply request.
                return state.remote_channel_reply_refresh_action();
            }
            Some(super::remote::RemoteChannelPageAction::ShowPosts) => {
                // Surface switching is purely presentational. It preserves the
                // independent session-only posts, replies, and file metadata
                // rather than turning a remote channel into local state.
                state.remote_channel_posts_surface_action();
                return None;
            }
            Some(super::remote::RemoteChannelPageAction::ShowFiles) => {
                // The first explicit Files visit becomes a bounded,
                // selection-scoped metadata read. A retained file snapshot
                // only changes surface and yields no provider action.
                return state.remote_channel_files_selection_action();
            }
            Some(super::remote::RemoteChannelPageAction::RefreshFiles) => {
                // Retrying the same selected channel retains opaque targets;
                // the app/controller deduplicates an active same-target read.
                return state.remote_channel_files_refresh_action();
            }
            Some(super::remote::RemoteChannelPageAction::OpenFile(file)) => {
                // An existing channel-item browser destination is not a
                // provider mutation or local file action. Keep it within the
                // session-only Teams state for the app shell's final HTTPS
                // browser boundary.
                state.remote_channel_file_open_action(file);
                return None;
            }
            Some(super::remote::RemoteChannelPageAction::SubmitPost) => {
                // The action has already locked the session-only draft. The
                // app/controller owns its opaque delivery lifecycle; this UI
                // path never appends an optimistic local post.
                match state.remote_channel_post_submission_action() {
                    Ok(action) => return Some(action),
                    Err(error) => {
                        toast.show_error(error.user_message());
                        return None;
                    }
                }
            }
            Some(super::remote::RemoteChannelPageAction::SubmitReply) => {
                // The selected-root reply action has already locked its
                // session-only draft. Delivery remains owned by the app
                // controller and cannot create an optimistic local reply.
                match state.remote_channel_reply_submission_action() {
                    Ok(action) => return Some(action),
                    Err(error) => {
                        toast.show_error(error.user_message());
                        return None;
                    }
                }
            }
            None => return None,
        }
    }
    let mut action = None;
    // Resolve the selected team and channel once per frame; every post below
    // reuses them instead of rebuilding catalog keys per post.
    let selected_team = team_for_channel(&state.selected_channel);
    let selected_definition = channel_for_key(&state.selected_channel);
    let team_name = selected_team.map_or("Team", |team| team.name);
    let channel_name = state
        .selected_channel
        .rsplit(" · ")
        .next()
        .unwrap_or("General")
        .to_owned();
    let compact_header = ui.available_width() < 780.0;
    egui::TopBottomPanel::top("team_channel_header")
        .exact_height(if compact_header { 96.0 } else { 56.0 })
        .frame(
            egui::Frame::new()
                .fill(theme::surface())
                .inner_margin(egui::Margin::symmetric(22, 8))
                .stroke(Stroke::new(1.0_f32, theme::stroke_subtle())),
        )
        .show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                Avatar::new(team_name, AvatarSize::Md32)
                    .rounded_square()
                    .show(ui);
                ui.label(
                    RichText::new(&channel_name)
                        .font(theme::semibold(font_size::SUBTITLE))
                        .color(theme::fg1()),
                );
                if !compact_header {
                    ui.add_space(space::SM);
                    draw_team_tabs(ui, state, toast);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let more = icon_button(ui, Icon::More, "More options");
                    egui::Popup::menu(&more).show(|ui| {
                        if ui.button("View shared content").clicked() {
                            state.team_tab = TeamTab::Shared;
                            ui.close();
                        }
                        if ui.button("Open channel notes").clicked() {
                            state.team_tab = TeamTab::Notes;
                            ui.close();
                        }
                        if ui.button("Clear channel search").clicked() {
                            state.clear_channel_search();
                            ui.close();
                        }
                    });
                    if icon_button(ui, Icon::Search, "Search this channel").clicked() {
                        state.toggle_channel_search();
                    }
                    ui.add_space(space::SM);
                    let meet = egui::Button::new((
                        Icon::Video.image(IconSize::S16),
                        RichText::new("Meet now")
                            .font(theme::regular(font_size::BODY))
                            .color(theme::fg1()),
                        Icon::ChevronDown.image(IconSize::S16),
                    ))
                    .stroke(Stroke::new(1.0_f32, theme::stroke()))
                    .corner_radius(CornerRadius::same(radius::MD));
                    if ui
                        .add_enabled(can_start_meeting, meet)
                        .on_hover_text(if can_start_meeting {
                            "Start a local channel meeting"
                        } else {
                            "End the active local call or meeting before starting another"
                        })
                        .clicked()
                    {
                        action = Some(TeamsAction::StartMeeting);
                    }
                });
            });
            if compact_header {
                ui.add_space(space::XS);
                ui.horizontal(|ui| draw_team_tabs(ui, state, toast));
            }
        });

    if state.team_search_open {
        egui::Frame::new()
            .fill(theme::surface_subtle())
            .inner_margin(egui::Margin::symmetric(22, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Search posts")
                            .font(theme::regular(font_size::CAPTION))
                            .color(theme::fg3()),
                    );
                    ui.add_sized(
                        [ui.available_width() - 98.0, 30.0],
                        egui::TextEdit::singleline(&mut state.team_search_query)
                            .hint_text("Find words in this channel"),
                    );
                    if ui.small_button("Clear").clicked() {
                        state.team_search_query.clear();
                    }
                });
            });
    }

    if state.team_tab != TeamTab::Posts {
        return draw_team_secondary_tab(ui, state);
    }

    let search_query = state.team_search_query.trim().to_lowercase();
    let composer_open = state
        .selected_channel_state()
        .is_some_and(|channel| channel.composer_open);
    let composer_action = egui::TopBottomPanel::bottom("team_channel_composer")
        .exact_height(if composer_open { 174.0 } else { 58.0 })
        .frame(
            egui::Frame::new()
                .fill(theme::canvas())
                .inner_margin(egui::Margin::symmetric(22, 8))
                .stroke(Stroke::new(1.0_f32, theme::stroke_subtle())),
        )
        .show_inside(ui, |ui| draw_channel_composer(ui, state, toast))
        .inner;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(space::XL);
            let available = ui.available_width();
            let column = (available - 64.0).clamp(0.0, POSTS_MAX_WIDTH);
            ui.horizontal(|ui| {
                ui.add_space(((available - column) / 2.0).max(0.0));
                ui.vertical(|ui| {
                    ui.set_width(column);
                    let mut shown_posts = 0;
                    let mut nav = MessageNav::default();
                    let mut nav_threads: Vec<Option<ThreadRequest>> = Vec::new();
                    let mut requested_thread: Option<(
                        String,
                        &str,
                        usize,
                        &'static [SourceReply],
                    )> = None;
                    let mut requested_reaction: Option<(
                        String,
                        ThreadTarget,
                        &'static [ReactionSeed],
                        String,
                    )> = None;
                    let mut link_requested = None;
                    let mut local_post_edit_requested = None;
                    let mut local_post_delete_requested = None;
                    let mut local_post_editor_action = None;
                    let source_posts = selected_definition
                        .map(|channel| channel.posts)
                        .unwrap_or_default();
                    for source_post in source_posts {
                        if !post_matches(&search_query, source_post.title, source_post.body) {
                            continue;
                        }
                        let thread_id =
                            TeamsState::discussion_id(&state.selected_channel, source_post.key);
                        let reactions = TeamsState::reactions_for_discussion(
                            discussions,
                            &thread_id,
                            &ThreadTarget::Post,
                            source_post.reactions,
                        );
                        let action = draw_team_post(
                            ui,
                            TeamPost {
                                key: source_post.key,
                                author: source_post.author,
                                date: source_post.date,
                                title: source_post.title,
                                body: source_post.body,
                                link: source_post.link,
                                reactions: &reactions,
                                source_reply_count: source_post.source_reply_count,
                                source_replies: source_post.source_replies,
                                local_reply_count: discussions.replies(&thread_id).len(),
                                locally_authored: false,
                            },
                        );
                        if let Some(item) = action.nav_item {
                            nav.push(item);
                            nav_threads.push(Some((
                                source_post.key.to_owned(),
                                source_post.title,
                                source_post.source_reply_count,
                                source_post.source_replies,
                            )));
                        }
                        if action.open_thread {
                            requested_thread = Some((
                                source_post.key.to_owned(),
                                source_post.title,
                                source_post.source_reply_count,
                                source_post.source_replies,
                            ));
                        }
                        if let Some(icon) = action.reaction {
                            requested_reaction = Some((
                                source_post.key.into(),
                                ThreadTarget::Post,
                                source_post.reactions,
                                icon,
                            ));
                        }
                        if state.active_thread.as_ref().is_some_and(|thread| {
                            thread.channel == state.selected_channel
                                && thread.key == source_post.key
                        }) {
                            draw_inline_post_thread(ui, state, discussions, toast);
                            ui.add_space(12.0);
                        }
                        if let Some(link) = action.link {
                            link_requested = Some(link);
                        }
                        shown_posts += 1;
                        ui.add_space(12.0);
                    }
                    let local_posts = state
                        .selected_channel_state()
                        .map(|channel| channel.posts.clone())
                        .unwrap_or_default();
                    for post in &local_posts {
                        if post_matches(&search_query, "New channel post", &post.body) {
                            let key = local_post_key(post.id);
                            let thread_id =
                                TeamsState::discussion_id(&state.selected_channel, &key);
                            let reactions = TeamsState::reactions_for_discussion(
                                discussions,
                                &thread_id,
                                &ThreadTarget::Post,
                                NO_REACTIONS,
                            );
                            let action = draw_team_post(
                                ui,
                                TeamPost {
                                    key: &key,
                                    author: "You",
                                    date: "Now",
                                    title: "New channel post",
                                    body: &post.body,
                                    link: None,
                                    reactions: &reactions,
                                    source_reply_count: 0,
                                    source_replies: &[],
                                    local_reply_count: discussions.replies(&thread_id).len(),
                                    locally_authored: true,
                                },
                            );
                            if let Some(item) = action.nav_item {
                                nav.push(item);
                                nav_threads.push(Some((key.clone(), "New channel post", 0, &[])));
                            }
                            if action.open_thread {
                                requested_thread = Some((key.clone(), "New channel post", 0, &[]));
                            }
                            if let Some(icon) = action.reaction {
                                requested_reaction =
                                    Some((key.clone(), ThreadTarget::Post, NO_REACTIONS, icon));
                            }
                            if action.edit_local_post {
                                local_post_edit_requested = Some(post.id);
                            }
                            if action.delete_local_post {
                                local_post_delete_requested = Some(post.id);
                            }
                            if state.is_editing_local_post(post.id) {
                                local_post_editor_action =
                                    draw_local_post_editor(ui, state, post.id);
                                ui.add_space(8.0);
                            }
                            if state.active_thread.as_ref().is_some_and(|thread| {
                                thread.channel == state.selected_channel && thread.key == key
                            }) {
                                draw_inline_post_thread(ui, state, discussions, toast);
                                ui.add_space(12.0);
                            }
                            if let Some(link) = action.link {
                                link_requested = Some(link);
                            }
                            shown_posts += 1;
                            ui.add_space(12.0);
                        }
                    }
                    let entry = if ui.memory(|memory| memory.focused().is_none())
                        && ui.rect_contains_pointer(ui.max_rect())
                    {
                        Entry::Anywhere
                    } else {
                        Entry::Never
                    };
                    if let NavOutcome::Activated(position) =
                        nav.handle(ui, team_composer_id(), entry)
                        && let Some(request) = nav_threads.get_mut(position).and_then(Option::take)
                    {
                        requested_thread = Some(request);
                    }
                    if let Some((key, title, source_reply_count, source_replies)) = requested_thread
                    {
                        state.open_post_thread(&key, title, source_reply_count, source_replies);
                    }
                    if let Some(id) = local_post_edit_requested
                        && !state.begin_editing_local_post(id)
                    {
                        toast.show_error("That local channel post is no longer available.");
                    }
                    if let Some(id) = local_post_delete_requested {
                        match state.delete_local_post(discussions, id) {
                            Ok(()) => {
                                toast.show_undo("Local channel post deleted", UndoKind::ChannelPost)
                            }
                            Err(error) => toast.show_error(error.message()),
                        }
                    } else if let Some(action) = local_post_editor_action {
                        let id = action.id();
                        match action {
                            LocalPostEditorAction::Cancel(_) => state.cancel_editing_local_post(id),
                            LocalPostEditorAction::Save(_) => {
                                match state.save_editing_local_post(id) {
                                    Ok(()) => toast.show("Local channel post updated"),
                                    Err(error) => toast.show_error(error.message()),
                                }
                            }
                        }
                    }
                    if let Some((key, target, seed, icon)) = requested_reaction {
                        match state.toggle_post_reaction(discussions, &key, target, seed, &icon) {
                            Ok(true) => toast.show(format!("Added {icon} reaction")),
                            Ok(false) => toast.show(format!("Removed {icon} reaction")),
                            Err(error) => toast.show(error.to_string()),
                        }
                    }
                    if let Some(link) = link_requested {
                        state.open_link_preview(&link);
                    }
                    if shown_posts == 0 {
                        ui.label(
                            RichText::new("No channel posts match this search.")
                                .font(theme::regular(font_size::CAPTION))
                                .color(theme::fg3()),
                        );
                        ui.add_space(12.0);
                    }
                });
            });
            ui.add_space(space::XXL);
        });
    action.or(composer_action)
}

const POSTS_MAX_WIDTH: f32 = 900.0;

const TEAM_TABS: [TeamTab; 5] = [
    TeamTab::Posts,
    TeamTab::Shared,
    TeamTab::Notes,
    TeamTab::Suppliers,
    TeamTab::Monthly,
];

fn draw_team_tabs(ui: &mut egui::Ui, state: &mut TeamsState, toast: &mut ToastState) {
    const LABELS: [&str; 5] = [
        TeamTab::Posts.label(),
        TeamTab::Shared.label(),
        TeamTab::Notes.label(),
        TeamTab::Suppliers.label(),
        TeamTab::Monthly.label(),
    ];
    let selected = TEAM_TABS
        .iter()
        .position(|tab| *tab == state.team_tab)
        .unwrap_or(0);
    let next = tab_strip(ui, &LABELS, selected);
    state.team_tab = TEAM_TABS[next];
    if icon_button(ui, Icon::Add, "Add a tab").clicked() {
        toast.show("Custom tabs are not available in this local preview.");
    }
}

pub(super) fn draw_channel_composer(
    ui: &mut egui::Ui,
    state: &mut TeamsState,
    toast: &mut ToastState,
) -> Option<TeamsAction> {
    if state
        .selected_channel_state()
        .is_some_and(|channel| channel.composer_open)
    {
        if take_escape(ui.ctx()) {
            state.cancel_channel_composer();
            return None;
        }
        let focus_composer = {
            let channel = state.selected_channel_state_mut();
            let focus_composer = channel.composer_focus_requested;
            channel.composer_focus_requested = false;
            focus_composer
        };
        let mut cancel_requested = false;
        let mut submit_requested = false;
        egui::Frame::new()
            .fill(theme::surface())
            .stroke(Stroke::new(1.0_f32, theme::stroke()))
            .corner_radius(CornerRadius::same(radius::LG))
            .inner_margin(egui::Margin::symmetric(14, 12))
            .show(ui, |ui| {
                ui.label(
                    RichText::new("Post in channel")
                        .strong()
                        .color(theme::fg1()),
                );
                let channel = state.selected_channel_state_mut();
                let input = ui.add_sized(
                    [ui.available_width(), 88.0],
                    egui::TextEdit::multiline(&mut channel.draft)
                        .id(team_composer_id())
                        .hint_text("Share an update with this channel"),
                );
                if focus_composer {
                    input.request_focus();
                }
                if input.has_focus() && multiline_submit_pressed(ui) {
                    submit_requested = true;
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("{} to post", crate::ui::shortcuts::submit_chord()))
                            .font(theme::regular(font_size::CAPTION))
                            .color(theme::fg3()),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Cancel").clicked() {
                            cancel_requested = true;
                        }
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new("Post").color(theme::fg_on_brand()),
                                )
                                .fill(theme::brand())
                                .corner_radius(CornerRadius::same(radius::MD)),
                            )
                            .clicked()
                        {
                            submit_requested = true;
                        }
                    });
                });
            });
        if cancel_requested {
            state.cancel_channel_composer();
        }
        if submit_requested {
            let submitted_post = state.pending_channel_post();
            match state.submit_channel_post() {
                Ok(()) => {
                    toast.show("Posted to this local preview channel");
                    let (channel, body) = submitted_post
                        .expect("a submitted channel post must have non-empty pending content");
                    return Some(TeamsAction::ChannelPostSubmitted { channel, body });
                }
                Err(error) => toast.show_error(error.message()),
            }
        }
        return None;
    }

    ui.vertical_centered(|ui| {
        let post = egui::Button::new((
            Icon::Compose
                .image(IconSize::S16)
                .tint(theme::fg_on_brand()),
            RichText::new("Post in channel")
                .font(theme::semibold(font_size::BODY))
                .color(theme::fg_on_brand()),
        ))
        .fill(theme::brand())
        .min_size(egui::vec2(160.0, 36.0))
        .corner_radius(CornerRadius::same(radius::MD));
        if ui.add(post).clicked() {
            state.open_channel_composer();
        }
    });
    None
}

#[derive(Clone, Copy)]
enum LocalPostEditorAction {
    Save(u64),
    Cancel(u64),
}

impl LocalPostEditorAction {
    const fn id(self) -> u64 {
        match self {
            Self::Save(id) | Self::Cancel(id) => id,
        }
    }
}

fn draw_local_post_editor(
    ui: &mut egui::Ui,
    state: &mut TeamsState,
    id: u64,
) -> Option<LocalPostEditorAction> {
    let focus_requested = state.take_editing_local_post_focus(id);
    let draft = state
        .editing_local_post_draft_mut(id)
        .expect("the rendered local post editor must retain its draft");
    let mut action = None;
    egui::Frame::new()
        .fill(theme::surface())
        .stroke(Stroke::new(1.0_f32, theme::stroke()))
        .corner_radius(CornerRadius::same(radius::MD))
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.label(
                RichText::new("Edit your local channel post")
                    .font(theme::semibold(font_size::CAPTION))
                    .color(theme::fg1()),
            );
            let input = ui.add(
                egui::TextEdit::multiline(draft)
                    .desired_rows(3)
                    .hint_text("Update this local channel post"),
            );
            if focus_requested {
                input.request_focus();
            }
            if input.has_focus() && multiline_submit_pressed(ui) {
                action = Some(LocalPostEditorAction::Save(id));
            }
            ui.horizontal(|ui| {
                if ui.small_button("Cancel").clicked() {
                    action = Some(LocalPostEditorAction::Cancel(id));
                }
                if ui.small_button("Save").clicked() {
                    action = Some(LocalPostEditorAction::Save(id));
                }
            });
        });
    action
}

fn draw_team_secondary_tab(ui: &mut egui::Ui, state: &mut TeamsState) -> Option<TeamsAction> {
    let mut action = None;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(28.0);
            ui.horizontal(|ui| {
                ui.add_space(32.0);
                ui.vertical(|ui| {
                    ui.set_max_width(760.0);
                    match state.team_tab {
                        TeamTab::Shared => {
                            ui.label(
                                RichText::new("Shared in this channel")
                                    .font(theme::semibold(font_size::TITLE))
                                    .color(theme::fg1()),
                            );
                            ui.label(
                                RichText::new("Files and links shared with this team")
                                    .font(theme::regular(font_size::CAPTION))
                                    .color(theme::fg3()),
                            );
                            ui.add_space(18.0);
                            let mut shown = false;
                            let mut selected_preview = None;
                            for file in shared_files_for_channel(&state.selected_channel) {
                                shown = true;
                                let response = egui::Frame::new()
                                    .fill(theme::surface())
                                    .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
                                    .corner_radius(8.0)
                                    .inner_margin(egui::Margin::symmetric(14, 12))
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                RichText::new(file.kind.label())
                                                    .font(theme::regular(font_size::CAPTION))
                                                    .color(theme::brand_fg()),
                                            );
                                            ui.vertical(|ui| {
                                                ui.label(
                                                    RichText::new(file.name)
                                                        .font(theme::semibold(font_size::BODY))
                                                        .color(theme::fg1()),
                                                );
                                                ui.label(
                                                    RichText::new(file.detail)
                                                        .font(theme::regular(font_size::CAPTION))
                                                        .color(theme::fg3()),
                                                );
                                            });
                                        });
                                    });
                                if clickable(
                                    ui,
                                    response.response,
                                    &format!("Open file preview: {}", file.name),
                                )
                                .on_hover_text("Open bundled file preview")
                                .clicked()
                                {
                                    selected_preview = Some(file.name);
                                }
                                ui.add_space(8.0);
                            }
                            if state.selected_channel == "Bike Commuters · General" {
                                shown = true;
                                let response = egui::Frame::new()
                                    .fill(theme::surface())
                                    .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
                                    .corner_radius(8.0)
                                    .inner_margin(egui::Margin::symmetric(14, 12))
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                RichText::new("LINK")
                                                    .font(theme::regular(font_size::CAPTION))
                                                    .color(theme::brand_fg()),
                                            );
                                            ui.vertical(|ui| {
                                                ui.label(
                                                    RichText::new("Cycle Shelter Planning Guide")
                                                        .font(theme::semibold(font_size::BODY))
                                                        .color(theme::fg1()),
                                                );
                                                ui.label(
                                                    RichText::new("example.com")
                                                        .font(theme::regular(font_size::CAPTION))
                                                        .color(theme::fg3()),
                                                );
                                            });
                                        });
                                    });
                                if clickable(
                                    ui,
                                    response.response,
                                    "Open link details: Cycle Shelter Planning Guide",
                                )
                                .on_hover_text("Open local link details")
                                .clicked()
                                {
                                    state.open_link_preview("Cycle Shelter Planning Guide");
                                }
                                ui.add_space(8.0);
                            }
                            if let Some(name) = selected_preview {
                                action = Some(TeamsAction::OpenSharedPreview(name.into()));
                            }
                            if !shown {
                                ui.label(
                                    RichText::new("No shared items in this local preview channel.")
                                        .font(theme::regular(font_size::CAPTION))
                                        .color(theme::fg3()),
                                );
                            }
                        }
                        TeamTab::Notes => {
                            ui.label(
                                RichText::new("Channel notes")
                                    .font(theme::semibold(font_size::TITLE))
                                    .color(theme::fg1()),
                            );
                            ui.label(
                                RichText::new("Keep follow-ups for this local preview channel")
                                    .font(theme::regular(font_size::CAPTION))
                                    .color(theme::fg3()),
                            );
                            ui.add_space(18.0);
                            let mut notes_changed = false;
                            egui::Frame::new()
                                .fill(theme::surface())
                                .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
                                .corner_radius(8.0)
                                .inner_margin(egui::Margin::symmetric(16, 14))
                                .show(ui, |ui| {
                                    let channel = state.selected_channel_state_mut();
                                    notes_changed = ui
                                        .add(
                                            egui::TextEdit::multiline(&mut channel.notes)
                                                .desired_rows(12)
                                                .hint_text("Add a note, decision, or follow-up…")
                                                .frame(false),
                                        )
                                        .changed();
                                    ui.label(
                                        RichText::new("Notes are kept in this local profile.")
                                            .font(theme::regular(font_size::CAPTION))
                                            .color(theme::fg3()),
                                    );
                                });
                            if notes_changed {
                                state.mark_persistence_dirty();
                            }
                        }
                        TeamTab::Suppliers | TeamTab::Monthly => {
                            let tab = state.team_tab;
                            let (heading, description, hint) = match tab {
                                TeamTab::Suppliers => (
                                    "Supplier contacts",
                                    "Keep supplier contacts and open questions for this channel.",
                                    "Add a contact, question, or follow-up…",
                                ),
                                TeamTab::Monthly => (
                                    "Monthly – Workshop Crew",
                                    "Keep the monthly discussion and decisions together.",
                                    "Add a monthly update, decision, or action…",
                                ),
                                _ => unreachable!("only secondary note tabs reach this branch"),
                            };
                            ui.label(
                                RichText::new(heading)
                                    .font(theme::semibold(font_size::TITLE))
                                    .color(theme::fg1()),
                            );
                            ui.add_space(10.0);
                            ui.label(
                                RichText::new(description)
                                    .font(theme::regular(font_size::CAPTION))
                                    .color(theme::fg3()),
                            );
                            ui.add_space(18.0);
                            let mut notes_changed = false;
                            egui::Frame::new()
                                .fill(theme::surface())
                                .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
                                .corner_radius(8.0)
                                .inner_margin(egui::Margin::symmetric(16, 14))
                                .show(ui, |ui| {
                                    let channel = state.selected_channel_state_mut();
                                    let notes = match tab {
                                        TeamTab::Suppliers => &mut channel.supplier_notes,
                                        TeamTab::Monthly => &mut channel.monthly_notes,
                                        _ => unreachable!(
                                            "only secondary note tabs reach this branch"
                                        ),
                                    };
                                    notes_changed = ui
                                        .add(
                                            egui::TextEdit::multiline(notes)
                                                .desired_rows(12)
                                                .hint_text(hint)
                                                .frame(false),
                                        )
                                        .changed();
                                    ui.label(
                                        RichText::new("Notes are kept in this local profile.")
                                            .font(theme::regular(font_size::CAPTION))
                                            .color(theme::fg3()),
                                    );
                                });
                            if notes_changed {
                                state.mark_persistence_dirty();
                            }
                        }
                        TeamTab::Posts => {}
                    }
                });
            });
        });
    action
}

/// Stable identity of the channel composer, so post navigation can return
/// focus to it.
fn team_composer_id() -> egui::Id {
    egui::Id::new("team_channel_composer_input")
}

#[cfg(test)]
mod tests {
    use super::draw_page;
    use crate::features::teams::TeamsState;
    use crate::features::threads::ThreadStore;
    use crate::ui::toast::ToastState;
    use eframe::egui;
    use egui_kittest::Harness;

    struct Page {
        teams: TeamsState,
        threads: ThreadStore,
        toast: ToastState,
    }

    fn page() -> Harness<'static, Page> {
        Harness::builder()
            .with_size(egui::vec2(900.0, 1600.0))
            .build_ui_state(
                |ui, page: &mut Page| {
                    let key = egui::Id::new("teams_nav_theme");
                    if ui.ctx().data(|data| data.get_temp::<bool>(key)).is_none() {
                        crate::ui::theme::apply(ui.ctx());
                        ui.ctx().data_mut(|data| data.insert_temp(key, true));
                        ui.ctx().request_discard("install theme fonts");
                        return;
                    }
                    let _ = draw_page(
                        ui,
                        &mut page.teams,
                        &mut page.threads,
                        true,
                        &mut page.toast,
                    );
                },
                Page {
                    teams: TeamsState::default(),
                    threads: ThreadStore::default(),
                    toast: ToastState::default(),
                },
            )
    }

    #[test]
    fn j_and_enter_on_a_channel_post_open_its_thread() {
        let mut harness = page();
        harness.run();
        harness.hover_at(egui::pos2(450.0, 760.0));
        harness.run();
        assert!(harness.state().teams.active_thread.is_none());

        harness.key_press(egui::Key::J);
        harness.run();
        harness.key_press(egui::Key::Enter);
        harness.run();
        assert!(harness.state().teams.active_thread.is_some());
    }

    #[test]
    fn source_replies_are_navigable_and_escape_returns_to_the_reply_composer() {
        let mut harness = page();
        {
            let page = harness.state_mut();
            page.teams
                .select_team_channel("Juniper Row All Hands · Facilities");
            assert!(page.teams.open_selected_post_thread("parking-changes"));
        }
        harness.run();
        use egui_kittest::kittest::Queryable;
        let replies_label = harness.get_by_label("Reference replies").rect().center();
        harness.hover_at(replies_label);
        harness.run();

        harness.key_press(egui::Key::J);
        harness.run();
        let reply = harness.ctx.memory(|memory| memory.focused());
        assert!(reply.is_some());
        assert_ne!(
            reply,
            Some(crate::features::teams::thread::thread_composer_id_for_test())
        );

        harness.key_press(egui::Key::J);
        harness.run();
        assert_ne!(harness.ctx.memory(|memory| memory.focused()), reply);

        harness.key_press(egui::Key::Escape);
        harness.run();
        assert_eq!(
            harness.ctx.memory(|memory| memory.focused()),
            Some(crate::features::teams::thread::thread_composer_id_for_test())
        );
        // Esc went to navigation, not to closing the thread.
        assert!(harness.state().teams.active_thread.is_some());
    }
}
