mod persistence;
#[cfg(feature = "fuzzing")]
pub(crate) use persistence::fuzz_snapshot;

use self::persistence::ACTIVITY_SCHEMA_VERSION;
pub(crate) use self::persistence::{LocalActivityStore, StoredActivity};
use crate::data::team_catalog::{SourceReply, seeded_post_for_channel_key};
use crate::features::search::workspace::{WorkspaceSearchResult, WorkspaceSearchTarget};
use crate::features::threads::{LocalThreadReply, ThreadId, ThreadStore, ThreadTarget};
use crate::ui::avatar::{Avatar, AvatarSize};
use crate::ui::icon::{Icon, icon_button};
use crate::ui::list_row::ListRow;
use crate::ui::reactions::{ReactionSeed, ReactionSet, draw_reaction_bar};
use crate::ui::shortcuts::{multiline_submit_pressed, singleline_submit_pressed, take_escape};
use crate::ui::tabs::tab_strip_clicked;
use crate::ui::theme::{self, font_size, radius, space};
use crate::ui::toast::ToastState;
use eframe::egui::{self, Align, Layout, RichText, Stroke};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) struct ActivityState {
    filter: ActivityFilter,
    selected_item: usize,
    reply_draft: String,
    editing_reply: Option<ActivityLocalReplyEditor>,
    replies_by_item: BTreeMap<String, Vec<String>>,
    read_item_keys: BTreeSet<String>,
    unread_item_keys: BTreeSet<String>,
    reaction_overrides: BTreeMap<String, ReactionSet>,
    persistence_dirty: bool,
}

impl Default for ActivityState {
    fn default() -> Self {
        Self {
            filter: ActivityFilter::Unread,
            selected_item: 0,
            reply_draft: String::new(),
            editing_reply: None,
            replies_by_item: BTreeMap::new(),
            read_item_keys: BTreeSet::new(),
            unread_item_keys: BTreeSet::new(),
            reaction_overrides: BTreeMap::new(),
            persistence_dirty: false,
        }
    }
}

/// Transient inline editor for one locally authored canonical reply.
///
/// Reference replies stay in the Activity catalog, so they cannot create an
/// editor. The reply text itself remains in [`ThreadStore`]; Activity retains
/// only an unsaved draft and the one-shot focus request.
struct ActivityLocalReplyEditor {
    item_key: &'static str,
    reply_id: u64,
    draft: String,
    focus_requested: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActivityFilter {
    All,
    Unread,
    Mentions,
    TagMentions,
}

const ACTIVITY_TAB_LABELS: [&str; 3] = ["Unread", "Mentions", "Tag mentions"];

const PRIMARY_ACTIVITY_FILTERS: [ActivityFilter; 3] = [
    ActivityFilter::Unread,
    ActivityFilter::Mentions,
    ActivityFilter::TagMentions,
];

#[derive(Clone, Copy)]
enum ActivityReactionTarget {
    Post,
    SourceReply(&'static str),
}

/// A navigation intent emitted by the Activity surface.
///
/// Activity owns the notification selection, while the workspace coordinator
/// owns cross-surface routing. Carrying a post key when one is available
/// preserves the user's exact thread rather than routing every notification
/// to a channel's default view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ActivityAction {
    OpenChannel { channel: String },
    OpenTeamPost { channel: String, post_key: String },
}

/// Migration-only discussion data from an Activity v1 snapshot.
///
/// The app coordinator imports these stable item/target keys into the
/// canonical thread store before clearing this legacy projection. New Activity
/// interactions never read or write it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LegacyActivityDiscussion {
    pub(crate) replies_by_item: BTreeMap<String, Vec<String>>,
    pub(crate) selected_reactions_by_target: BTreeMap<String, Vec<String>>,
}

impl ActivityItem {
    /// The activity signal shown as the list row's top line.
    fn headline(&self) -> String {
        if self.mentioned {
            format!("{} mentioned you", self.name)
        } else if self.tagged {
            format!("{} tagged you", self.name)
        } else {
            self.name.to_owned()
        }
    }
}

impl ActivityFilter {
    const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Unread => "Unread",
            Self::Mentions => "Mentions",
            Self::TagMentions => "Tag mentions",
        }
    }
}

impl ActivityState {
    /// Rebuilds only the local user's durable activity mutations. Selection,
    /// filters, drafts, and other presentation state start fresh per session.
    pub(crate) fn from_stored(stored: StoredActivity) -> Self {
        let read_item_keys = stored
            .read_item_keys
            .into_iter()
            .filter(|key| known_activity_item(key).is_some_and(|item| item.unread))
            .collect();
        let unread_item_keys = stored
            .unread_item_keys
            .into_iter()
            .filter(|key| known_activity_item(key).is_some_and(|item| !item.unread))
            .collect();
        let replies_by_item = stored
            .replies_by_item
            .into_iter()
            .filter(|(item_key, replies)| {
                known_activity_item(item_key).is_some() && !replies.is_empty()
            })
            .collect();
        let reaction_overrides = stored
            .selected_reactions_by_target
            .into_iter()
            .filter_map(|(target_key, selected_icons)| {
                let seed = activity_reaction_seed_for_key(&target_key)?;
                Some((
                    target_key,
                    ReactionSet::from_seed_with_selected(seed, &selected_icons),
                ))
            })
            .collect();
        Self {
            replies_by_item,
            read_item_keys,
            unread_item_keys,
            reaction_overrides,
            ..Self::default()
        }
    }

