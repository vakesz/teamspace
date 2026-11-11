use super::RemoteMessageInstant;
use super::files::RemoteDriveItem;
#[cfg(doc)]
use super::files::RemoteFilesProjection;
use super::{ProviderResourceRef, TransportError};
use crate::integrations::reaction::RemoteMessageReaction;

/// A provider-normalized remote Teams directory snapshot.
///
/// Like remote chats, this data is session-only. The feature layer may render
/// its teams, channels, and opaque selection targets, but it must not merge
/// them into the local-preview catalog or persistence format. Provider DTOs,
/// continuation links, and Graph route details remain inside the integration
/// adapter that constructed this value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RemoteTeamsProjection {
    teams: Vec<RemoteTeam>,
}

impl RemoteTeamsProjection {
    /// Constructs a normalized directory only after an integration adapter
    /// has validated the provider response and bounded any follow-up reads.
    pub(in crate::integrations) fn microsoft365(teams: Vec<RemoteTeam>) -> Self {
        Self { teams }
    }

    /// Returns presentation-safe teams, their channels, and opaque provider
    /// targets. Resource identifiers stay hidden behind the transport seam.
    pub(crate) fn teams(&self) -> &[RemoteTeam] {
        &self.teams
    }

    #[cfg(test)]
    pub(crate) fn for_test(teams: Vec<RemoteTeam>) -> Self {
        Self::microsoft365(teams)
    }
}

/// A remote Microsoft 365 team and its bounded channel directory.
///
/// The team owns the channel list so a feature cannot accidentally pair a
/// channel from one provider team with a display label from another one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteTeam {
    target: ProviderResourceRef,
    display_name: String,
    description: String,
    channels: Vec<RemoteChannel>,
}

impl RemoteTeam {
    pub(in crate::integrations) fn microsoft365(
        target: ProviderResourceRef,
        display_name: String,
        description: String,
        channels: Vec<RemoteChannel>,
    ) -> Result<Self, TransportError> {
        target.require_microsoft365("team")?;
        target.validate("team id")?;
        Ok(Self {
            target,
            display_name,
            description,
            channels,
        })
    }

    /// An opaque provider-issued reference that can be retained only for a
    /// future provider-aware team command. Features cannot inspect or forge
    /// the underlying Graph identifier.
    pub(crate) fn target(&self) -> ProviderResourceRef {
        self.target.clone()
    }

    pub(crate) fn display_name(&self) -> &str {
        &self.display_name
    }

    pub(crate) fn description(&self) -> &str {
        &self.description
    }

    pub(crate) fn channels(&self) -> &[RemoteChannel] {
        &self.channels
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        provider_id: impl Into<String>,
        display_name: impl Into<String>,
        description: impl Into<String>,
        channels: Vec<RemoteChannel>,
    ) -> Self {
        Self::microsoft365(
            ProviderResourceRef::microsoft365(provider_id),
            display_name.into(),
            description.into(),
            channels,
        )
        .expect("test remote team targets must be valid Microsoft 365 identifiers")
    }
}

/// The provider-normalized membership mode of a Microsoft 365 channel.
///
/// This deliberately remains a small presentation classification. It does
/// not expose provider payloads or imply that the application currently reads
/// the channel's message feed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteChannelMembership {
    Standard,
    Private,
    Shared,
    Unknown,
}

/// A remote Microsoft 365 channel in a bounded team directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteChannel {
    target: ProviderResourceRef,
    display_name: String,
    description: String,
    membership: RemoteChannelMembership,
    archived: bool,
}

impl RemoteChannel {
    pub(in crate::integrations) fn microsoft365(
        target: ProviderResourceRef,
        display_name: String,
        description: String,
        membership: RemoteChannelMembership,
        archived: bool,
    ) -> Result<Self, TransportError> {
        target.require_microsoft365("channel")?;
        target.validate("channel id")?;
        Ok(Self {
            target,
            display_name,
            description,
            membership,
            archived,
        })
    }

