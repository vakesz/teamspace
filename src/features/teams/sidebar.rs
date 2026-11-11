//! Sidebar and community-picker views owned by the Teams feature.

use super::{REFERENCE_COMMUNITY_NAME, TeamTab, TeamsAction, TeamsState, team_matches_filter};
use crate::data::team_catalog::{TeamDefinition, channel_key, team_catalog};
use crate::ui::avatar::{Avatar, AvatarSize};
use crate::ui::icon::{Icon, IconSize, icon_button};
use crate::ui::list_row::{layout_truncated, section_header};
use crate::ui::modal;
use crate::ui::shortcuts::take_escape;
use crate::ui::theme::{self, font_size, radius, space};
use crate::ui::toast::ToastState;
use eframe::egui::{self, Align, CornerRadius, Layout, RichText, Sense, Ui};

const ROW_HEIGHT: f32 = 32.0;
const ROW_INSET: f32 = 8.0;
const CHANNEL_INDENT: f32 = 52.0;
const ACTION_SIZE: f32 = 32.0;

pub(crate) fn draw_sidebar(
    ui: &mut egui::Ui,
    state: &mut TeamsState,
    can_start_meeting: bool,
    toast: &mut ToastState,
) -> Option<TeamsAction> {
    let mut action = None;
    ui.add_space(space::XL);
    ui.horizontal(|ui| {
        ui.add_space(space::XL);
        ui.label(
            RichText::new("Teams")
                .font(theme::semibold(font_size::TITLE))
                .color(theme::fg1()),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add_space(space::MD);
            let compose_enabled = !state.remote.has_selection();
            ui.add_enabled_ui(compose_enabled, |ui| {
                if icon_button(ui, Icon::Compose, "New post in channel").clicked() {
                    state.open_channel_composer();
                }
            });
            if icon_button(ui, Icon::Search, "Search teams and channels").clicked() {
                state.toggle_team_filter();
            }
            let more = icon_button(ui, Icon::More, "More options");
            egui::Popup::menu(&more).show(|ui| {
                if ui.button("Clear team search").clicked() {
                    state.clear_team_filter();
                    ui.close();
                }
                if ui.button("Manage communities").clicked() {
                    state.open_community_picker();
                    ui.close();
                }
            });
        });
    });
    if state.team_filter_open {
        ui.horizontal(|ui| {
            ui.add_space(space::XL);
            ui.add_sized(
                [ui.available_width() - space::XL * 2.0, 30.0],
                egui::TextEdit::singleline(&mut state.team_filter_query)
                    .hint_text("Search teams and channels"),
            );
        });
        ui.add_space(space::SM);
    }
    // Collect the row intent first, then mutate after the scroll content has
    // finished borrowing the local membership list. This keeps a clicked row
    // from becoming a stale sidebar selection during this frame.
    let mut removal_requested = None;
    egui::ScrollArea::vertical()
        .id_salt("teams_sidebar_list")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(space::MD);
            // "Quick views" has no local content; its collapsed state is a
            // per-session view detail and is deliberately not persisted.
            let quick_views_id = ui.id().with("quick_views_open");
            let mut quick_views_open = ui.data(|data| data.get_temp(quick_views_id).unwrap_or(true));
            if section_header(ui, "Quick views", &mut quick_views_open).clicked() {
                ui.data_mut(|data| data.insert_temp(quick_views_id, quick_views_open));
            }
            ui.add_space(space::XS);
            let mut teams_open = state.teams_and_channels_open;
            if section_header(ui, "Teams and channels", &mut teams_open).clicked() {
                state.toggle_teams_and_channels();
            }
            if state.teams_and_channels_open {
                let team_filter = state.team_filter_query.trim().to_lowercase();
                let remote_selected = state.remote.has_selection();
                let mut shown_teams = 0;
                for team in team_catalog() {
                    if !team_matches_filter(&team_filter, team) {
                        continue;
                    }
                    shown_teams += 1;
                    let expanded = state.is_team_expanded(team.name);
                    if draw_team_row(ui, team, expanded)
                        .on_hover_text(if expanded {
                            "Collapse team channels"
                        } else {
                            "Expand team channels"
                        })
                        .clicked()
                    {
                        state.toggle_team_expansion(team.name);
                    }
                    if !expanded && team_filter.is_empty() {
                        continue;
                    }
                    for channel in team.channels {
                        let channel_key = channel_key(team, channel);
                        // Remote and local selections are separate session
                        // domains. Do not make a persisted local key look
                        // selected while a Microsoft 365 channel is active.
                        let selected = !remote_selected && state.selected_channel == channel_key;
                        let last_activity = channel.posts.first().map_or("", |post| post.date);
                        let (row, more) = draw_channel_row(ui, channel.name, last_activity, selected);
                        if let Some(more) = &more {
                            egui::Popup::menu(more).show(|ui| {
                                if ui.button("Open channel notes").clicked() {
                                    state.team_tab = TeamTab::Notes;
                                    ui.close();
                                }
                                if ui.button("View shared content").clicked() {
                                    state.team_tab = TeamTab::Shared;
                                    ui.close();
                                }
                                if ui
                                    .add_enabled(can_start_meeting, egui::Button::new("Meet now"))
                                    .on_hover_text(if can_start_meeting {
                                        "Start a local channel meeting"
                                    } else {
                                        "End the active local call or meeting before starting another"
                                    })
                                    .clicked()
                                {
                                    action = Some(TeamsAction::StartMeeting);
                                    ui.close();
                                }
                            });
                        }
                        // The action button sits above the row, so its clicks never reach `row`.
                        if row.clicked() {
                            state.select_team_channel(&channel_key);
                        }
                    }
                }
                if shown_teams == 0 {
                    ui.horizontal(|ui| {
                        ui.add_space(space::XL);
                        ui.label(
                            RichText::new("No teams or channels match your search.")
                                .font(theme::regular(font_size::CAPTION))
                                .color(theme::fg3()),
                        );
                    });
                }
                if link_row(ui, "See all your teams").clicked() {
                    state.show_all_teams();
                    toast.show("All teams are shown");
                }
            }
            let remote_filter = state.team_filter_query.trim().to_lowercase();
            if super::remote::draw_directory(ui, &mut state.remote, &remote_filter) {
                state.on_remote_channel_selected();
                action = state.remote_channel_selection_action();
            }
            ui.add_space(space::MD);
            let mut communities_open = state.communities_open;
            if section_header(ui, "Communities", &mut communities_open).clicked() {
                state.toggle_communities();
            }
            if state.communities_open {
                ui.horizontal(|ui| {
                    ui.add_space(space::XL + space::XL);
                    ui.label(
                        RichText::new(REFERENCE_COMMUNITY_NAME)
                            .font(theme::regular(font_size::CAPTION))
                            .color(theme::fg3()),
                    );
                });
                for community in &state.joined_communities {
                    ui.horizontal(|ui| {
                        ui.add_space(space::XL + space::XL);
                        ui.label(
                            RichText::new(community)
                                .font(theme::regular(font_size::CAPTION))
                                .color(theme::fg3()),
                        );
                        let remove = ui.small_button("Remove local").on_hover_text(
                            "Remove from this local preview only. This does not leave a real Teams community.",
                        );
                        if remove.clicked() {
                            removal_requested = Some(community.clone());
                        }
                    });
                }
                if link_row(ui, "Join communities").clicked() {
                    state.open_community_picker();
                }
            }
        });
    if let Some(community) = removal_requested {
        // Preview membership is state-only: removal deliberately emits no
        // TeamsAction and therefore no provider or transport intent.
        if state.remove_joined_community(&community) {
            toast.show(format!("Removed {community} from local preview"));
        }
    }
    draw_community_picker(ui.ctx(), state, toast);
    action
}

