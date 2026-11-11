use super::{
    RemoteChannelFile, RemoteChannelFilesState, RemoteChannelMessagesState,
    RemoteChannelRepliesState, RemoteTeamDirectory, RemoteTeamsState,
};
use crate::integrations::{ProviderResourceRef, RemoteMessageReaction};
use crate::shared::reaction::ReactionKind;
use crate::ui::a11y::clickable;
use crate::ui::avatar::{Avatar, AvatarSize};
use crate::ui::icon::{Icon, IconSize};
use crate::ui::theme::{self, font_size, radius};
use eframe::egui::{self, Align, Color32, CornerRadius, Layout, RichText, Stroke, Vec2};

/// Draws the Microsoft 365 directory as a distinct, read-only hierarchy.
/// Returns immediately when no remote snapshot is present so the caller can
/// preserve the local preview layout unchanged.
pub(in crate::features::teams) fn draw_directory(
    ui: &mut egui::Ui,
    state: &mut RemoteTeamsState,
    filter: &str,
) -> bool {
    if state.is_empty() {
        return false;
    }

    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.add_space(18.0);
        ui.label(
            RichText::new("Microsoft 365")
                .font(theme::semibold(font_size::CAPTION))
                .color(theme::brand_fg()),
        );
    });
    ui.horizontal(|ui| {
        ui.add_space(18.0);
        ui.label(
            RichText::new("Read-only teams and channels from your signed-in account")
                .font(theme::regular(font_size::CAPTION))
                .color(theme::fg3()),
        );
    });
    ui.add_space(7.0);

    // Borrow the bounded snapshot for this frame instead of cloning every
    // team and channel on each repaint. Clicks are recorded and applied after
    // the loop, when no row borrow is outstanding.
    let mut shown = 0;
    let mut toggled_team: Option<ProviderResourceRef> = None;
    let mut selected_channel: Option<(ProviderResourceRef, ProviderResourceRef)> = None;
    for team in &state.teams {
        if !team_matches_filter(filter, team) {
            continue;
        }
        shown += 1;
        let expanded = state.is_expanded(&team.target);
        let header = ui.horizontal(|ui| {
            ui.add_space(17.0);
            ui.add(
                if expanded {
                    Icon::ChevronDown
                } else {
                    Icon::ChevronRight
                }
                .image(IconSize::S16),
            );
            Avatar::new(&team.display_name, AvatarSize::Sm24)
                .rounded_square()
                .show(ui);
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(&team.display_name)
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg1()),
                );
                if !team.description.is_empty() {
                    ui.label(
                        RichText::new(&team.description)
                            .font(theme::regular(font_size::CAPTION))
                            .color(theme::fg3()),
                    );
                }
            });
        });
        let toggle_label = if expanded {
            "Collapse Microsoft 365 team channels"
        } else {
            "Expand Microsoft 365 team channels"
        };
        if clickable(
            ui,
            header.response,
            &format!("{toggle_label}: {}", team.display_name),
        )
        .on_hover_text(toggle_label)
        .clicked()
        {
            toggled_team = Some(team.target.clone());
        }

        if !expanded && filter.is_empty() {
            ui.add_space(6.0);
            continue;
        }
        for channel in &team.channels {
            if !filter.is_empty()
                && !contains_folded(&channel.display_name, filter)
                && !contains_folded(&channel.description, filter)
            {
                continue;
            }
            let selected = state.selected_channel_is(&team.target, &channel.target);
            let row = egui::Frame::new()
                .fill(if selected {
                    theme::surface()
                } else {
                    Color32::TRANSPARENT
                })
                .stroke(if selected {
                    Stroke::new(1.0_f32, theme::fg1())
                } else {
                    Stroke::NONE
                })
                .corner_radius(CornerRadius::same(radius::MD))
                .inner_margin(egui::Margin::symmetric(8, 6))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.add_space(43.0);
                        ui.label(
                            RichText::new(&channel.display_name)
                                .font(theme::semibold(font_size::CAPTION))
                                .color(if selected { theme::fg1() } else { theme::fg3() }),
                        );
                        if channel.archived {
                            ui.label(
                                RichText::new("Archived")
                                    .font(theme::regular(font_size::CAPTION))
                                    .color(theme::fg3()),
                            );
                        }
                    });
                });
            if clickable(ui, row.response, &channel.display_name).clicked() {
                selected_channel = Some((team.target.clone(), channel.target.clone()));
            }
        }
        ui.add_space(6.0);
    }
    if shown == 0 {
        ui.horizontal(|ui| {
            ui.add_space(22.0);
            ui.label(
                RichText::new("No Microsoft 365 teams or channels match your search.")
                    .font(theme::regular(font_size::CAPTION))
                    .color(theme::fg3()),
            );
        });
    }
    if let Some(target) = toggled_team {
        state.toggle_expanded(target);
    }
    selected_channel.is_some_and(|(team, channel)| state.select(team, channel))
}