    /// Captures Activity-owned user mutations without storing reference data
    /// or transient UI state. Legacy discussion maps remain only until the
    /// coordinator durably imports them into the canonical thread store.
    pub(crate) fn stored(&self) -> StoredActivity {
        StoredActivity {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            read_item_keys: self.read_item_keys.iter().cloned().collect(),
            unread_item_keys: self.unread_item_keys.iter().cloned().collect(),
            replies_by_item: self
                .replies_by_item
                .iter()
                .filter(|(item_key, replies)| {
                    known_activity_item(item_key).is_some() && !replies.is_empty()
                })
                .map(|(item_key, replies)| (item_key.clone(), replies.clone()))
                .collect(),
            selected_reactions_by_target: self
                .reaction_overrides
                .iter()
                .filter_map(|(target_key, reactions)| {
                    let selected_icons = reactions.selected_icons();
                    (activity_reaction_seed_for_key(target_key).is_some()
                        && !selected_icons.is_empty())
                    .then_some((target_key.clone(), selected_icons))
                })
                .collect(),
        }
    }

    pub(crate) const fn is_persistence_dirty(&self) -> bool {
        self.persistence_dirty
    }

    pub(crate) fn mark_persisted(&mut self) {
        self.persistence_dirty = false;
    }

    /// Returns an immutable legacy overlay for one-time migration only.
    /// Callers must durably save the canonical thread projection before they
    /// invoke [`Self::clear_legacy_discussion`].
    pub(crate) fn legacy_discussion(&self) -> LegacyActivityDiscussion {
        LegacyActivityDiscussion {
            replies_by_item: self.replies_by_item.clone(),
            selected_reactions_by_target: self
                .reaction_overrides
                .iter()
                .filter_map(|(target_key, reactions)| {
                    let selected_icons = reactions.selected_icons();
                    (!selected_icons.is_empty()).then_some((target_key.clone(), selected_icons))
                })
                .collect(),
        }
    }

    /// Removes the legacy overlay only after the coordinator has safely
    /// committed its canonical replacement.
    pub(crate) fn clear_legacy_discussion(&mut self) {
        if self.replies_by_item.is_empty() && self.reaction_overrides.is_empty() {
            return;
        }
        self.replies_by_item.clear();
        self.reaction_overrides.clear();
        self.mark_persistence_dirty();
    }

