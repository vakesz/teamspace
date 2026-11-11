use super::{
    RemoteChannelDirectory, RemoteChannelFile, RemoteChannelFilesDirectory,
    RemoteChannelMessagesDirectory, RemoteChannelPost, RemoteChannelRepliesDirectory,
    RemoteChannelReply, RemoteTeamDirectory, RemoteTeamsDirectory, TeamsAction, TeamsState,
};
use crate::features::threads::ThreadStore;
use crate::integrations::{ProviderResourceRef, RemoteMessageReaction};
use crate::shared::reaction::ReactionKind;
use crate::ui::toast::ToastState;
use eframe::egui;

mod local_threads;
mod navigation_search;
mod remote_channel_reads;
mod remote_delivery;
mod remote_directory;
mod remote_post_reactions;
mod remote_refresh;
mod remote_reply_reactions;

fn render_selected_channel(state: &mut TeamsState, discussions: &mut ThreadStore) {
    let ctx = crate::ui::testing::themed_context();
    ctx.begin_pass(egui::RawInput::default());
    egui::CentralPanel::default().show(&ctx, |ui| {
        ui.set_min_width(516.0);
        let mut toast = ToastState::default();
        let _ = super::draw_teams_posts_page(ui, state, discussions, true, &mut toast);
    });
    let _ = ctx.end_pass();
}

fn render_open_thread(state: &mut TeamsState, discussions: &mut ThreadStore) {
    let ctx = crate::ui::testing::themed_context();
    ctx.begin_pass(egui::RawInput::default());
    egui::CentralPanel::default().show(&ctx, |ui| {
        let mut toast = ToastState::default();
        super::draw_inline_post_thread(ui, state, discussions, &mut toast);
    });
    let _ = ctx.end_pass();
}

fn render_teams_sidebar(state: &mut TeamsState) {
    let ctx = crate::ui::testing::themed_context();
    ctx.begin_pass(egui::RawInput::default());
    egui::CentralPanel::default().show(&ctx, |ui| {
        let mut toast = ToastState::default();
        let _ = super::draw_sidebar(ui, state, true, &mut toast);
    });
    let _ = ctx.end_pass();
}

/// Test-only opaque stand-ins. Production remote directories can only obtain
/// their Microsoft 365 targets from the integrations projection; this checks
/// feature state compares opaque values rather than display labels.
fn remote_directory(
    team_name: &str,
    channel_name: &str,
) -> (
    RemoteTeamsDirectory,
    ProviderResourceRef,
    ProviderResourceRef,
) {
    let team_target = ProviderResourceRef::local_preview(format!("test-team:{team_name}"));
    let channel_target =
        ProviderResourceRef::local_preview(format!("test-channel:{team_name}:{channel_name}"));
    let channel = RemoteChannelDirectory::microsoft365(
        channel_target.clone(),
        channel_name.into(),
        "A session-only channel fixture.".into(),
        "Standard".into(),
        false,
    );
    let team = RemoteTeamDirectory::microsoft365(
        team_target.clone(),
        team_name.into(),
        "A session-only team fixture.".into(),
        vec![channel],
    );
    (
        RemoteTeamsDirectory::microsoft365(vec![team]),
        team_target,
        channel_target,
    )
}

fn remote_two_channel_directory() -> (
    RemoteTeamsDirectory,
    ProviderResourceRef,
    ProviderResourceRef,
    ProviderResourceRef,
) {
    let team = ProviderResourceRef::local_preview("test-team:engineering");
    let planning = ProviderResourceRef::local_preview("test-channel:engineering:planning");
    let delivery = ProviderResourceRef::local_preview("test-channel:engineering:delivery");
    let directory = RemoteTeamsDirectory::microsoft365(vec![RemoteTeamDirectory::microsoft365(
        team.clone(),
        "Engineering".into(),
        "A session-only team fixture.".into(),
        vec![
            RemoteChannelDirectory::microsoft365(
                planning.clone(),
                "Planning".into(),
                String::new(),
                "Standard".into(),
                false,
            ),
            RemoteChannelDirectory::microsoft365(
                delivery.clone(),
                "Delivery".into(),
                String::new(),
                "Standard".into(),
                false,
            ),
        ],
    )]);
    (directory, team, planning, delivery)
}

fn archived_remote_directory() -> (
    RemoteTeamsDirectory,
    ProviderResourceRef,
    ProviderResourceRef,
) {
    let team = ProviderResourceRef::local_preview("test-team:archived");
    let channel = ProviderResourceRef::local_preview("test-channel:archived:history");
    let directory = RemoteTeamsDirectory::microsoft365(vec![RemoteTeamDirectory::microsoft365(
        team.clone(),
        "Archived Engineering".into(),
        String::new(),
        vec![RemoteChannelDirectory::microsoft365(
            channel.clone(),
            "History".into(),
            String::new(),
            "Standard".into(),
            true,
        )],
    )]);
    (directory, team, channel)
}

fn remote_channel_post_action(
    state: &mut TeamsState,
) -> (
    super::RemoteChannelPostSubmissionId,
    ProviderResourceRef,
    ProviderResourceRef,
    String,
) {
    match state
        .remote_channel_post_submission_action()
        .expect("a valid remote channel draft should create one command")
    {
        TeamsAction::SendRemoteChannelPost {
            submission,
            team,
            channel,
            body,
        } => (submission, team, channel, body),
        action => panic!("unexpected action: {action:?}"),
    }
}