/// A UI intent emitted by the selected Microsoft 365 channel. Mutations stay
/// narrowly limited to text-only root posts/replies and bounded-palette
/// reactions on their visible root posts or direct replies.
pub(in crate::features::teams) enum RemoteChannelPageAction {
    RefreshPosts,
    SelectPost(ProviderResourceRef),
    ToggleReaction {
        post: ProviderResourceRef,
        reaction: ReactionKind,
        selected: bool,
    },
    ToggleReplyReaction {
        reply: ProviderResourceRef,
        reaction: ReactionKind,
        selected: bool,
    },
    RefreshReplies,
    ShowPosts,
    ShowFiles,
    RefreshFiles,
    OpenFile(ProviderResourceRef),
    SubmitPost,
    SubmitReply,
}

/// Draws a selected remote channel. Its session-only Posts surface permits an
/// explicit text-only root post, a text-only reply to a selected root, and a
/// provider-synchronized bounded-palette reaction on a visible root post or
/// direct reply. Files, custom/tenant-configured reactions, notes, meetings,
/// local-feed actions, reply-to-reply, and every file-content operation remain
/// unavailable.
pub(in crate::features::teams) fn draw_selected_channel(
    ui: &mut egui::Ui,
    state: &mut RemoteTeamsState,
) -> Option<RemoteChannelPageAction> {
    let (
        team_display_name,
        channel_display_name,
        channel_description,
        channel_membership_label,
        channel_archived,
    ) = {
        let (team, channel) = state.selected()?;
        (
            team.display_name.clone(),
            channel.display_name.clone(),
            channel.description.clone(),
            channel.membership_label.clone(),
            channel.archived,
        )
    };
    let mut refresh_requested = false;
    let mut reply_feed_action = None;
    let mut refresh_files_requested = false;
    let mut show_posts_requested = false;
    let mut show_files_requested = false;
    let mut file_open_requested = None;
    let mut submit_post_requested = false;
    let mut submit_reply_requested = false;
    let mut message_feed_action = None;
    let showing_files = state.is_showing_files();
    let compact_header = ui.available_width() < 780.0;
    egui::TopBottomPanel::top("remote_team_channel_header")
        .exact_height(if compact_header { 94.0 } else { 74.0 })
        .frame(
            egui::Frame::new()
                .fill(theme::surface())
                .inner_margin(egui::Margin::symmetric(22, 8))
                .stroke(Stroke::new(1.0_f32, theme::stroke_subtle())),
        )
        .show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                Avatar::new(&team_display_name, AvatarSize::Md32)
                    .rounded_square()
                    .show(ui);
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(&channel_display_name)
                            .font(theme::semibold(font_size::SUBTITLE))
                            .color(theme::fg1()),
                    );
                    ui.label(
                        RichText::new(format!("{} · Microsoft 365", team_display_name))
                            .font(theme::regular(font_size::CAPTION))
                            .color(theme::fg3()),
                    );
                    ui.horizontal(|ui| {
                        if ui
                            .selectable_label(!showing_files, "Posts")
                            .on_hover_text("Show this channel's read-only Microsoft 365 root posts")
                            .clicked()
                        {
                            show_posts_requested = true;
                        }
                        if ui
                            .selectable_label(showing_files, "Files")
                            .on_hover_text(
                                "Show this channel's bounded, read-only Microsoft 365 file metadata",
                            )
                            .clicked()
                        {
                            show_files_requested = true;
                        }
                    });
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if showing_files {
                        if ui
                            .button("Refresh files")
                            .on_hover_text(
                                "Read this selected channel's bounded Microsoft 365 file metadata again",
                            )
                            .clicked()
                        {
                            refresh_files_requested = true;
                        }
                    } else if ui
                        .button("Refresh posts")
                        .on_hover_text(
                            "Read this selected channel's bounded Microsoft 365 root-post feed again",
                        )
                        .clicked()
                    {
                        refresh_requested = true;
                    }
                });
            });
        });

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(32.0);
            ui.horizontal(|ui| {
                ui.add_space(32.0);
                ui.vertical(|ui| {
                    ui.set_max_width(720.0);
                    if showing_files {
                        ui.label(
                            RichText::new("Microsoft 365 channel files")
                                .font(theme::semibold(font_size::TITLE))
                                .color(theme::fg1()),
                        );
                        ui.add_space(7.0);
                        file_open_requested = draw_channel_file_feed(ui, &state.files);
                    } else {
                        ui.label(
                            RichText::new("Microsoft 365 channel posts")
                                .font(theme::semibold(font_size::TITLE))
                                .color(theme::fg1()),
                        );
                        ui.add_space(7.0);
                        message_feed_action =
                            draw_channel_message_feed(ui, state, channel_archived);
                        if state.reply_selection.is_some() {
                            ui.add_space(14.0);
                            reply_feed_action =
                                draw_channel_reply_feed(ui, state, channel_archived);
                            if !channel_archived {
                                ui.add_space(10.0);
                                submit_reply_requested = draw_channel_reply_composer(ui, state);
                            }
                        }
                        ui.add_space(14.0);
                        if channel_archived {
                            egui::Frame::new()
                                .fill(theme::surface())
                                .stroke(Stroke::new(
                                    1.0_f32,
                                    theme::stroke(),
                                ))
                                .corner_radius(CornerRadius::same(radius::LG))
                                .inner_margin(egui::Margin::symmetric(14, 12))
                                .show(ui, |ui| {
                                    ui.label(
                                        RichText::new("This Microsoft 365 channel is archived.")
                                            .font(theme::regular(font_size::CAPTION))
                                            .color(theme::fg1()),
                                    );
                                    ui.label(
                                        RichText::new(
                                            "New remote root posts and replies are unavailable.",
                                        )
                                            .font(theme::regular(font_size::CAPTION))
                                            .color(theme::fg3()),
                                    );
                                });
                        } else {
                            submit_post_requested = draw_channel_post_composer(ui, state);
                        }
                    }
                    ui.add_space(18.0);
                    egui::Frame::new()
                        .fill(theme::surface())
                        .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
                        .corner_radius(CornerRadius::same(radius::LG))
                        .inner_margin(egui::Margin::symmetric(14, 12))
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new("Channel details")
                                    .font(theme::semibold(font_size::CAPTION))
                                    .color(theme::fg1()),
                            );
                            ui.add_space(4.0);
                            if !channel_description.is_empty() {
                                ui.label(
                                    RichText::new(&channel_description).font(theme::regular(font_size::CAPTION)).color(theme::fg3()),
                                );
                            } else {
                                ui.label(
                                    RichText::new("No channel description was supplied.")
                                        .font(theme::regular(font_size::CAPTION))
                                        .color(theme::fg3()),
                                );
                            }
                            ui.add_space(6.0);
                            ui.label(
                                RichText::new(format!(
                                    "Membership: {}{}",
                                    channel_membership_label,
                                    if channel_archived { " · Archived" } else { "" }
                                ))
                                .font(theme::regular(font_size::CAPTION))
                                .color(theme::fg3()),
                            );
                        });
                    ui.add_space(16.0);
                    ui.label(
                        RichText::new(
                            "Remote root posts and replies are text-only. Supported reactions are available on visible root posts and replies; editing, deleting, custom reactions, file content actions, notes, meetings, and other remote mutations remain unavailable.",
                        )
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg3()),
                    );
                });
            });
        });

    if show_posts_requested {
        Some(RemoteChannelPageAction::ShowPosts)
    } else if show_files_requested {
        Some(RemoteChannelPageAction::ShowFiles)
    } else if refresh_requested {
        Some(RemoteChannelPageAction::RefreshPosts)
    } else if let Some(reply_action) = reply_feed_action {
        Some(reply_action)
    } else if refresh_files_requested {
        Some(RemoteChannelPageAction::RefreshFiles)
    } else if let Some(file) = file_open_requested {
        Some(RemoteChannelPageAction::OpenFile(file))
    } else if submit_reply_requested {
        Some(RemoteChannelPageAction::SubmitReply)
    } else if submit_post_requested {
        Some(RemoteChannelPageAction::SubmitPost)
    } else {
        message_feed_action
    }
}