    /// An opaque target retained only for a future, provider-aware channel
    /// action. It is not a local catalog key or a Graph ID accessor.
    pub(crate) fn target(&self) -> ProviderResourceRef {
        self.target.clone()
    }

    pub(crate) fn display_name(&self) -> &str {
        &self.display_name
    }

    pub(crate) fn description(&self) -> &str {
        &self.description
    }

    pub(crate) const fn membership(&self) -> RemoteChannelMembership {
        self.membership
    }

    pub(crate) const fn is_archived(&self) -> bool {
        self.archived
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        provider_id: impl Into<String>,
        display_name: impl Into<String>,
        description: impl Into<String>,
        membership: RemoteChannelMembership,
        archived: bool,
    ) -> Self {
        Self::microsoft365(
            ProviderResourceRef::microsoft365(provider_id),
            display_name.into(),
            description.into(),
            membership,
            archived,
        )
        .expect("test remote channel targets must be valid Microsoft 365 identifiers")
    }
}

/// A selection-scoped, provider-normalized root-post feed for one Microsoft
/// 365 channel.
///
/// This projection deliberately carries both opaque team and channel targets.
/// The app can therefore discard a late response when the user has selected a
/// different channel, without comparing display labels or merging Graph posts
/// into the local Teams catalog. Replies, reactions, and mutations have their
/// own contracts and are intentionally absent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteChannelMessagesProjection {
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
    availability: RemoteChannelMessagesAvailability,
    posts: Vec<RemoteChannelPost>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteChannelMessagesAvailability {
    Available,
    AccessDenied,
}

impl RemoteChannelMessagesProjection {
    /// Constructs a bounded root-post feed only inside a provider adapter
    /// after the requested team and channel targets have been validated.
    pub(in crate::integrations) fn microsoft365(
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        posts: Vec<RemoteChannelPost>,
    ) -> Result<Self, TransportError> {
        validate_remote_channel_selection(&team, &channel)?;
        Ok(Self {
            team,
            channel,
            availability: RemoteChannelMessagesAvailability::Available,
            posts,
        })
    }

    /// Replaces a selected channel's old posts when Graph explicitly denies
    /// access. A 403 can reflect delegated consent or channel-level
    /// authorization, so the normalized state does not guess which. The
    /// target binding remains intact for truthful per-channel presentation.
    pub(in crate::integrations) fn access_denied(
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
    ) -> Result<Self, TransportError> {
        validate_remote_channel_selection(&team, &channel)?;
        Ok(Self {
            team,
            channel,
            availability: RemoteChannelMessagesAvailability::AccessDenied,
            posts: Vec::new(),
        })
    }

    pub(crate) fn team(&self) -> ProviderResourceRef {
        self.team.clone()
    }

    pub(crate) fn channel(&self) -> ProviderResourceRef {
        self.channel.clone()
    }

    pub(crate) const fn is_access_denied(&self) -> bool {
        matches!(
            self.availability,
            RemoteChannelMessagesAvailability::AccessDenied
        )
    }

    pub(crate) fn posts(&self) -> &[RemoteChannelPost] {
        &self.posts
    }

    /// Builds a normalized selected-channel feed for integration tests without
    /// widening the provider-owned construction API in production code.
    #[cfg(test)]
    pub(crate) fn for_test(
        team_provider_id: impl Into<String>,
        channel_provider_id: impl Into<String>,
        posts: Vec<RemoteChannelPost>,
    ) -> Self {
        Self::microsoft365(
            ProviderResourceRef::microsoft365(team_provider_id),
            ProviderResourceRef::microsoft365(channel_provider_id),
            posts,
        )
        .expect("test selected-channel targets must be valid Microsoft 365 identifiers")
    }
}

fn validate_remote_channel_selection(
    team: &ProviderResourceRef,
    channel: &ProviderResourceRef,
) -> Result<(), TransportError> {
    team.require_microsoft365("team")?;
    team.validate("team id")?;
    channel.require_microsoft365("channel")?;
    channel.validate("channel id")?;
    Ok(())
}