    pub(crate) fn search_results(
        &self,
        threads: &ThreadStore,
        query: &str,
    ) -> Vec<WorkspaceSearchResult> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return Vec::new();
        }

        activity_items()
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                let activity_matches = format!(
                    "{} {} {} {}",
                    item.name, item.title, item.channel, item.body
                )
                .to_lowercase()
                .contains(&query);
                let detail = if activity_matches {
                    Some(format!("Activity · {}", item.channel))
                } else if let Some(reply) = item.source_replies.iter().find(|reply| {
                    format!("{} {}", reply.author, reply.body)
                        .to_lowercase()
                        .contains(&query)
                }) {
                    Some(format!(
                        "Activity reply · {} · {}",
                        reply.author, item.channel
                    ))
                } else if threads
                    .replies(&activity_thread_id(*item))
                    .iter()
                    .any(|reply| reply.body().to_lowercase().contains(&query))
                {
                    Some(format!("Activity reply · You · {}", item.channel))
                } else {
                    None
                };
                detail.map(|detail| WorkspaceSearchResult {
                    label: item.title.into(),
                    detail,
                    target: WorkspaceSearchTarget::Activity(index),
                })
            })
            .collect()
    }

    pub(crate) fn select_search_result(&mut self, index: usize) -> bool {
        if index >= activity_items().len() {
            return false;
        }
        self.filter = ActivityFilter::All;
        self.select_item(index);
        true
    }

    pub(crate) fn draw_sidebar(&mut self, ui: &mut egui::Ui, toast: &mut ToastState) {
        ui.add_space(space::XL);
        ui.horizontal(|ui| {
            ui.add_space(space::XL);
            ui.label(
                RichText::new("Activity")
                    .font(theme::semibold(font_size::TITLE))
                    .color(theme::fg1()),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(space::MD);
                let menu_icon = if self.filter == ActivityFilter::All {
                    Icon::Filter
                } else {
                    Icon::More
                };
                let menu_button = icon_button(ui, menu_icon, "Activity options");
                egui::Popup::menu(&menu_button).show(|ui| {
                    let unread_count = self.unread_item_count();
                    if ui
                        .add_enabled(unread_count > 0, egui::Button::new("Mark all as read"))
                        .on_hover_text(
                            "Marks every item in this local Activity preview as read. This does not update Microsoft 365.",
                        )
                        .clicked()
                    {
                        let marked = self.mark_all_items_read();
                        debug_assert_eq!(marked, unread_count);
                        toast.show(format!(
                            "Marked {marked} local activity {} as read",
                            if marked == 1 { "item" } else { "items" }
                        ));
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .selectable_label(self.filter == ActivityFilter::All, "Show all activity")
                        .clicked()
                    {
                        if self.set_filter(ActivityFilter::All) {
                            toast.show("Showing all activity");
                        }
                        ui.close();
                    }
                });
            });
        });
        ui.add_space(space::SM);
        let selected_tab = PRIMARY_ACTIVITY_FILTERS
            .iter()
            .position(|filter| *filter == self.filter)
            .unwrap_or(usize::MAX);
        ui.horizontal(|ui| {
            ui.add_space(space::MD);
            if let Some(index) = tab_strip_clicked(ui, &ACTIVITY_TAB_LABELS, selected_tab)
                && let Some(filter) = PRIMARY_ACTIVITY_FILTERS.get(index).copied()
                && self.set_filter(filter)
            {
                toast.show(format!(
                    "Showing {} activity",
                    filter.label().to_lowercase()
                ));
            }
        });
        ui.add_space(space::SM);
        let mut shown = 0;
        for (index, item) in activity_items().iter().enumerate() {
            if !self.matches_filter(index, item) {
                continue;
            }
            shown += 1;
            let response = egui::Frame::new()
                .inner_margin(egui::Margin::symmetric(space::MD as i8, 0))
                .show(ui, |ui| {
                    let headline = item.headline();
                    ListRow::new(&headline)
                        .preview(item.preview)
                        .timestamp(item.date)
                        .selected(self.selected_item == index)
                        .unread(self.is_unread(index))
                        .avatar(Avatar::new(item.name, AvatarSize::Md32))
                        .show(ui)
                })
                .inner;
            if response.clicked() {
                self.select_item(index);
            }
        }
        if shown == 0 {
            ui.horizontal(|ui| {
                ui.add_space(space::XL);
                ui.label(
                    RichText::new("You are all caught up in this view.")
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg3()),
                );
            });
        }
    }

    /// Draws the selected activity thread and reports its cross-surface
    /// navigation intent, if the user opens the source.
    pub(crate) fn draw_page(
        &mut self,
        ui: &mut egui::Ui,
        toast: &mut ToastState,
        threads: &mut ThreadStore,
    ) -> Option<ActivityAction> {
        if self.editing_reply.is_some() && take_escape(ui.ctx()) {
            self.editing_reply = None;
        }
        let items = activity_items();
        let selected_item = self.selected_item.min(items.len() - 1);
        let item = items[selected_item];
        let thread = activity_thread_id(item);
        let replies = threads.replies(&thread).to_vec();
        let editing_reply_was_removed = self.editing_reply.as_ref().is_some_and(|editor| {
            editor.item_key == item.key
                && !replies.iter().any(|reply| reply.id() == editor.reply_id)
        });
        if editing_reply_was_removed {
            self.editing_reply = None;
        }
        let post_reactions = self.reactions_for(threads, item, ActivityReactionTarget::Post);
        let source_reply_reactions = item
            .source_replies
            .iter()
            .map(|reply| {
                self.reactions_for(
                    threads,
                    item,
                    ActivityReactionTarget::SourceReply(reply.key),
                )
            })
            .collect::<Vec<_>>();
        let local_reply_reactions = replies
            .iter()
            .map(|reply| {
                // A zero id is rejected at load; if one ever appears, render
                // the reply without reactions so row alignment is preserved.
                ThreadTarget::local_reply(reply.id()).map_or_else(ReactionSet::default, |target| {
                    threads.reactions(&thread, &target, NO_REACTIONS)
                })
            })
            .collect::<Vec<_>>();
        let mut open_channel = false;
        let mut reaction_action = None;
        let mut local_reply_reaction = None;
        let mut local_reply_edit_requested = None;
        let mut local_reply_edit_save_requested = None;
        let mut local_reply_edit_cancel_requested = None;
        let mut local_reply_delete_requested = None;
        ui.add_space(space::LG);
        ui.horizontal(|ui| {
            ui.add_space(space::XXL);
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(item.channel.rsplit(" · ").next().unwrap_or(item.channel))
                        .font(theme::semibold(font_size::SUBTITLE))
                        .color(theme::fg1()),
                );
                ui.label(
                    RichText::new(item.title)
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg3()),
                );
                ui.label(
                    RichText::new(format!("{} activity", self.filter.label()))
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::brand_fg()),
                );
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(space::XL);
                let more = icon_button(ui, Icon::More, "More options");
                egui::Popup::menu(&more).show(|ui| {
                    if ui.button("Open channel").clicked() {
                        open_channel = true;
                        ui.close();
                    }
                    if ui.button("Clear reply draft").clicked() {
                        self.reply_draft.clear();
                        toast.show("Activity reply draft cleared");
                        ui.close();
                    }
                    if ui
                        .button(if self.is_unread(selected_item) {
                            "Mark as read"
                        } else {
                            "Mark as unread"
                        })
                        .clicked()
                    {
                        let now_unread = !self.is_unread(selected_item);
                        self.set_item_unread(selected_item, now_unread);
                        toast.show(if now_unread {
                            "Activity marked as unread"
                        } else {
                            "Activity marked as read"
                        });
                        ui.close();
                    }
                });
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new("Go to channel")
                                .font(theme::regular(font_size::BODY))
                                .color(theme::fg1()),
                        )
                        .fill(theme::surface())
                        .stroke(Stroke::new(1.0_f32, theme::stroke()))
                        .corner_radius(radius::SM),
                    )
                    .clicked()
                {
                    open_channel = true;
                }
            });
        });
        ui.add_space(space::MD);
        ui.separator();
        egui::TopBottomPanel::bottom("activity_thread_composer")
            .exact_height(66.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::surface())
                    .inner_margin(egui::Margin::symmetric(18, 8))
                    .stroke(Stroke::new(1.0_f32, theme::stroke_subtle())),
            )
            .show_inside(ui, |ui| self.draw_reply_composer(ui, toast, threads));
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(32.0);
                ui.horizontal(|ui| {
                    ui.add_space((ui.available_width() * 0.16).max(24.0));
                    ui.vertical(|ui| {
                        ui.set_max_width(850.0);
                        reaction_action = draw_activity_thread(
                            ui,
                            item,
                            &post_reactions,
                            &source_reply_reactions,
                        );
                        ui.add_space(16.0);
                        if !replies.is_empty() {
                            ui.label(
                                RichText::new("Your local replies")
                                    .font(theme::semibold(font_size::CAPTION))
                                    .color(theme::fg3()),
                            );
                            ui.add_space(6.0);
                        }
                        for (reply, reactions) in replies.iter().zip(&local_reply_reactions) {
                            ui.push_id(reply.id(), |ui| {
                                egui::Frame::new()
                                    .fill(theme::outgoing_bubble())
                                    .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
                                    .corner_radius(radius::MD)
                                    .inner_margin(egui::Margin::symmetric(14, 10))
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                RichText::new("You · Now")
                                                    .font(theme::regular(font_size::CAPTION))
                                                    .color(theme::fg3()),
                                            );
                                            ui.with_layout(
                                                Layout::right_to_left(Align::Center),
                                                |ui| {
                                                    if !self
                                                        .is_editing_local_reply(item, reply.id())
                                                    {
                                                        let more = icon_button(
                                                            ui,
                                                            Icon::More,
                                                            "Reply options",
                                                        );
                                                        egui::Popup::menu(&more).show(|ui| {
                                                            if ui.button("Edit reply").clicked() {
                                                                local_reply_edit_requested =
                                                                    Some(reply.id());
                                                                ui.close();
                                                            }
                                                            if ui.button("Delete reply").clicked() {
                                                                local_reply_delete_requested =
                                                                    Some(reply.id());
                                                                ui.close();
                                                            }
                                                        });
                                                    }
                                                },
                                            );
                                        });
                                        if self.is_editing_local_reply(item, reply.id()) {
                                            let focus_requested = self
                                                .take_editing_local_reply_focus(item, reply.id());
                                            let draft = self
                                                .editing_local_reply_draft_mut(item, reply.id());
                                            let input = draft.map(|draft| {
                                                ui.add(
                                                    egui::TextEdit::multiline(draft)
                                                        .desired_rows(3)
                                                        .hint_text("Edit your reply"),
                                                )
                                            });
                                            if focus_requested && let Some(input) = &input {
                                                input.request_focus();
                                            }
                                            if input.is_some_and(|input| input.has_focus())
                                                && multiline_submit_pressed(ui)
                                            {
                                                local_reply_edit_save_requested = Some(reply.id());
                                            }
                                            ui.horizontal(|ui| {
                                                if ui.small_button("Cancel").clicked() {
                                                    local_reply_edit_cancel_requested =
                                                        Some(reply.id());
                                                }
                                                if ui.small_button("Save").clicked() {
                                                    local_reply_edit_save_requested =
                                                        Some(reply.id());
                                                }
                                            });
                                        } else {
                                            ui.label(
                                                RichText::new(reply.body())
                                                    .font(theme::regular(font_size::BODY))
                                                    .color(theme::fg1()),
                                            );
                                        }
                                        ui.add_space(6.0);
                                        if let Some(icon) = draw_reaction_bar(ui, reactions) {
                                            local_reply_reaction = Some((reply.id(), icon));
                                        }
                                    });
                            });
                            ui.add_space(8.0);
                        }
                    });
                });
                ui.add_space(30.0);
            });
        if let Some((target, icon)) = reaction_action {
            match self.toggle_reaction(threads, item, target, &icon) {
                Ok(reacted) => toast.show(if reacted {
                    format!("Added {icon} reaction")
                } else {
                    format!("Removed {icon} reaction")
                }),
                Err(error) => toast.show_error(format!("Couldn't update that reaction ({error}).")),
            }
        }
        if let Some((reply_id, icon)) = local_reply_reaction {
            if let Some(target) = ThreadTarget::local_reply(reply_id) {
                match threads.toggle_reaction(thread.clone(), target, NO_REACTIONS, &icon) {
                    Ok(true) => toast.show(format!("Added {icon} reaction")),
                    Ok(false) => toast.show(format!("Removed {icon} reaction")),
                    Err(error) => {
                        toast.show_error(format!("Couldn't update that reaction ({error})."))
                    }
                }
            } else {
                toast.show_error("Couldn't update that reaction.");
            }
        }
        if let Some(reply_id) = local_reply_edit_requested
            && let Some(reply) = replies.iter().find(|reply| reply.id() == reply_id)
        {
            self.begin_editing_local_reply(item, reply);
        }
        if let Some(reply_id) = local_reply_delete_requested {
            match self.delete_local_reply(threads, item, reply_id) {
                Ok(()) => toast.show("Local reply deleted"),
                Err(error) => {
                    toast.show_error(format!("Couldn't delete this local reply ({error})."))
                }
            }
        } else if let Some(reply_id) = local_reply_edit_cancel_requested {
            self.cancel_editing_local_reply(item, reply_id);
        } else if let Some(reply_id) = local_reply_edit_save_requested {
            match self.save_editing_local_reply(threads, item, reply_id) {
                Ok(()) => toast.show("Local reply updated"),
                Err(error) => {
                    toast.show_error(format!("Couldn't update this local reply ({error})."))
                }
            }
        }
        open_channel.then(|| item.navigation_action())
    }

    fn draw_reply_composer(
        &mut self,
        ui: &mut egui::Ui,
        toast: &mut ToastState,
        threads: &mut ThreadStore,
    ) {
        let mut submit_requested = false;
        egui::Frame::new()
            .fill(theme::surface_subtle())
            .stroke(Stroke::new(1.0_f32, theme::stroke()))
            .corner_radius(radius::MD)
            .inner_margin(egui::Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let input = ui.add_sized(
                        [ui.available_width() - 72.0, 30.0],
                        egui::TextEdit::singleline(&mut self.reply_draft)
                            .hint_text("Reply in thread")
                            .frame(false),
                    );
                    if singleline_submit_pressed(&input, ui) {
                        submit_requested = true;
                    }
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new("Send")
                                    .font(theme::semibold(font_size::BODY))
                                    .color(theme::fg_on_brand()),
                            )
                            .fill(theme::brand())
                            .corner_radius(radius::SM),
                        )
                        .clicked()
                    {
                        submit_requested = true;
                    }
                });
            });
        if submit_requested {
            match self.submit_reply(threads) {
                Ok(()) => toast.show("Reply added to this local thread"),
                Err(error) => toast.show(error.to_string()),
            }
        }
    }

    fn submit_reply(&mut self, threads: &mut ThreadStore) -> Result<(), ReplySubmitError> {
        let reply = self.reply_draft.trim().to_owned();
        if reply.is_empty() {
            return Err(ReplySubmitError::EmptyDraft);
        }
        let Some(item) = activity_items().get(self.selected_item).copied() else {
            return Err(ReplySubmitError::ThreadUnavailable);
        };
        threads
            .append_reply(activity_thread_id(item), &reply)
            .map_err(ReplySubmitError::SaveFailed)?;
        self.reply_draft.clear();
        Ok(())
    }

    fn begin_editing_local_reply(&mut self, item: ActivityItem, reply: &LocalThreadReply) {
        self.editing_reply = Some(ActivityLocalReplyEditor {
            item_key: item.key,
            reply_id: reply.id(),
            draft: reply.body().into(),
            focus_requested: true,
        });
    }

    fn is_editing_local_reply(&self, item: ActivityItem, reply_id: u64) -> bool {
        self.editing_reply
            .as_ref()
            .is_some_and(|editor| editor.item_key == item.key && editor.reply_id == reply_id)
    }

    fn take_editing_local_reply_focus(&mut self, item: ActivityItem, reply_id: u64) -> bool {
        let Some(editor) = self.editing_reply.as_mut() else {
            return false;
        };
        if editor.item_key != item.key || editor.reply_id != reply_id {
            return false;
        }
        std::mem::take(&mut editor.focus_requested)
    }

    fn editing_local_reply_draft_mut(
        &mut self,
        item: ActivityItem,
        reply_id: u64,
    ) -> Option<&mut String> {
        let editor = self.editing_reply.as_mut()?;
        (editor.item_key == item.key && editor.reply_id == reply_id).then_some(&mut editor.draft)
    }

    fn save_editing_local_reply(
        &mut self,
        threads: &mut ThreadStore,
        item: ActivityItem,
        reply_id: u64,
    ) -> Result<(), crate::features::threads::ThreadError> {
        let Some((editor_reply_id, draft)) = self.editing_reply.as_ref().and_then(|editor| {
            (editor.item_key == item.key && editor.reply_id == reply_id)
                .then(|| (editor.reply_id, editor.draft.clone()))
        }) else {
            return Err(crate::features::threads::ThreadError::MissingLocalReply);
        };
        threads.edit_reply(activity_thread_id(item), editor_reply_id, &draft)?;
        self.editing_reply = None;
        Ok(())
    }

    fn cancel_editing_local_reply(&mut self, item: ActivityItem, reply_id: u64) {
        if self.is_editing_local_reply(item, reply_id) {
            self.editing_reply = None;
        }
    }

    fn delete_local_reply(
        &mut self,
        threads: &mut ThreadStore,
        item: ActivityItem,
        reply_id: u64,
    ) -> Result<(), crate::features::threads::ThreadError> {
        threads.delete_reply(activity_thread_id(item), reply_id)?;
        self.cancel_editing_local_reply(item, reply_id);
        Ok(())
    }

    fn select_item(&mut self, index: usize) {
        if index < activity_items().len() {
            self.selected_item = index;
            self.reply_draft.clear();
            self.editing_reply = None;
            self.set_item_unread(index, false);
        }
    }

    fn is_unread(&self, index: usize) -> bool {
        activity_items().get(index).is_some_and(|item| {
            self.unread_item_keys.contains(item.key)
                || (item.unread && !self.read_item_keys.contains(item.key))
        })
    }

    /// Marks every currently unread catalog item read in this local profile.
    ///
    /// Read state uses sparse override sets rather than a full replacement of
    /// the catalog defaults: source-unread items receive a read marker, while
    /// source-read items that were locally marked unread have that marker
    /// removed. Delegating each transition to [`Self::set_item_unread`] keeps
    /// those inverse marker semantics and its idempotent dirty-state behavior
    /// intact.
    fn mark_all_items_read(&mut self) -> usize {
        let mut marked = 0;
        for index in 0..activity_items().len() {
            if self.is_unread(index) {
                self.set_item_unread(index, false);
                marked += 1;
            }
        }
        marked
    }

    /// Local activity items still unread, for the rail badge.
    pub(crate) fn unread_count(&self) -> usize {
        self.unread_item_count()
    }

    fn unread_item_count(&self) -> usize {
        activity_items()
            .iter()
            .enumerate()
            .filter(|(index, _)| self.is_unread(*index))
            .count()
    }

    fn set_item_unread(&mut self, index: usize, unread: bool) {
        let Some(item) = activity_items().get(index).copied() else {
            return;
        };
        let changed = if unread {
            let removed_read_marker = self.read_item_keys.remove(item.key);
            let added_unread_marker = !item.unread && self.unread_item_keys.insert(item.key.into());
            removed_read_marker || added_unread_marker
        } else {
            let removed_unread_marker = self.unread_item_keys.remove(item.key);
            let added_read_marker = item.unread && self.read_item_keys.insert(item.key.into());
            removed_unread_marker || added_read_marker
        };
        if changed {
            self.mark_persistence_dirty();
        }
    }

    fn set_filter(&mut self, filter: ActivityFilter) -> bool {
        if self.filter == filter {
            return false;
        }
        self.filter = filter;
        let selected_is_visible = activity_items()
            .get(self.selected_item)
            .is_some_and(|item| self.matches_filter(self.selected_item, item));
        if !selected_is_visible
            && let Some(index) = activity_items()
                .iter()
                .enumerate()
                .find_map(|(index, item)| self.matches_filter(index, item).then_some(index))
        {
            self.selected_item = index;
            self.reply_draft.clear();
            self.editing_reply = None;
        }
        true
    }

    fn matches_filter(&self, index: usize, item: &ActivityItem) -> bool {
        match self.filter {
            ActivityFilter::All => true,
            ActivityFilter::Unread => self.is_unread(index),
            ActivityFilter::Mentions => item.mentioned,
            ActivityFilter::TagMentions => item.tagged,
        }
    }

    fn reactions_for(
        &self,
        threads: &ThreadStore,
        item: ActivityItem,
        target: ActivityReactionTarget,
    ) -> ReactionSet {
        let thread = activity_thread_id(item);
        let thread_target = activity_thread_target(target);
        threads.reactions(
            &thread,
            &thread_target,
            activity_reaction_seed(item, target),
        )
    }

    fn toggle_reaction(
        &self,
        threads: &mut ThreadStore,
        item: ActivityItem,
        target: ActivityReactionTarget,
        icon: &str,
    ) -> Result<bool, crate::features::threads::ThreadError> {
        let thread = activity_thread_id(item);
        let thread_target = activity_thread_target(target);
        threads.toggle_reaction(
            thread,
            thread_target,
            activity_reaction_seed(item, target),
            icon,
        )
    }

    fn mark_persistence_dirty(&mut self) {
        self.persistence_dirty = true;
    }
}