/// Renders the deliberately narrow root-post composer. Its pending and
/// unknown-delivery states are feature-owned only: neither inserts a local
/// row nor treats a refreshed bounded feed as a delivery receipt.
fn draw_channel_post_composer(ui: &mut egui::Ui, state: &mut RemoteTeamsState) -> bool {
    let Some((team, channel)) = state.selected_targets() else {
        return false;
    };
    if state.channel_post_is_pending_elsewhere(&team, &channel) {
        egui::Frame::new()
            .fill(theme::surface())
            .stroke(Stroke::new(1.0_f32, theme::stroke()))
            .corner_radius(CornerRadius::same(radius::LG))
            .inner_margin(egui::Margin::symmetric(14, 12))
            .show(ui, |ui| {
                ui.label(
                    RichText::new(
                        "A Microsoft 365 channel message is awaiting a delivery outcome in another target.",
                    )
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg1()),
                );
                ui.label(
                    RichText::new(
                        "Return to that target to review it before sending another message.",
                    )
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg3()),
                );
            });
        return false;
    }

    let mut submit_requested = false;
    let mut make_editable_requested = false;
    {
        let composer = state
            .selected_channel_post_composer_mut()
            .expect("the checked selected channel must retain a composer");
        egui::Frame::new()
            .fill(theme::surface())
            .stroke(Stroke::new(1.0_f32, theme::stroke()))
            .corner_radius(CornerRadius::same(radius::LG))
            .inner_margin(egui::Margin::symmetric(14, 12))
            .show(ui, |ui| {
                ui.label(RichText::new("Post in Microsoft 365 channel").strong().color(theme::fg1()));
                ui.add_space(5.0);
                if composer.delivery_unknown.is_some() {
                    ui.label(
                        RichText::new("Delivery status unknown")
                            .font(theme::semibold(font_size::CAPTION))
                            .color(theme::fg1()),
                    );
                    ui.label(
                        RichText::new(
                            "Microsoft 365 could not confirm whether the prior post was delivered. A refreshed feed cannot prove that it was not delivered.",
                        )
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg3()),
                    );
                    ui.add_space(7.0);
                    if ui
                        .button("Make draft editable")
                        .on_hover_text(
                            "Review the retained text before deciding whether to submit a new post.",
                        )
                        .clicked()
                    {
                        make_editable_requested = true;
                    }
                } else if composer.pending.is_some() {
                    ui.label(
                        RichText::new("Sending your text-only Microsoft 365 channel post…")
                            .font(theme::regular(font_size::CAPTION))
                            .color(theme::fg1()),
                    );
                    ui.label(
                        RichText::new("The draft stays locked to avoid duplicate delivery.")
                            .font(theme::regular(font_size::CAPTION))
                            .color(theme::fg3()),
                    );
                } else {
                    ui.add_sized(
                        [ui.available_width(), 88.0],
                        egui::TextEdit::multiline(&mut composer.draft)
                            .hint_text("Share a text-only update with this channel"),
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("Text only · maximum 64 KiB")
                                .font(theme::regular(font_size::CAPTION))
                                .color(theme::fg3()),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui
                                .add(
                                    egui::Button::new(RichText::new("Post").color(theme::fg_on_brand()))
                                        .fill(theme::brand())
                                        .corner_radius(CornerRadius::same(radius::MD)),
                                )
                                .clicked()
                            {
                                submit_requested = true;
                            }
                        });
                    });
                    ui.label(
                        RichText::new(
                            "Attachments, mentions, rich formatting, custom reactions, edits, and deletion are unavailable.",
                        )
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg3()),
                    );
                }
            });
    }
    if make_editable_requested {
        // This is intentionally a local state transition only. It neither
        // creates a command nor reuses the ambiguous request automatically.
        let _ = state.make_uncertain_channel_post_editable();
    }
    submit_requested
}