/// Team row: expand chevron, rounded-square avatar and the team name.
fn draw_team_row(ui: &mut Ui, team: &TeamDefinition, expanded: bool) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_HEIGHT), Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::CollapsingHeader,
            ui.is_enabled(),
            team.name,
        )
    });
    if !ui.is_rect_visible(rect) {
        return response;
    }
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(radius::SM), theme::surface_hover());
    }
    let chevron = if expanded {
        Icon::ChevronDown
    } else {
        Icon::ChevronRight
    };
    let side = IconSize::S16.px();
    chevron.image(IconSize::S16).tint(theme::fg3()).paint_at(
        ui,
        egui::Rect::from_min_size(
            egui::pos2(rect.left() + ROW_INSET, rect.center().y - side / 2.0),
            egui::vec2(side, side),
        ),
    );
    let avatar_side = AvatarSize::Sm24.px();
    let avatar_rect = egui::Rect::from_min_size(
        egui::pos2(
            rect.left() + ROW_INSET + side + space::SM,
            rect.center().y - avatar_side / 2.0,
        ),
        egui::vec2(avatar_side, avatar_side),
    );
    Avatar::new(team.name, AvatarSize::Sm24)
        .rounded_square()
        .paint(ui, avatar_rect);
    let text_left = avatar_rect.right() + space::MD;
    let galley = layout_truncated(
        ui,
        team.name,
        theme::regular(font_size::BODY),
        theme::fg1(),
        rect.right() - text_left - ROW_INSET,
    );
    ui.painter().galley(
        egui::pos2(text_left, rect.center().y - galley.size().y / 2.0),
        galley,
        theme::fg1(),
    );
    response
}