#[derive(Clone, Copy)]
struct ActivityItem {
    key: &'static str,
    name: &'static str,
    title: &'static str,
    preview: &'static str,
    date: &'static str,
    timestamp: &'static str,
    channel: &'static str,
    /// The stable Teams catalog post key, when this notification has a
    /// matching source post in the local workspace. Absence is intentional:
    /// the coordinator falls back to opening the channel rather than
    /// inventing a thread that does not exist.
    team_post_key: Option<&'static str>,
    body: &'static str,
    reactions: &'static [ReactionSeed],
    source_replies: &'static [SourceReply],
    unread: bool,
    mentioned: bool,
    tagged: bool,
}

impl ActivityItem {
    fn navigation_action(self) -> ActivityAction {
        match self.team_post_key {
            Some(post_key) => ActivityAction::OpenTeamPost {
                channel: self.channel.into(),
                post_key: post_key.into(),
            },
            None => ActivityAction::OpenChannel {
                channel: self.channel.into(),
            },
        }
    }
}

/// Returns the canonical discussion identity for an Activity item.
///
/// Catalogued Teams posts retain their original channel/post identifiers so
/// Activity and Teams meet at the same seam. Activity-only notifications use
/// a namespaced key to avoid colliding with any present or future channel post.
fn activity_thread_id(item: ActivityItem) -> ThreadId {
    let post_key = item
        .team_post_key
        .map(str::to_owned)
        .unwrap_or_else(|| format!("activity:{}", item.key));
    // invariant: embedded seed data
    ThreadId::new(item.channel, post_key)
        .expect("static Activity discussion identifiers must be valid")
}