/// Renders the only supported remote reply mutation: text sent to the exact
/// selected root post. It shares the root-post delivery gate and never treats
/// a refreshed bounded reply view as evidence for an ambiguous write.
fn draw_channel_reply_composer(ui: &mut egui::Ui, state: &mut RemoteTeamsState) -> bool {
    if state.selected_reply_delivery_target().is_none() {
        return false;
    }
    if state.channel_reply_is_pending_elsewhere() {
        egui::Frame::new()
            .fill(theme::surface())
            .stroke(Stroke::new(1.0_f32, theme::stroke()))
            .corner_radius(CornerRadius::same(radius::LG))
            .inner_margin(egui::Margin::symmetric(14, 12))
            .show(ui, |ui| {
                ui.label(
                    RichText::new(
                        "A Microsoft 365 channel message is awaiting a delivery outcome in another target.",
                    )
                    .font(theme::regular(font_size::CAPTION))
                    .color(theme::fg1()),
                );
                ui.label(
                    RichText::new(
                        "Return to that target to review it before sending another message.",
                    )
                    .font(theme::regular(font_size::CAPTION))
                    .color(theme::fg3()),
                );
            });
        return false;
    }

    let mut submit_requested = false;
    let mut make_editable_requested = false;
    {
        let composer = state
            .selected_channel_reply_composer_mut()
            .expect("the checked selected root post must retain a reply composer");
        egui::Frame::new()
            .fill(theme::surface())
            .stroke(Stroke::new(1.0_f32, theme::stroke()))
            .corner_radius(CornerRadius::same(radius::LG))
            .inner_margin(egui::Margin::symmetric(14, 12))
            .show(ui, |ui| {
                ui.label(RichText::new("Reply in Microsoft 365 channel").strong().color(theme::fg1()));
                ui.add_space(5.0);
                if composer.delivery_unknown.is_some() {
                    ui.label(
                        RichText::new("Delivery status unknown")
                            .font(theme::semibold(font_size::CAPTION))
                            .color(theme::fg1()),
                    );
                    ui.label(
                        RichText::new(
                            "Microsoft 365 could not confirm whether the prior reply was delivered. A refreshed bounded reply view cannot prove that it was not delivered.",
                        )
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg3()),
                    );
                    ui.add_space(7.0);
                    if ui
                        .button("Make draft editable")
                        .on_hover_text(
                            "Review the retained text before deciding whether to send a new reply.",
                        )
                        .clicked()
                    {
                        make_editable_requested = true;
                    }
                } else if composer.pending.is_some() {
                    ui.label(
                        RichText::new("Sending your text-only Microsoft 365 reply…")
                            .font(theme::regular(font_size::CAPTION))
                            .color(theme::fg1()),
                    );
                    ui.label(
                        RichText::new("The draft stays locked to avoid duplicate delivery.")
                            .font(theme::regular(font_size::CAPTION))
                            .color(theme::fg3()),
                    );
                } else {
                    ui.add_sized(
                        [ui.available_width(), 72.0],
                        egui::TextEdit::multiline(&mut composer.draft)
                            .hint_text("Reply to this root post with text only"),
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("Text only · maximum 64 KiB")
                                .font(theme::regular(font_size::CAPTION))
                                .color(theme::fg3()),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui
                                .add(
                                    egui::Button::new(RichText::new("Reply").color(theme::fg_on_brand()))
                                        .fill(theme::brand())
                                        .corner_radius(CornerRadius::same(radius::MD)),
                                )
                                .clicked()
                            {
                                submit_requested = true;
                            }
                        });
                    });
                    ui.label(
                        RichText::new(
                            "Attachments, mentions, rich formatting, editing, deletion, custom reactions, and reply-to-reply are unavailable.",
                        )
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg3()),
                    );
                }
            });
    }
    if make_editable_requested {
        // Manual review is a feature-local state transition only. It never
        // retries or reuses the ambiguous request automatically.
        let _ = state.make_uncertain_channel_reply_editable();
    }
    submit_requested
}