/// Indented channel row. The selected row is a white rounded pill with a
/// semibold name and a "more" action; others show their last-activity date.
fn draw_channel_row(
    ui: &mut Ui,
    name: &str,
    last_activity: &str,
    selected: bool,
) -> (egui::Response, Option<egui::Response>) {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_HEIGHT), Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            ui.is_enabled(),
            selected,
            name,
        )
    });
    let mut more = None;
    if ui.is_rect_visible(rect) {
        let inner = rect.shrink2(egui::vec2(ROW_INSET, 0.0));
        if selected {
            ui.painter().rect(
                inner,
                CornerRadius::same(radius::MD),
                theme::surface(),
                egui::Stroke::new(1.0_f32, theme::stroke_subtle()),
                egui::StrokeKind::Inside,
            );
        } else if response.hovered() {
            ui.painter().rect_filled(
                inner,
                CornerRadius::same(radius::MD),
                theme::surface_hover(),
            );
        }
        let right_edge = if selected {
            let button = egui::Rect::from_min_size(
                egui::pos2(inner.right() - ACTION_SIZE - space::XS, rect.top()),
                egui::vec2(ACTION_SIZE, ROW_HEIGHT),
            );
            more = Some(ui.put(button, |ui: &mut Ui| {
                icon_button(ui, Icon::More, "Channel options")
            }));
            button.left()
        } else {
            let date = layout_truncated(
                ui,
                last_activity,
                theme::regular(font_size::CAPTION),
                theme::fg3(),
                inner.width() * 0.4,
            );
            let left = inner.right() - space::MD - date.size().x;
            ui.painter().galley(
                egui::pos2(left, rect.center().y - date.size().y / 2.0),
                date,
                theme::fg3(),
            );
            left
        };
        let (font, color) = if selected {
            (theme::semibold(font_size::BODY), theme::fg1())
        } else {
            (theme::regular(font_size::BODY), theme::fg2())
        };
        let text_left = rect.left() + CHANNEL_INDENT;
        let galley = layout_truncated(ui, name, font, color, right_edge - text_left - space::SM);
        ui.painter().galley(
            egui::pos2(text_left, rect.center().y - galley.size().y / 2.0),
            galley,
            color,
        );
    }
    (response, more)
}

/// Brand-colored text link row ("See all your teams", "Join communities").
fn link_row(ui: &mut Ui, label: &str) -> egui::Response {
    ui.horizontal(|ui| {
        ui.add_space(CHANNEL_INDENT);
        ui.add(egui::Link::new(
            RichText::new(label)
                .font(theme::regular(font_size::CAPTION))
                .color(theme::brand_fg()),
        ))
    })
    .inner
}

pub(super) fn draw_community_picker(
    ctx: &egui::Context,
    state: &mut TeamsState,
    toast: &mut ToastState,
) {
    if !state.community_picker_open {
        return;
    }
    if take_escape(ctx) {
        state.community_picker_open = false;
        return;
    }

    let mut selection = None;
    let mut close_requested = false;
    let _ = modal::show(ctx, "community_picker", 390.0, |ui| {
        modal::heading(ui, "Join a community");
        ui.add_space(4.0);
        ui.label(
            RichText::new("Choose a local preview community to add.")
                .font(theme::regular(font_size::CAPTION))
                .color(theme::fg1()),
        );
        ui.label(
            RichText::new("Membership is saved in this local profile.")
                .font(theme::regular(font_size::CAPTION))
                .color(theme::fg3()),
        );
        ui.add_space(10.0);
        for community in ["Craft fairs", "Photo walks", "Sourdough swap"] {
            if ui.button(format!("＋  {community}")).clicked() {
                selection = Some(community);
            }
        }
        ui.add_space(10.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.button("Cancel").clicked() {
                close_requested = true;
            }
        });
    });

    if let Some(community) = selection {
        let joined = state.join_community(community);
        toast.show(if joined {
            format!("Joined {community}")
        } else {
            format!("You already joined {community}")
        });
        close_requested = true;
    }
    if close_requested {
        state.community_picker_open = false;
    }
}