/// One presentation-safe root post returned for a selected Microsoft 365
/// channel. Its opaque provider target can address separately scoped reply
/// and reaction commands, but never gives reply rows or other remote
/// mutations implicit capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteChannelPost {
    target: ProviderResourceRef,
    author: String,
    created_at: String,
    /// Parsed once at the seam, so a feature can show the time in the user's
    /// zone without parsing display text. `None` when unknown.
    created_instant: Option<RemoteMessageInstant>,
    subject: String,
    plain_text: String,
    deleted: bool,
    edited: bool,
    reactions: Vec<RemoteMessageReaction>,
}

impl RemoteChannelPost {
    #[allow(clippy::too_many_arguments)]
    pub(in crate::integrations) fn microsoft365(
        target: ProviderResourceRef,
        author: String,
        created_at: String,
        subject: String,
        plain_text: String,
        deleted: bool,
        edited: bool,
        reactions: Vec<RemoteMessageReaction>,
    ) -> Result<Self, TransportError> {
        target.require_microsoft365("channel message")?;
        target.validate("channel message id")?;
        Ok(Self {
            target,
            author,
            created_instant: RemoteMessageInstant::parse(&created_at),
            created_at,
            subject,
            plain_text,
            deleted,
            edited,
            reactions,
        })
    }

    pub(crate) fn target(&self) -> ProviderResourceRef {
        self.target.clone()
    }

    pub(crate) fn author(&self) -> &str {
        &self.author
    }

    pub(crate) fn created_at(&self) -> &str {
        &self.created_at
    }

    pub(crate) const fn created_instant(&self) -> Option<RemoteMessageInstant> {
        self.created_instant
    }

    pub(crate) fn subject(&self) -> &str {
        &self.subject
    }

    pub(crate) fn plain_text(&self) -> &str {
        &self.plain_text
    }

    pub(crate) const fn is_deleted(&self) -> bool {
        self.deleted
    }

    pub(crate) const fn is_edited(&self) -> bool {
        self.edited
    }

    /// Bounded application-supported reaction summaries for this post.
    pub(crate) fn reactions(&self) -> &[RemoteMessageReaction] {
        &self.reactions
    }

    /// Builds a presentation-safe root post for integration tests without
    /// allowing production callers to manufacture provider identifiers.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn for_test(
        provider_id: impl Into<String>,
        author: impl Into<String>,
        created_at: impl Into<String>,
        subject: impl Into<String>,
        plain_text: impl Into<String>,
        deleted: bool,
        edited: bool,
    ) -> Self {
        Self::microsoft365(
            ProviderResourceRef::microsoft365(provider_id),
            author.into(),
            created_at.into(),
            subject.into(),
            plain_text.into(),
            deleted,
            edited,
            Vec::new(),
        )
        .expect("test channel message targets must be valid Microsoft 365 identifiers")
    }
}

/// A selection-scoped, provider-normalized reply feed for one root post in a
/// Microsoft 365 channel.
///
/// The three opaque targets bind the feed to both its channel and its root
/// post. This lets presentation code reject a late reply result after the
/// user selects another post without comparing titles or provider IDs. The
/// projection itself remains read-only; separate commands may send text-only
/// content to its selected root or toggle an app-supported reaction on a
/// reply. Attachments, edits, deletion, and reply-to-reply
/// mutations still require their own delivery semantics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteChannelRepliesProjection {
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
    post: ProviderResourceRef,
    availability: RemoteChannelRepliesAvailability,
    replies: Vec<RemoteChannelReply>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteChannelRepliesAvailability {
    Available,
    AccessDenied,
}

impl RemoteChannelRepliesProjection {
    /// Constructs a bounded reply feed only inside a provider adapter after
    /// every selected target and returned reply has been validated.
    pub(in crate::integrations) fn microsoft365(
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        post: ProviderResourceRef,
        replies: Vec<RemoteChannelReply>,
    ) -> Result<Self, TransportError> {
        validate_remote_channel_reply_selection(&team, &channel, &post)?;
        Ok(Self {
            team,
            channel,
            post,
            availability: RemoteChannelRepliesAvailability::Available,
            replies,
        })
    }