/// Draws one provider-synchronized reaction pill. Its caller owns opaque
/// targets and turns a click into a typed page action; this helper receives
/// only presentation state so it cannot manufacture a provider route.
fn draw_remote_reaction_pill(
    ui: &mut egui::Ui,
    kind: ReactionKind,
    reaction: Option<RemoteMessageReaction>,
    pending: bool,
    channel_archived: bool,
    message_deleted: bool,
    message_label: &str,
) -> bool {
    let count = reaction.map_or(0, |reaction| reaction.count());
    let selected = reaction.is_some_and(|reaction| reaction.reacted_by_current_user());
    let label = if count == 0 {
        kind.icon().into()
    } else {
        format!("{} {count}", kind.icon())
    };
    let can_react = !channel_archived && !message_deleted && !pending;
    ui.add_enabled(
        can_react,
        egui::Button::new(RichText::new(label).color(if selected {
            theme::brand_fg()
        } else {
            theme::fg1()
        }))
        .fill(if selected {
            theme::brand_tint()
        } else {
            Color32::TRANSPARENT
        })
        .stroke(Stroke::new(
            1.0_f32,
            if selected {
                theme::brand_fg()
            } else {
                theme::stroke_subtle()
            },
        ))
        .corner_radius(CornerRadius::same(radius::MD)),
    )
    .on_hover_text(if pending {
        format!(
            "Waiting for this {message_label}'s Microsoft 365 {} reaction update",
            kind.label()
        )
    } else if channel_archived {
        "Archived Microsoft 365 channels cannot receive new reactions".into()
    } else if message_deleted {
        "Deleted Microsoft 365 messages cannot receive reactions".into()
    } else if selected {
        format!("Remove your {} reaction", kind.label())
    } else {
        format!("Add a {} reaction", kind.label())
    })
    .clicked()
}