fn activity_thread_target(target: ActivityReactionTarget) -> ThreadTarget {
    match target {
        ActivityReactionTarget::Post => ThreadTarget::Post,
        // invariant: embedded seed data
        ActivityReactionTarget::SourceReply(reply_key) => ThreadTarget::source_reply(reply_key)
            .expect("static Activity source-reply identifiers must be valid"),
    }
}

/// Resolves a v1 Activity item key to its canonical discussion identity for
/// one-time migration. The `parking-changes` item deliberately resolves to
/// the real Teams channel/post identity; all other Activity-only items retain
/// their namespaced identities.
pub(crate) fn legacy_thread_id_for_item_key(item_key: &str) -> Option<ThreadId> {
    known_activity_item(item_key).map(activity_thread_id)
}

/// Resolves a v1 Activity reaction key to the canonical target and its
/// source-owned reaction seed. It accepts only keys that were valid for the
/// legacy projection, so migration cannot invent a target from stale text.
pub(crate) fn legacy_reaction_target(
    target_key: &str,
) -> Option<(ThreadId, ThreadTarget, &'static [ReactionSeed])> {
    for item in activity_items() {
        if target_key == item.key {
            return Some((activity_thread_id(item), ThreadTarget::Post, item.reactions));
        }
        for reply in item.source_replies {
            if target_key
                == activity_reaction_key(item.key, ActivityReactionTarget::SourceReply(reply.key))
            {
                return Some((
                    activity_thread_id(item),
                    activity_thread_target(ActivityReactionTarget::SourceReply(reply.key)),
                    reply.reactions,
                ));
            }
        }
    }
    None
}