fn remote_channel_reply_action(
    state: &mut TeamsState,
) -> (
    super::RemoteChannelReplySubmissionId,
    ProviderResourceRef,
    ProviderResourceRef,
    ProviderResourceRef,
    String,
) {
    match state
        .remote_channel_reply_submission_for_test()
        .expect("a valid remote reply draft should create one command")
    {
        TeamsAction::SendRemoteChannelReply {
            submission,
            team,
            channel,
            post,
            body,
        } => (submission, team, channel, post, body),
        action => panic!("unexpected action: {action:?}"),
    }
}

fn remote_messages(
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
    title: &str,
) -> RemoteChannelMessagesDirectory {
    remote_messages_for_posts(team, channel, &[title])
}

fn remote_messages_for_posts(
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
    titles: &[&str],
) -> RemoteChannelMessagesDirectory {
    RemoteChannelMessagesDirectory::microsoft365(
        team,
        channel,
        titles
            .iter()
            .map(|title| {
                RemoteChannelPost::microsoft365(
                    remote_post_target(title),
                    "Ada Lovelace".into(),
                    "2026-11-06T10:30:00Z".into(),
                    (*title).into(),
                    "A bounded session-only Microsoft 365 root post.".into(),
                    false,
                    false,
                    Vec::new(),
                )
            })
            .collect(),
    )
}

fn remote_messages_with_like(
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
    title: &str,
    like_count: usize,
    like_selected: bool,
) -> RemoteChannelMessagesDirectory {
    remote_messages_with_reaction(
        team,
        channel,
        title,
        ReactionKind::Like,
        like_count,
        like_selected,
    )
}

fn remote_messages_with_reaction(
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
    title: &str,
    reaction: ReactionKind,
    count: usize,
    selected: bool,
) -> RemoteChannelMessagesDirectory {
    remote_messages_with_reactions(
        team,
        channel,
        title,
        remote_reactions(reaction, count, selected),
    )
}

fn remote_messages_with_reactions(
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
    title: &str,
    reactions: Vec<RemoteMessageReaction>,
) -> RemoteChannelMessagesDirectory {
    RemoteChannelMessagesDirectory::microsoft365(
        team,
        channel,
        vec![RemoteChannelPost::microsoft365(
            remote_post_target(title),
            "Ada Lovelace".into(),
            "2026-11-06T10:30:00Z".into(),
            title.into(),
            "A bounded session-only Microsoft 365 root post.".into(),
            false,
            false,
            reactions,
        )],
    )
}

fn remote_reactions(
    reaction: ReactionKind,
    count: usize,
    selected: bool,
) -> Vec<RemoteMessageReaction> {
    (count != 0)
        .then_some(RemoteMessageReaction::new(reaction, count, selected))
        .into_iter()
        .collect()
}

fn remote_post_target(title: &str) -> ProviderResourceRef {
    ProviderResourceRef::local_preview(format!("test-post:{title}"))
}

fn remote_replies(
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
    post: ProviderResourceRef,
    body: &str,
) -> RemoteChannelRepliesDirectory {
    RemoteChannelRepliesDirectory::microsoft365(
        team,
        channel,
        post,
        vec![RemoteChannelReply::microsoft365(
            ProviderResourceRef::local_preview(format!("test-reply:{body}")),
            "Grace Hopper".into(),
            "2026-11-06T10:35:00Z".into(),
            body.into(),
            false,
            false,
            Vec::new(),
        )],
    )
}

fn remote_replies_with_like(
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
    post: ProviderResourceRef,
    body: &str,
    like_count: usize,
    like_selected: bool,
) -> RemoteChannelRepliesDirectory {
    remote_replies_with_reaction(
        team,
        channel,
        post,
        body,
        ReactionKind::Like,
        like_count,
        like_selected,
    )
}

fn remote_replies_with_reaction(
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
    post: ProviderResourceRef,
    body: &str,
    reaction: ReactionKind,
    count: usize,
    selected: bool,
) -> RemoteChannelRepliesDirectory {
    remote_replies_with_reactions(
        team,
        channel,
        post,
        body,
        remote_reactions(reaction, count, selected),
    )
}

fn remote_replies_with_reactions(
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
    post: ProviderResourceRef,
    body: &str,
    reactions: Vec<RemoteMessageReaction>,
) -> RemoteChannelRepliesDirectory {
    RemoteChannelRepliesDirectory::microsoft365(
        team,
        channel,
        post,
        vec![RemoteChannelReply::microsoft365(
            ProviderResourceRef::local_preview(format!("test-reply:{body}")),
            "Grace Hopper".into(),
            "2026-11-06T10:35:00Z".into(),
            body.into(),
            false,
            false,
            reactions,
        )],
    )
}

fn remote_replies_for_reactions(
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
    post: ProviderResourceRef,
    bodies: &[&str],
) -> RemoteChannelRepliesDirectory {
    RemoteChannelRepliesDirectory::microsoft365(
        team,
        channel,
        post,
        bodies
            .iter()
            .map(|body| {
                RemoteChannelReply::microsoft365(
                    ProviderResourceRef::local_preview(format!("test-reply:{body}")),
                    "Grace Hopper".into(),
                    "2026-11-06T10:35:00Z".into(),
                    (*body).into(),
                    false,
                    false,
                    Vec::new(),
                )
            })
            .collect(),
    )
}

fn remote_files(
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
    name: &str,
) -> RemoteChannelFilesDirectory {
    RemoteChannelFilesDirectory::microsoft365(
        team,
        channel,
        vec![RemoteChannelFile::microsoft365(
            ProviderResourceRef::local_preview(format!("test-channel-file:{name}")),
            name.into(),
            "2026-11-06T10:40:00Z".into(),
            Some(42),
            "File".into(),
            None,
        )],
    )
}