fn draw_channel_message_feed(
    ui: &mut egui::Ui,
    state: &RemoteTeamsState,
    channel_archived: bool,
) -> Option<RemoteChannelPageAction> {
    let (team, channel) = state.selected_targets()?;
    match &state.messages {
        RemoteChannelMessagesState::None | RemoteChannelMessagesState::Loading => {
            ui.label(
                RichText::new("Loading a bounded Microsoft 365 root-post feed…")
                    .font(theme::regular(font_size::CAPTION))
                    .color(theme::fg3()),
            );
            None
        }
        RemoteChannelMessagesState::AccessDenied => {
            ui.label(
                RichText::new("Microsoft 365 denied access to this channel's posts.")
                    .font(theme::regular(font_size::CAPTION))
                    .color(theme::fg1()),
            );
            ui.label(
                RichText::new(
                    "Verify channel membership and delegated ChannelMessage.Read.All, then reconnect or refresh.",
                )
                .font(theme::regular(font_size::CAPTION))
                .color(theme::fg3()),
            );
            None
        }
        RemoteChannelMessagesState::Ready(posts) if posts.is_empty() => {
            ui.label(
                RichText::new("No root posts were returned in this bounded Microsoft 365 view.")
                    .font(theme::regular(font_size::CAPTION))
                    .color(theme::fg3()),
            );
            None
        }
        RemoteChannelMessagesState::Ready(posts) => {
            ui.label(
                RichText::new("Remote root posts from Microsoft 365")
                    .font(theme::regular(font_size::CAPTION))
                    .color(theme::brand_fg()),
            );
            ui.add_space(8.0);
            let mut requested_action = None;
            for post in posts {
                let is_selected = state
                    .reply_selection
                    .as_ref()
                    .is_some_and(|selection| selection.post == post.target);
                egui::Frame::new()
                    .fill(theme::surface())
                    .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
                    .corner_radius(CornerRadius::same(radius::LG))
                    .inner_margin(egui::Margin::symmetric(14, 12))
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                RichText::new(&post.author)
                                    .font(theme::semibold(font_size::CAPTION))
                                    .color(theme::fg1()),
                            );
                            ui.label(
                                RichText::new(&post.created_at)
                                    .font(theme::regular(font_size::CAPTION))
                                    .color(theme::fg3()),
                            );
                            if post.edited {
                                ui.label(
                                    RichText::new("Edited")
                                        .font(theme::regular(font_size::CAPTION))
                                        .color(theme::fg3()),
                                );
                            }
                            if post.deleted {
                                ui.label(
                                    RichText::new("Deleted")
                                        .font(theme::regular(font_size::CAPTION))
                                        .color(theme::fg3()),
                                );
                            }
                        });
                        if !post.subject.is_empty() {
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new(&post.subject)
                                    .font(theme::semibold(font_size::CAPTION))
                                    .color(theme::fg1()),
                            );
                        }
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(&post.plain_text)
                                .font(theme::regular(font_size::CAPTION))
                                .color(theme::fg1()),
                        );
                        ui.add_space(8.0);
                        ui.horizontal_wrapped(|ui| {
                            let mut any_reaction_pending = false;
                            for reaction_kind in ReactionKind::ALL {
                                let reaction_pending = state.post_reaction_is_pending(
                                    &team,
                                    &channel,
                                    &post.target,
                                    reaction_kind,
                                );
                                any_reaction_pending |= reaction_pending;
                                let reaction = post.reaction(reaction_kind);
                                let selected = reaction
                                    .is_some_and(|reaction| reaction.reacted_by_current_user());
                                let clicked = draw_remote_reaction_pill(
                                    ui,
                                    reaction_kind,
                                    reaction,
                                    reaction_pending,
                                    channel_archived,
                                    post.deleted,
                                    "root post",
                                );
                                if requested_action.is_none() && clicked {
                                    requested_action = Some(
                                        RemoteChannelPageAction::ToggleReaction {
                                            post: post.target.clone(),
                                            reaction: reaction_kind,
                                            selected: !selected,
                                        },
                                    );
                                }
                            }
                            if any_reaction_pending {
                                ui.label(
                                    RichText::new("Updating reaction…")
                                        .font(theme::regular(font_size::CAPTION))
                                        .color(theme::fg3()),
                                );
                            }
                            if is_selected {
                                ui.label(
                                    RichText::new("Viewing replies")
                                        .font(theme::regular(font_size::CAPTION))
                                        .color(theme::brand_fg()),
                                );
                            } else if requested_action.is_none()
                                && ui
                                    .small_button("View replies")
                                    .on_hover_text(
                                        "Read this root post's bounded Microsoft 365 reply projection",
                                    )
                                    .clicked()
                            {
                                requested_action =
                                    Some(RemoteChannelPageAction::SelectPost(post.target.clone()));
                            }
                        });
                    });
                ui.add_space(10.0);
            }
            requested_action
        }
    }
}

