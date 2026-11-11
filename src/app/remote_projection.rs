//! Pure adapters from integration-normalized remote projections into
//! feature-owned session state, plus their display-label helpers.

use crate::features::calendar::{
    RemoteCalendarDirectory, RemoteCalendarEvent as RemoteAgendaEvent,
};
use crate::features::files::{
    RemoteFileItem, RemoteFileKind, RemoteFilePreview, RemoteFilePreviewContent,
    RemoteFilesDirectory,
};
use crate::features::teams::{
    RemoteChannelDirectory, RemoteChannelFile as RemoteChannelFileItem,
    RemoteChannelFilesDirectory, RemoteChannelMessagesDirectory,
    RemoteChannelPost as RemoteChannelFeedPost, RemoteChannelRepliesDirectory,
    RemoteChannelReply as RemoteChannelFeedReply, RemoteTeamDirectory, RemoteTeamsDirectory,
};
use crate::integrations::{
    RemoteCalendarProjection, RemoteChannelFilesProjection, RemoteChannelMembership,
    RemoteChannelMessagesProjection, RemoteChannelRepliesProjection, RemoteDriveItemKind,
    RemoteFilePreviewPayload, RemoteFilePreviewProjection, RemoteFilesProjection,
    RemoteTeamsProjection,
};
use crate::shared::local_time;

/// Adapts an integration-normalized Teams directory into feature-owned
/// session state. Both sides retain opaque provider targets, while this
/// narrow conversion keeps the local catalog and its persistence format out
/// of the remote transport path.
pub(super) fn remote_teams_directory_from_projection(
    projection: RemoteTeamsProjection,
) -> RemoteTeamsDirectory {
    RemoteTeamsDirectory::microsoft365(
        projection
            .teams()
            .iter()
            .map(|team| {
                RemoteTeamDirectory::microsoft365(
                    team.target(),
                    team.display_name().into(),
                    team.description().into(),
                    team.channels()
                        .iter()
                        .map(|channel| {
                            RemoteChannelDirectory::microsoft365(
                                channel.target(),
                                channel.display_name().into(),
                                channel.description().into(),
                                remote_channel_membership_label(channel.membership()).into(),
                                channel.is_archived(),
                            )
                        })
                        .collect(),
                )
            })
            .collect(),
    )
}

pub(super) fn remote_channel_membership_label(membership: RemoteChannelMembership) -> &'static str {
    match membership {
        RemoteChannelMembership::Standard => "Standard",
        RemoteChannelMembership::Private => "Private",
        RemoteChannelMembership::Shared => "Shared",
        RemoteChannelMembership::Unknown => "Unspecified",
    }
}

/// Adapts normalized OneDrive-root metadata into the Files-owned, transient
/// view. The integration target remains opaque and the feature deliberately
/// receives no route, path, content, or mutable provider field.
pub(super) fn remote_files_directory_from_projection(
    projection: RemoteFilesProjection,
) -> RemoteFilesDirectory {
    if projection.is_access_denied() {
        return RemoteFilesDirectory::access_denied();
    }
    let clock = local_time::clock();
    RemoteFilesDirectory::microsoft365(
        projection
            .items()
            .iter()
            .map(|item| {
                RemoteFileItem::microsoft365(
                    item.target(),
                    item.name().into(),
                    local_time::label_or(item.modified_instant(), item.modified_at(), &clock),
                    item.size_bytes(),
                    remote_file_kind(item.kind()),
                    remote_drive_item_kind_label(item.kind()).into(),
                    item.web_url().cloned(),
                )
            })
            .collect(),
    )
}

/// Adapts one normalized, target-bound OneDrive content result into Files'
/// remote-only preview state. The integration projection deliberately omits
/// every Graph URL, redirect, raw response header, and local path; this bridge
/// retains that boundary while collapsing provider-specific failure categories
/// into the feature's single safe unavailable presentation.
pub(super) fn remote_file_preview_from_projection(
    projection: RemoteFilePreviewProjection,
) -> RemoteFilePreview {
    let content = match projection.payload() {
        RemoteFilePreviewPayload::Text(value) => RemoteFilePreviewContent::Text(value.clone()),
        RemoteFilePreviewPayload::Image { bytes, cache_key } => RemoteFilePreviewContent::Image {
            bytes: bytes.clone(),
            cache_key: *cache_key,
        },
        RemoteFilePreviewPayload::Unavailable(_) => RemoteFilePreviewContent::Unavailable,
    };
    RemoteFilePreview::microsoft365(projection.target(), content)
}

fn remote_file_kind(kind: RemoteDriveItemKind) -> RemoteFileKind {
    match kind {
        RemoteDriveItemKind::File => RemoteFileKind::File,
        RemoteDriveItemKind::Folder => RemoteFileKind::Folder,
        RemoteDriveItemKind::Unknown => RemoteFileKind::Unknown,
    }
}