const NO_ACTIVITY_REPLIES: &[SourceReply] = &[];
const NO_REACTIONS: &[ReactionSeed] = &[];

const DELIVERY_REPLIES: &[SourceReply] = &[SourceReply {
    key: "delivery-checklist",
    author: "Wren Castellanos",
    date: "Oct 14, 11:09",
    body: "The scanned slip is attached to the channel files.",
    reactions: &[ReactionSeed {
        icon: "👍",
        count: 1,
    }],
}];

const LOCKER_CODE_REPLIES: &[SourceReply] = &[SourceReply {
    key: "locker-follow-up",
    author: "Dara Whitlock",
    date: "Oct 06, 08:44",
    body: "Post it in the channel when you find it so everyone sees.",
    reactions: NO_REACTIONS,
}];

fn activity_items() -> [ActivityItem; 4] {
    // invariant: embedded seed data
    let parking =
        seeded_post_for_channel_key("Juniper Row All Hands · Facilities", "parking-changes")
            .expect("the Activity parking notification must resolve to a Teams catalog post");
    [
        ActivityItem {
            key: "parking-changes",
            name: parking.author,
            title: parking.title,
            preview: "Car park resurfacing schedule · Juniper Row All Hands",
            date: "Yesterday",
            timestamp: parking.date,
            channel: "Juniper Row All Hands · Facilities",
            team_post_key: Some("parking-changes"),
            body: parking.body,
            reactions: parking.reactions,
            source_replies: parking.source_replies,
            unread: true,
            mentioned: true,
            tagged: true,
        },
        ActivityItem {
            key: "van-booking",
            name: "Rhys Calloway",
            title: "Van booking follow-up",
            preview: "Casey Marlowe could you confirm the van…",
            date: "10. 30.",
            timestamp: "Oct 30, 15:26",
            channel: "Bike Commuters · General",
            team_post_key: None,
            body: "Casey Marlowe, could you confirm the van with the team before Friday? Pickup times still need settling.",
            reactions: &[ReactionSeed {
                icon: "👍",
                count: 2,
            }],
            source_replies: NO_ACTIVITY_REPLIES,
            unread: true,
            mentioned: true,
            tagged: false,
        },
        ActivityItem {
            key: "delivery-update",
            name: "Wren Castellanos",
            title: "Delivery update",
            preview: "Timber arrived this morning and looks…",
            date: "10. 14.",
            timestamp: "Oct 14, 11:02",
            channel: "Bike Commuters · Winter Gear Swap",
            team_post_key: None,
            body: "Timber arrived this morning. Could you compare the packing slip against what is on the pallet before the next order goes in?",
            reactions: &[ReactionSeed {
                icon: "👍",
                count: 1,
            }],
            source_replies: DELIVERY_REPLIES,
            unread: false,
            mentioned: false,
            tagged: false,
        },
        ActivityItem {
            key: "locker-code",
            name: "Dara Whitlock",
            title: "Locker code question",
            preview: "Do you still have my old locker number?",
            date: "10. 06.",
            timestamp: "Oct 06, 08:41",
            channel: "Book Corner · General",
            team_post_key: None,
            body: "Do you still have my old locker number? I would like to confirm which one is assigned before the next delivery arrives.",
            reactions: NO_REACTIONS,
            source_replies: LOCKER_CODE_REPLIES,
            unread: false,
            mentioned: false,
            tagged: false,
        },
    ]
}