fn draw_channel_reply_feed(
    ui: &mut egui::Ui,
    state: &RemoteTeamsState,
    channel_archived: bool,
) -> Option<RemoteChannelPageAction> {
    let (team, channel, post) = state.selected_reply_targets()?;
    let mut requested_action = None;
    egui::Frame::new()
        .fill(theme::surface())
        .stroke(Stroke::new(1.0_f32, theme::stroke()))
        .corner_radius(CornerRadius::same(radius::LG))
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Replies").font(theme::semibold(font_size::BODY)).color(theme::fg1()));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if requested_action.is_none()
                        && ui
                        .small_button("Refresh replies")
                        .on_hover_text(
                            "Read this selected root post's bounded Microsoft 365 reply view again",
                        )
                        .clicked()
                    {
                        requested_action = Some(RemoteChannelPageAction::RefreshReplies);
                    }
                });
            });
            ui.add_space(5.0);
            match &state.replies {
                RemoteChannelRepliesState::None | RemoteChannelRepliesState::Loading => {
                    ui.label(
                        RichText::new("Loading a bounded Microsoft 365 reply view…")
                            .font(theme::regular(font_size::CAPTION))
                            .color(theme::fg3()),
                    );
                }
                RemoteChannelRepliesState::AccessDenied => {
                    ui.label(
                        RichText::new("Microsoft 365 denied access to this root post's replies.")
                            .font(theme::regular(font_size::CAPTION))
                            .color(theme::fg1()),
                    );
                    ui.label(
                        RichText::new(
                            "Verify channel membership and delegated ChannelMessage.Read.All, then reconnect or use Refresh replies.",
                        )
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg3()),
                    );
                }
                RemoteChannelRepliesState::Ready(replies) if replies.is_empty() => {
                    ui.label(
                        RichText::new("No replies were returned in this bounded Microsoft 365 view.")
                            .font(theme::regular(font_size::CAPTION))
                            .color(theme::fg3()),
                    );
                }
                RemoteChannelRepliesState::Ready(replies) => {
                    ui.label(
                        RichText::new("Remote replies from Microsoft 365")
                            .font(theme::regular(font_size::CAPTION))
                            .color(theme::brand_fg()),
                    );
                    ui.add_space(8.0);
                    for reply in replies {
                        egui::Frame::new()
                            .fill(theme::surface())
                            .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
                            .corner_radius(CornerRadius::same(radius::MD))
                            .inner_margin(egui::Margin::symmetric(12, 10))
                            .show(ui, |ui| {
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(
                                        RichText::new(&reply.author).font(theme::semibold(font_size::CAPTION)).color(theme::fg1()),
                                    );
                                    ui.label(
                                        RichText::new(&reply.created_at).font(theme::regular(font_size::CAPTION)).color(theme::fg3()),
                                    );
                                    if reply.edited {
                                        ui.label(RichText::new("Edited").font(theme::regular(font_size::CAPTION)).color(theme::fg3()));
                                    }
                                    if reply.deleted {
                                        ui.label(RichText::new("Deleted").font(theme::regular(font_size::CAPTION)).color(theme::fg3()));
                                    }
                                });
                                ui.add_space(4.0);
                                ui.label(RichText::new(&reply.plain_text).font(theme::regular(font_size::CAPTION)).color(theme::fg1()));
                                ui.add_space(8.0);
                                ui.horizontal_wrapped(|ui| {
                                    let mut any_reaction_pending = false;
                                    for reaction_kind in ReactionKind::ALL {
                                        let reaction_pending = state.reply_reaction_is_pending(
                                            &team,
                                            &channel,
                                            &post,
                                            &reply.target,
                                            reaction_kind,
                                        );
                                        any_reaction_pending |= reaction_pending;
                                        let reaction = reply.reaction(reaction_kind);
                                        let selected = reaction.is_some_and(|reaction| {
                                            reaction.reacted_by_current_user()
                                        });
                                        let clicked = draw_remote_reaction_pill(
                                            ui,
                                            reaction_kind,
                                            reaction,
                                            reaction_pending,
                                            channel_archived,
                                            reply.deleted,
                                            "reply",
                                        );
                                        if requested_action.is_none() && clicked {
                                            requested_action = Some(
                                                RemoteChannelPageAction::ToggleReplyReaction {
                                                    reply: reply.target.clone(),
                                                    reaction: reaction_kind,
                                                    selected: !selected,
                                                },
                                            );
                                        }
                                    }
                                    if any_reaction_pending {
                                        ui.label(
                                            RichText::new("Updating reaction…")
                                                .font(theme::regular(font_size::CAPTION))
                                                .color(theme::fg3()),
                                        );
                                    }
                                });
                            });
                        ui.add_space(8.0);
                    }
                }
            }
            ui.add_space(5.0);
            ui.label(
                RichText::new(
                    "You can send a text-only reply and add/remove supported reactions on visible replies. Editing, deleting, custom reactions, rich formatting, attachments, and reply-to-reply remain unavailable.",
                )
                .font(theme::regular(font_size::CAPTION))
                .color(theme::fg3()),
            );
        });
    requested_action
}