    /// Replaces a root post's old replies when Graph explicitly denies the
    /// optional read. A 403 can reflect delegated consent or channel-level
    /// authorization, so the normalized state intentionally does not guess
    /// which remediation applies.
    pub(in crate::integrations) fn access_denied(
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        post: ProviderResourceRef,
    ) -> Result<Self, TransportError> {
        validate_remote_channel_reply_selection(&team, &channel, &post)?;
        Ok(Self {
            team,
            channel,
            post,
            availability: RemoteChannelRepliesAvailability::AccessDenied,
            replies: Vec::new(),
        })
    }

    pub(crate) fn team(&self) -> ProviderResourceRef {
        self.team.clone()
    }

    pub(crate) fn channel(&self) -> ProviderResourceRef {
        self.channel.clone()
    }

    pub(crate) fn post(&self) -> ProviderResourceRef {
        self.post.clone()
    }

    pub(crate) const fn is_access_denied(&self) -> bool {
        matches!(
            self.availability,
            RemoteChannelRepliesAvailability::AccessDenied
        )
    }

    pub(crate) fn replies(&self) -> &[RemoteChannelReply] {
        &self.replies
    }

    /// Builds a reply projection for app lifecycle tests without exposing
    /// provider-owned identifier construction in a production build.
    #[cfg(test)]
    pub(crate) fn for_test(
        team_provider_id: impl Into<String>,
        channel_provider_id: impl Into<String>,
        post_provider_id: impl Into<String>,
        replies: Vec<RemoteChannelReply>,
    ) -> Self {
        Self::microsoft365(
            ProviderResourceRef::microsoft365(team_provider_id),
            ProviderResourceRef::microsoft365(channel_provider_id),
            ProviderResourceRef::microsoft365(post_provider_id),
            replies,
        )
        .expect("test selected reply targets must be valid Microsoft 365 identifiers")
    }
}

/// A selection-scoped, provider-normalized metadata view of one Microsoft 365
/// channel's backing files folder.
///
/// This is intentionally distinct from [`RemoteFilesProjection`]: the latter
/// represents the signed-in user's OneDrive root, while this view is bound to
/// a selected team and channel. Provider-only drive and folder routing IDs are
/// resolved and discarded inside the adapter; callers retain only the opaque
/// channel selection and presentation-safe direct-child metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteChannelFilesProjection {
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
    availability: RemoteChannelFilesAvailability,
    items: Vec<RemoteDriveItem>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteChannelFilesAvailability {
    Available,
    AccessDenied,
}

impl RemoteChannelFilesProjection {
    /// Constructs a bounded direct-child view only inside a provider adapter
    /// after it has validated the selected channel and normalized each item.
    pub(in crate::integrations) fn microsoft365(
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        items: Vec<RemoteDriveItem>,
    ) -> Result<Self, TransportError> {
        validate_remote_channel_selection(&team, &channel)?;
        Ok(Self {
            team,
            channel,
            availability: RemoteChannelFilesAvailability::Available,
            items,
        })
    }

    /// Replaces a selected channel's old metadata when Graph explicitly
    /// denies the optional read. A 403 can be delegated consent or channel
    /// authorization, so the normalized state deliberately does not guess.
    pub(in crate::integrations) fn access_denied(
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
    ) -> Result<Self, TransportError> {
        validate_remote_channel_selection(&team, &channel)?;
        Ok(Self {
            team,
            channel,
            availability: RemoteChannelFilesAvailability::AccessDenied,
            items: Vec::new(),
        })
    }

    pub(crate) fn team(&self) -> ProviderResourceRef {
        self.team.clone()
    }

    pub(crate) fn channel(&self) -> ProviderResourceRef {
        self.channel.clone()
    }