fn known_activity_item(key: &str) -> Option<ActivityItem> {
    activity_items().into_iter().find(|item| item.key == key)
}

fn activity_reaction_seed_for_key(target_key: &str) -> Option<&'static [ReactionSeed]> {
    for item in activity_items() {
        if target_key == item.key {
            return Some(item.reactions);
        }
        for reply in item.source_replies {
            if target_key
                == activity_reaction_key(item.key, ActivityReactionTarget::SourceReply(reply.key))
            {
                return Some(reply.reactions);
            }
        }
    }
    None
}

fn activity_reaction_key(item_key: &str, target: ActivityReactionTarget) -> String {
    match target {
        ActivityReactionTarget::Post => item_key.into(),
        ActivityReactionTarget::SourceReply(reply_key) => format!("{item_key}:reply:{reply_key}"),
    }
}

fn activity_reaction_seed(
    item: ActivityItem,
    target: ActivityReactionTarget,
) -> &'static [ReactionSeed] {
    match target {
        ActivityReactionTarget::Post => item.reactions,
        ActivityReactionTarget::SourceReply(reply_key) => item
            .source_replies
            .iter()
            .find(|reply| reply.key == reply_key)
            .map_or(NO_REACTIONS, |reply| reply.reactions),
    }
}

fn draw_activity_thread(
    ui: &mut egui::Ui,
    item: ActivityItem,
    post_reactions: &ReactionSet,
    source_reply_reactions: &[ReactionSet],
) -> Option<(ActivityReactionTarget, String)> {
    let mut reaction_action = None;
    egui::Frame::new()
        .fill(theme::surface())
        .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
        .corner_radius(radius::LG)
        .inner_margin(egui::Margin::symmetric(22, 18))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                Avatar::new(item.name, AvatarSize::Md32).show(ui);
                ui.label(
                    RichText::new(item.name)
                        .font(theme::semibold(font_size::BODY))
                        .color(theme::fg1()),
                );
                ui.label(
                    RichText::new(item.timestamp)
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg3()),
                );
            });
            ui.add_space(16.0);
            ui.label(
                RichText::new(item.title)
                    .font(theme::semibold(font_size::TITLE))
                    .color(theme::fg1()),
            );
            ui.add_space(14.0);
            ui.label(
                RichText::new(item.body)
                    .font(theme::regular(font_size::BODY))
                    .color(theme::fg1()),
            );
            ui.add_space(12.0);
            if let Some(icon) = draw_reaction_bar(ui, post_reactions) {
                reaction_action = Some((ActivityReactionTarget::Post, icon));
            }
            for (index, reply) in item.source_replies.iter().enumerate() {
                ui.separator();
                ui.add_space(10.0);
                ui.label(
                    RichText::new(format!("{} · {}", reply.author, reply.date))
                        .font(theme::semibold(font_size::CAPTION))
                        .color(theme::fg1()),
                );
                ui.add_space(3.0);
                ui.label(
                    RichText::new(reply.body)
                        .font(theme::regular(font_size::BODY))
                        .color(theme::fg1()),
                );
                if let Some(reactions) = source_reply_reactions.get(index) {
                    ui.add_space(7.0);
                    if let Some(icon) = draw_reaction_bar(ui, reactions) {
                        reaction_action =
                            Some((ActivityReactionTarget::SourceReply(reply.key), icon));
                    }
                }
            }
        });
    reaction_action
}

/// Why an Activity reply could not be submitted. `Display` is the toast text.
#[derive(Debug)]
enum ReplySubmitError {
    EmptyDraft,
    ThreadUnavailable,
    SaveFailed(crate::features::threads::ThreadError),
}

impl std::fmt::Display for ReplySubmitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyDraft => formatter.write_str("Write a reply before sending."),
            Self::ThreadUnavailable => {
                formatter.write_str("That activity thread is no longer available.")
            }
            Self::SaveFailed(error) => {
                write!(formatter, "Couldn't save this reply locally ({error}).")
            }
        }
    }
}

impl std::error::Error for ReplySubmitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::SaveFailed(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