/// Draws presentation-safe direct-child metadata. The only row interaction is
/// an explicit browser handoff for a file that carries a validated provider
/// destination; browsing folders, previewing, downloading, uploading,
/// renaming, moving, sharing, and deletion remain separate contracts.
fn draw_channel_file_feed(
    ui: &mut egui::Ui,
    files: &RemoteChannelFilesState,
) -> Option<ProviderResourceRef> {
    let mut open_requested = None;
    egui::Frame::new()
        .fill(theme::surface())
        .stroke(Stroke::new(1.0_f32, theme::stroke()))
        .corner_radius(CornerRadius::same(radius::LG))
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Channel files")
                        .font(theme::semibold(font_size::BODY))
                        .color(theme::fg1()),
                );
                ui.label(
                    RichText::new("Read-only metadata")
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::brand_fg()),
                );
            });
            ui.add_space(5.0);
            match files {
                RemoteChannelFilesState::None | RemoteChannelFilesState::Loading => {
                    ui.label(
                        RichText::new("Loading a bounded Microsoft 365 channel-files view…")
                            .font(theme::regular(font_size::CAPTION))
                            .color(theme::fg3()),
                    );
                }
                RemoteChannelFilesState::AccessDenied => {
                    ui.label(
                        RichText::new("Microsoft 365 denied access to this channel's files.")
                            .font(theme::regular(font_size::CAPTION))
                            .color(theme::fg1()),
                    );
                    ui.label(
                        RichText::new(
                            "Verify your channel membership and Microsoft 365 file access, then reconnect or use Refresh files.",
                        )
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg3()),
                    );
                }
                RemoteChannelFilesState::Ready(files) if files.is_empty() => {
                    ui.label(
                        RichText::new(
                            "No direct channel-folder items were returned in this bounded Microsoft 365 view.",
                        )
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg3()),
                    );
                }
                RemoteChannelFilesState::Ready(files) => {
                    ui.label(
                        RichText::new("Read-only direct children from Microsoft 365")
                            .font(theme::regular(font_size::CAPTION))
                            .color(theme::brand_fg()),
                    );
                    ui.add_space(8.0);
                    for file in files {
                        if let Some(target) = draw_channel_file_row(ui, file) {
                            // Only one pointer click is normally delivered in
                            // a frame; retaining the first makes the direct
                            // gesture boundary explicit regardless.
                            open_requested.get_or_insert(target);
                        }
                        ui.add_space(8.0);
                    }
                }
            }
            ui.add_space(5.0);
            ui.label(
                RichText::new(
                    "Open in browser is available only when Microsoft 365 supplies a link. Browsing, previewing, downloading, uploading, renaming, moving, sharing, and deletion are unavailable.",
                )
                .font(theme::regular(font_size::CAPTION))
                .color(theme::fg3()),
            );
        });
    open_requested
}

fn draw_channel_file_row(
    ui: &mut egui::Ui,
    file: &RemoteChannelFile,
) -> Option<ProviderResourceRef> {
    let mut open_requested = false;
    egui::Frame::new()
        .fill(theme::surface())
        .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
        .corner_radius(CornerRadius::same(radius::MD))
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (icon, _) = ui.allocate_exact_size(Vec2::new(34.0, 40.0), egui::Sense::hover());
                ui.painter()
                    .rect_filled(icon, CornerRadius::same(radius::MD), theme::brand_tint());
                ui.painter().text(
                    icon.center(),
                    egui::Align2::CENTER_CENTER,
                    &file.kind_label,
                    theme::semibold(font_size::CAPTION),
                    theme::brand_fg(),
                );
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(&file.name)
                            .font(theme::semibold(font_size::CAPTION))
                            .color(theme::fg1()),
                    );
                    let size = file
                        .size_bytes
                        .map_or_else(|| "Size unavailable".into(), format_channel_file_size);
                    ui.label(
                        RichText::new(format!("{} · {size}", file.modified_at))
                            .font(theme::regular(font_size::CAPTION))
                            .color(theme::fg3()),
                    );
                });
                let open_available = file.web_url.is_some();
                let open = ui
                    .add_enabled(open_available, egui::Button::new("Open in browser"))
                    .on_hover_text(if open_available {
                        "Open this Microsoft 365 channel item in your system browser"
                    } else {
                        "Microsoft 365 did not supply a browser link for this channel item"
                    });
                open_requested = open.clicked();
            });
        });
    open_requested.then(|| file.target.clone())
}

fn format_channel_file_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / KIB)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (KIB * KIB))
    } else {
        format!("{:.1} GB", bytes as f64 / (KIB * KIB * KIB))
    }
}

fn team_matches_filter(filter: &str, team: &RemoteTeamDirectory) -> bool {
    filter.is_empty()
        || contains_folded(&team.display_name, filter)
        || contains_folded(&team.description, filter)
        || team.channels.iter().any(|channel| {
            contains_folded(&channel.display_name, filter)
                || contains_folded(&channel.description, filter)
        })
}

fn contains_folded(value: &str, filter: &str) -> bool {
    value.to_lowercase().contains(filter)
}