    pub(crate) const fn is_access_denied(&self) -> bool {
        matches!(
            self.availability,
            RemoteChannelFilesAvailability::AccessDenied
        )
    }

    /// Returns only direct-child metadata. It never exposes the backing
    /// SharePoint drive ID, files-folder ID, content URLs, or local paths.
    pub(crate) fn items(&self) -> &[RemoteDriveItem] {
        &self.items
    }

    /// Builds a normalized selected-channel file view for lifecycle tests
    /// without widening provider-owned construction in production builds.
    #[cfg(test)]
    pub(crate) fn for_test(
        team_provider_id: impl Into<String>,
        channel_provider_id: impl Into<String>,
        items: Vec<RemoteDriveItem>,
    ) -> Self {
        Self::microsoft365(
            ProviderResourceRef::microsoft365(team_provider_id),
            ProviderResourceRef::microsoft365(channel_provider_id),
            items,
        )
        .expect("test selected channel targets must be valid Microsoft 365 identifiers")
    }
}

fn validate_remote_channel_reply_selection(
    team: &ProviderResourceRef,
    channel: &ProviderResourceRef,
    post: &ProviderResourceRef,
) -> Result<(), TransportError> {
    validate_remote_channel_selection(team, channel)?;
    post.require_microsoft365("channel root post")?;
    post.validate("channel root post id")?;
    Ok(())
}

/// One presentation-safe reply returned for a selected Microsoft 365 channel
/// root post. Its opaque target can address only the separately scoped reply
/// reaction command; it does not imply capability for other remote mutations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteChannelReply {
    target: ProviderResourceRef,
    author: String,
    created_at: String,
    /// See [`RemoteChannelPost`]: parsed once at the seam.
    created_instant: Option<RemoteMessageInstant>,
    plain_text: String,
    deleted: bool,
    edited: bool,
    reactions: Vec<RemoteMessageReaction>,
}

impl RemoteChannelReply {
    #[allow(clippy::too_many_arguments)]
    pub(in crate::integrations) fn microsoft365(
        target: ProviderResourceRef,
        author: String,
        created_at: String,
        plain_text: String,
        deleted: bool,
        edited: bool,
        reactions: Vec<RemoteMessageReaction>,
    ) -> Result<Self, TransportError> {
        target.require_microsoft365("channel reply")?;
        target.validate("channel reply id")?;
        Ok(Self {
            target,
            author,
            created_instant: RemoteMessageInstant::parse(&created_at),
            created_at,
            plain_text,
            deleted,
            edited,
            reactions,
        })
    }

    pub(crate) fn target(&self) -> ProviderResourceRef {
        self.target.clone()
    }

    pub(crate) fn author(&self) -> &str {
        &self.author
    }

    pub(crate) fn created_at(&self) -> &str {
        &self.created_at
    }

    pub(crate) const fn created_instant(&self) -> Option<RemoteMessageInstant> {
        self.created_instant
    }

    pub(crate) fn plain_text(&self) -> &str {
        &self.plain_text
    }

    pub(crate) const fn is_deleted(&self) -> bool {
        self.deleted
    }

    pub(crate) const fn is_edited(&self) -> bool {
        self.edited
    }

    /// Bounded application-supported reaction summaries for this reply.
    pub(crate) fn reactions(&self) -> &[RemoteMessageReaction] {
        &self.reactions
    }

    /// Builds a presentation-safe reply for integration tests without
    /// allowing production callers to manufacture provider identifiers.
    #[cfg(test)]
    pub(crate) fn for_test(
        provider_id: impl Into<String>,
        author: impl Into<String>,
        created_at: impl Into<String>,
        plain_text: impl Into<String>,
        deleted: bool,
        edited: bool,
    ) -> Self {
        Self::microsoft365(
            ProviderResourceRef::microsoft365(provider_id),
            author.into(),
            created_at.into(),
            plain_text.into(),
            deleted,
            edited,
            Vec::new(),
        )
        .expect("test channel reply targets must be valid Microsoft 365 identifiers")
    }
}