pub(super) fn remote_drive_item_kind_label(kind: RemoteDriveItemKind) -> &'static str {
    match kind {
        RemoteDriveItemKind::File => "File",
        RemoteDriveItemKind::Folder => "Folder",
        RemoteDriveItemKind::Unknown => "Item",
    }
}

/// Converts an integration-normalized selected-channel feed into the
/// Teams-owned session state. Both layers retain opaque provider targets, so
/// the feature can reject a late feed for a channel the user no longer has
/// selected without leaking Graph paths or identifiers into UI code.
pub(super) fn remote_channel_messages_directory_from_projection(
    projection: RemoteChannelMessagesProjection,
) -> RemoteChannelMessagesDirectory {
    let team = projection.team();
    let channel = projection.channel();
    if projection.is_access_denied() {
        return RemoteChannelMessagesDirectory::access_denied(team, channel);
    }
    let clock = local_time::clock();
    RemoteChannelMessagesDirectory::microsoft365(
        team,
        channel,
        projection
            .posts()
            .iter()
            .map(|post| {
                RemoteChannelFeedPost::microsoft365(
                    post.target(),
                    post.author().into(),
                    local_time::label_or(post.created_instant(), post.created_at(), &clock),
                    post.subject().into(),
                    post.plain_text().into(),
                    post.is_deleted(),
                    post.is_edited(),
                    post.reactions().to_vec(),
                )
            })
            .collect(),
    )
}

/// Converts an integration-normalized selected-channel backing-folder view
/// into Teams-owned session state. The provider adapter owns the resolved
/// drive/folder route; this bridge retains only the opaque selected channel
/// targets and presentation-safe direct-child metadata.
pub(super) fn remote_channel_files_directory_from_projection(
    projection: RemoteChannelFilesProjection,
) -> RemoteChannelFilesDirectory {
    let team = projection.team();
    let channel = projection.channel();
    if projection.is_access_denied() {
        return RemoteChannelFilesDirectory::access_denied(team, channel);
    }
    let clock = local_time::clock();
    RemoteChannelFilesDirectory::microsoft365(
        team,
        channel,
        projection
            .items()
            .iter()
            .map(|item| {
                RemoteChannelFileItem::microsoft365(
                    item.target(),
                    item.name().into(),
                    local_time::label_or(item.modified_instant(), item.modified_at(), &clock),
                    item.size_bytes(),
                    remote_drive_item_kind_label(item.kind()).into(),
                    item.web_url().cloned(),
                )
            })
            .collect(),
    )
}

/// Converts an integration-normalized selected-root-post reply feed into the
/// Teams-owned session state. The opaque team, channel, and post targets stay
/// attached so the feature can discard a late response after any part of the
/// remote selection changes.
pub(super) fn remote_channel_replies_directory_from_projection(
    projection: RemoteChannelRepliesProjection,
) -> RemoteChannelRepliesDirectory {
    let team = projection.team();
    let channel = projection.channel();
    let post = projection.post();
    if projection.is_access_denied() {
        return RemoteChannelRepliesDirectory::access_denied(team, channel, post);
    }
    let clock = local_time::clock();
    RemoteChannelRepliesDirectory::microsoft365(
        team,
        channel,
        post,
        projection
            .replies()
            .iter()
            .map(|reply| {
                RemoteChannelFeedReply::microsoft365(
                    reply.target(),
                    reply.author().into(),
                    local_time::label_or(reply.created_instant(), reply.created_at(), &clock),
                    reply.plain_text().into(),
                    reply.is_deleted(),
                    reply.is_edited(),
                    reply.reactions().to_vec(),
                )
            })
            .collect(),
    )
}

/// Adapts an integration-normalized default calendar into the separate,
/// session-only agenda owned by the Calendar feature. Local preview events
/// intentionally do not appear here, and a missing projection replaces any
/// stale remote rows without fabricating a writable calendar.
pub(super) fn remote_calendar_directory_from_projection(
    projection: RemoteCalendarProjection,
) -> RemoteCalendarDirectory {
    let Some(calendar) = projection.calendar() else {
        return RemoteCalendarDirectory::microsoft365(
            "Microsoft 365 calendar".into(),
            "UTC".into(),
            false,
            Vec::new(),
        );
    };

    let clock = local_time::clock();
    let ranges: Vec<Option<(String, String)>> = calendar
        .events()
        .iter()
        .map(|event| local_time::range(event.starts_instant(), event.ends_instant(), &clock))
        .collect();
    // Every time below is shown in the user's own zone when it could be placed.
    let all_local = !ranges.is_empty() && ranges.iter().all(Option::is_some);
    let first_time_zone = calendar
        .events()
        .first()
        .map(|event| event.time_zone())
        .filter(|time_zone| !time_zone.is_empty())
        .unwrap_or("UTC");
    let time_zone = if all_local {
        "Local time".into()
    } else if calendar
        .events()
        .iter()
        .all(|event| event.time_zone() == first_time_zone)
    {
        first_time_zone.into()
    } else {
        "Event time zones".into()
    };

    RemoteCalendarDirectory::microsoft365(
        calendar.display_name().into(),
        time_zone,
        calendar.can_edit(),
        calendar
            .events()
            .iter()
            .zip(ranges)
            .map(|(event, range)| {
                let (starts_at, ends_at) =
                    range.unwrap_or_else(|| (event.starts_at().into(), event.ends_at().into()));
                RemoteAgendaEvent::microsoft365(
                    event.target(),
                    event.title().into(),
                    starts_at,
                    ends_at,
                    event.location().into(),
                    event.organizer().into(),
                    event.has_online_meeting() || event.join_url().is_some(),
                    event.join_url().map(str::to_owned),
                    event.is_cancelled(),
                )
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        remote_channel_files_directory_from_projection, remote_file_preview_from_projection,
        remote_files_directory_from_projection, remote_teams_directory_from_projection,
    };
    use crate::features::files::FilesState;
    use crate::features::teams::TeamsState;
    use crate::integrations::{
        ProviderBrowserUrl, RemoteChannel, RemoteChannelFilesProjection, RemoteChannelMembership,
        RemoteDriveItem, RemoteDriveItemKind, RemoteFilePreviewPayload,
        RemoteFilePreviewProjection, RemoteFilesProjection, RemoteTeam, RemoteTeamsProjection,
    };

    #[test]
    fn onedrive_browser_destination_crosses_the_bridge_as_a_redacted_opaque_value() {
        let destination = "https://contoso.sharepoint.com/:x:/r/opaque-bridge-sentinel";
        let item = RemoteDriveItem::for_test_with_web_url(
            "remote-onedrive-bridge-item",
            "Roadmap.xlsx",
            "2026-11-06T10:30:00Z",
            Some(1_536),
            RemoteDriveItemKind::File,
            destination,
        );
        let target = item.target();
        let directory =
            remote_files_directory_from_projection(RemoteFilesProjection::for_test(vec![item]));

        // The bridge preserves the typed handoff capability without making a
        // provider URL part of a diagnostic representation or local state.
        assert!(!format!("{directory:?}").contains(destination));
        let mut files = FilesState::default();
        files.replace_remote_files_directory(directory);
        assert!(files.request_remote_file_open_for_test(target));
        let handoffs = files.take_remote_file_open_links();
        assert_eq!(handoffs, vec![ProviderBrowserUrl::for_test(destination)]);
        assert!(!format!("{handoffs:?}").contains(destination));
    }

    #[test]
    fn onedrive_preview_crosses_the_bridge_only_for_the_current_file_target() {
        let target_id = "remote-onedrive-preview-item";
        let source_text = "remote preview body must not reach debug output";
        let item = RemoteDriveItem::for_test(
            target_id,
            "Notes.txt",
            "2026-11-06T10:30:00Z",
            Some(128),
            RemoteDriveItemKind::File,
        );
        let target = item.target();
        let directory =
            remote_files_directory_from_projection(RemoteFilesProjection::for_test(vec![item]));
        let projection = RemoteFilePreviewProjection::for_test(
            target_id,
            RemoteFilePreviewPayload::Text(source_text.into()),
        );

        assert!(!format!("{projection:?}").contains(source_text));
        let preview = remote_file_preview_from_projection(projection);
        assert!(!format!("{preview:?}").contains(source_text));

        let mut files = FilesState::default();
        files.replace_remote_files_directory(directory);
        assert!(files.request_remote_file_preview_for_test(target));
        assert!(files.take_remote_file_preview_request().is_some());
        assert!(files.apply_remote_file_preview(preview));
        assert!(files.remote_file_preview_is_ready_for_test());
        assert!(!files.is_persistence_dirty());
    }

    #[test]
    fn channel_file_browser_destination_crosses_its_target_bound_bridge() {
        let destination = "https://contoso.sharepoint.com/:x:/r/opaque-channel-file-sentinel";
        let item = RemoteDriveItem::for_test_with_web_url(
            "remote-channel-drive-item",
            "Channel delivery plan.docx",
            "2026-11-06T10:40:00Z",
            Some(8_192),
            RemoteDriveItemKind::File,
            destination,
        );
        let target = item.target();
        let projection = RemoteChannelFilesProjection::for_test(
            "remote-channel-file-team",
            "remote-channel-file-channel",
            vec![item],
        );
        let team = projection.team();
        let channel = projection.channel();
        let directory = remote_channel_files_directory_from_projection(projection);

        let remote_directory =
            remote_teams_directory_from_projection(RemoteTeamsProjection::for_test(vec![
                RemoteTeam::for_test(
                    "remote-channel-file-team",
                    "Engineering",
                    "Session-only remote directory fixture.",
                    vec![RemoteChannel::for_test(
                        "remote-channel-file-channel",
                        "Delivery",
                        "Session-only remote channel fixture.",
                        RemoteChannelMembership::Standard,
                        false,
                    )],
                ),
            ]));
        let mut teams = TeamsState::default();
        teams.replace_remote_directory(remote_directory);
        assert!(teams.select_remote_channel_for_test(team, channel));
        assert!(teams.replace_remote_channel_files(directory));
        assert!(teams.request_remote_channel_file_open_for_test(target));

        let handoffs = teams.take_remote_channel_file_open_links();
        assert_eq!(handoffs, vec![ProviderBrowserUrl::for_test(destination)]);
        assert!(!format!("{handoffs:?}").contains(destination));
    }

    #[test]
    fn remote_times_cross_the_bridge_as_local_time_labels() {
        use super::{
            remote_calendar_directory_from_projection,
            remote_channel_messages_directory_from_projection,
        };
        use crate::integrations::{
            RemoteCalendar, RemoteCalendarEvent, RemoteCalendarProjection,
            RemoteChannelMessagesProjection, RemoteChannelPost,
        };
        // The test clock is Friday 2026-11-06 12:00 UTC, shown in UTC.
        let post = RemoteChannelPost::for_test(
            "post-1",
            "Ada",
            "2026-11-05T09:05:00.1234567Z",
            "",
            "Hello",
            false,
            false,
        );
        let feed = remote_channel_messages_directory_from_projection(
            RemoteChannelMessagesProjection::for_test("team-1", "channel-1", vec![post]),
        );
        let shown = format!("{feed:?}");
        assert!(shown.contains("Yesterday 09:05"), "{shown}");
        assert!(!shown.contains("2026-11-05T09:05"), "{shown}");

        let utc = RemoteCalendarEvent::for_test(
            "event-1",
            "Sync",
            "2026-11-06T09:00:00.0000000",
            "2026-11-06T10:30:00.0000000",
            "UTC",
            "",
            "",
            false,
            None,
            false,
        );
        let calendar = |event| {
            remote_calendar_directory_from_projection(RemoteCalendarProjection::for_test(Some(
                RemoteCalendar::for_test("cal-1", "Calendar", false, vec![event]),
            )))
        };
        let shown = format!("{:?}", calendar(utc));
        assert!(shown.contains("Today 09:00"), "{shown}");
        assert!(shown.contains("10:30"), "{shown}");
        assert!(shown.contains("Local time"), "{shown}");

        // A named zone Graph did not convert cannot be placed without its
        // rules, so its own text and zone label are kept.
        let named = RemoteCalendarEvent::for_test(
            "event-2",
            "Sync",
            "2026-11-06T09:00:00.0000000",
            "2026-11-06T10:30:00.0000000",
            "Pacific Standard Time",
            "",
            "",
            false,
            None,
            false,
        );
        let shown = format!("{:?}", calendar(named));
        assert!(shown.contains("2026-11-06T09:00:00.0000000"), "{shown}");
        assert!(shown.contains("Pacific Standard Time"), "{shown}");
    }

    #[test]
    fn drive_item_modified_times_cross_the_bridge_as_local_time_labels() {
        let item = |id: &str, modified: &str| {
            RemoteDriveItem::for_test(
                id,
                "Roadmap.xlsx",
                modified,
                Some(1_536),
                RemoteDriveItemKind::File,
            )
        };
        // The test clock is Friday 2026-11-06 12:00 UTC, shown in UTC.
        let directory =
            remote_files_directory_from_projection(RemoteFilesProjection::for_test(vec![
                item("item-1", "2026-11-06T10:30:00Z"),
                item("item-2", "2026-10-05T09:08:00Z"),
                item("item-3", "not a time"),
            ]));
        let shown = format!("{directory:?}");
        assert!(shown.contains("Today 10:30"), "{shown}");
        assert!(shown.contains("Oct 05, 09:08"), "{shown}");
        assert!(shown.contains("not a time"), "{shown}");
        assert!(!shown.contains("2026-11-06T10:30"), "{shown}");

        let channel =
            remote_channel_files_directory_from_projection(RemoteChannelFilesProjection::for_test(
                "team-1",
                "channel-1",
                vec![item("item-4", "2026-11-05T17:45:00Z")],
            ));
        assert!(format!("{channel:?}").contains("Yesterday 17:45"));
    }
}
