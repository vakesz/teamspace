//! Microsoft Graph Teams and channels directory normalization.
//!
//! This module deliberately implements only a bounded, read-only directory.
//! It owns Graph routes, pagination validation, response limits, and DTO
//! normalization; feature code sees only opaque provider references and safe
//! display text. Channel posts, replies, reactions, and writes need a
//! selection-driven message projection and a separate mutation lifecycle.

use super::GraphSession;
use super::text::{normalized_identifier, safe_display};
use super::{GraphHttpError, GraphHttpResponse, Microsoft365Transport};
use crate::integrations::cloud::GraphCloud;
use crate::integrations::transport::{
    ProviderResourceRef, RemoteChannel, RemoteChannelMembership, RemoteTeam, RemoteTeamsProjection,
};
use crate::integrations::{SyncInterest, TransportCancellation, TransportError, WorkspaceDelta};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::collections::BTreeSet;
use url::Url;

// `associatedTeams` deliberately has no OData query options. Keep the route
// exact: adding `$select` or `$top` makes Graph reject an otherwise valid
// delegated directory read.
const MAX_TEAMS_RESPONSE_BYTES: u64 = 512 * 1024;
const MAX_TEAM_PAGES: usize = 4;
const MAX_REMOTE_TEAMS: usize = 25;
const MAX_CHANNEL_PAGES_PER_TEAM: usize = 2;
const MAX_CHANNELS_PER_TEAM: usize = 100;
const MAX_TEAM_NAME_BYTES: usize = 256;
const MAX_TEAM_DESCRIPTION_BYTES: usize = 1_024;
const MAX_CHANNEL_NAME_BYTES: usize = 256;
const MAX_CHANNEL_DESCRIPTION_BYTES: usize = 1_024;

/// A normalized directory plus safe notices about an intentionally incomplete
/// provider view. Provider error bodies, URLs, identifiers, and raw DTOs never
/// leave this module.
struct TeamsReadResult {
    projection: RemoteTeamsProjection,
    notices: Vec<String>,
}

/// Failure categories are intentionally narrow. They make controller retry
/// policy possible without accidentally displaying provider response bodies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TeamsReadFailure {
    Cancelled,
    ReauthenticationRequired,
    AccessDenied,
    Throttled(Option<u64>),
    Unavailable,
}

impl TeamsReadFailure {
    fn transport_error(self) -> TransportError {
        match self {
            Self::Cancelled => TransportError::Cancelled,
            Self::ReauthenticationRequired => TransportError::ReauthenticationRequired,
            Self::AccessDenied => TransportError::Unauthorized,
            Self::Throttled(retry_after_seconds) => TransportError::Throttled {
                retry_after_seconds,
            },
            Self::Unavailable => TransportError::Unavailable(
                "Microsoft Graph Teams synchronization is temporarily unavailable".into(),
            ),
        }
    }
}

impl Microsoft365Transport {
    /// Reads a bounded hierarchy of Microsoft 365 teams and their channels.
    ///
    /// A missing directory scope replaces the old remote directory with an
    /// empty snapshot, but deliberately retains a verified `/me` identity.
    /// The account can remain connected while an optional surface lacks
    /// consent.
    pub(super) fn poll_teams(&mut self) -> Result<WorkspaceDelta, TransportError> {
        self.poll_teams_with_cancellation(None)
    }

    /// The remote worker passes its generation-owned cancellation signal so an
    /// account/profile transition does not start further fan-out after an
    /// adapter has become stale.
    pub(super) fn poll_teams_cancellable(
        &mut self,
        cancellation: &TransportCancellation,
    ) -> Result<WorkspaceDelta, TransportError> {
        self.poll_teams_with_cancellation(Some(cancellation))
    }

    fn poll_teams_with_cancellation(
        &mut self,
        cancellation: Option<&TransportCancellation>,
    ) -> Result<WorkspaceDelta, TransportError> {
        if let Err(failure) = check_cancelled(cancellation) {
            return Err(failure.transport_error());
        }
        match fetch_teams_projection(&self.session, cancellation)
        {
            Ok(result) => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::Teams],
                identity: None,
                remote_chats: None,
                remote_chat_messages: None,
                remote_pinned_chat_messages: None,
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: Some(result.projection),
                remote_channel_messages: None,
                remote_channel_files: None,
                remote_channel_replies: None,
                remote_files: None,
                remote_file_preview: None,
                remote_calendar: None,
                remote_capabilities: None,
                notices: result.notices,
            }),
            Err(TeamsReadFailure::AccessDenied) => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::Teams],
                identity: None,
                remote_chats: None,
                remote_chat_messages: None,
                remote_pinned_chat_messages: None,
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: Some(RemoteTeamsProjection::microsoft365(Vec::new())),
                remote_channel_messages: None,
                remote_channel_files: None,
                remote_channel_replies: None,
                remote_files: None,
                remote_file_preview: None,
                remote_calendar: None,
                remote_capabilities: None,
                notices: vec![
                    "Microsoft 365 Teams directory access was not consented. Add delegated Team.ReadBasic.All and Channel.ReadBasic.All, then reconnect to load remote teams."
                        .into(),
                ],
            }),
            Err(TeamsReadFailure::ReauthenticationRequired) => {
                let error = TransportError::ReauthenticationRequired;
                self.identity = None;
                self.record_error(&error);
                Err(error)
            }
            Err(TeamsReadFailure::Cancelled) => Err(TransportError::Cancelled),
            Err(failure) => Err(failure.transport_error()),
        }
    }
}

fn fetch_teams_projection(
    session: &GraphSession<'_>,
    cancellation: Option<&TransportCancellation>,
) -> Result<TeamsReadResult, TeamsReadFailure> {
    check_cancelled(cancellation)?;
    let (graph_teams, teams_truncated) = fetch_associated_team_pages(session, cancellation)?;
    let mut teams = Vec::with_capacity(graph_teams.len());
    let mut notices = Vec::new();
    let mut channel_pages_truncated = 0_usize;
    let mut channel_access_denied = 0_usize;
    let mut channel_unavailable = 0_usize;
    let mut channel_throttle_retry_after: Option<Option<u64>> = None;

    for graph_team in graph_teams {
        check_cancelled(cancellation)?;
        let Some(team_id) = graph_team.id.as_deref().and_then(normalized_identifier) else {
            notices.push(
                "Microsoft Graph returned a team without a usable identifier; it was skipped."
                    .into(),
            );
            continue;
        };

        let mut channels = Vec::new();
        if channel_throttle_retry_after.is_none() {
            match fetch_team_channels(session, &team_id, cancellation) {
                Ok(result) => {
                    if result.truncated {
                        channel_pages_truncated = channel_pages_truncated.saturating_add(1);
                    }
                    channels = result
                        .channels
                        .into_iter()
                        .filter_map(normalize_channel)
                        .collect();
                }
                Err(TeamsReadFailure::ReauthenticationRequired) => {
                    return Err(TeamsReadFailure::ReauthenticationRequired);
                }
                Err(TeamsReadFailure::Cancelled) => return Err(TeamsReadFailure::Cancelled),
                Err(TeamsReadFailure::AccessDenied) => {
                    // A team can be visible while the channel listing is not
                    // allowed. Preserve its safe directory row and aggregate
                    // one actionable notice rather than failing sign-in.
                    channel_access_denied = channel_access_denied.saturating_add(1);
                }
                Err(TeamsReadFailure::Throttled(retry_after_seconds)) => {
                    // Do not multiply a provider backoff with the remaining
                    // per-team requests. Existing team rows stay useful.
                    channel_throttle_retry_after = Some(retry_after_seconds);
                }
                Err(TeamsReadFailure::Unavailable) => {
                    channel_unavailable = channel_unavailable.saturating_add(1);
                }
            }
        }

        let display_name = graph_team
            .display_name
            .as_deref()
            .map(|value| safe_display(value, MAX_TEAM_NAME_BYTES))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "Microsoft 365 team".into());
        let description = graph_team
            .description
            .as_deref()
            .map(|value| safe_display(value, MAX_TEAM_DESCRIPTION_BYTES))
            .unwrap_or_default();
        let team = RemoteTeam::microsoft365(
            ProviderResourceRef::microsoft365(team_id),
            display_name,
            description,
            channels,
        )
        .map_err(|_| TeamsReadFailure::Unavailable)?;
        teams.push(team);
    }

    if teams_truncated {
        notices.push(format!(
            "Microsoft 365 Teams sync is showing the first {MAX_REMOTE_TEAMS} teams."
        ));
    }
    if channel_pages_truncated > 0 {
        notices.push(format!(
            "Microsoft 365 Teams sync is showing at most {MAX_CHANNELS_PER_TEAM} channels for some teams."
        ));
    }
    if channel_access_denied > 0 {
        notices.push(
            "Channels could not be read for some Microsoft 365 teams; add delegated Channel.ReadBasic.All and reconnect to load them."
                .into(),
        );
    }
    if let Some(retry_after_seconds) = channel_throttle_retry_after {
        notices.push(match retry_after_seconds {
            Some(seconds) => {
                format!("Microsoft 365 channel sync was throttled; retry after {seconds} seconds.")
            }
            None => "Microsoft 365 channel sync was throttled; retry later.".into(),
        });
    }
    if channel_unavailable > 0 {
        notices
            .push("Channels could not be read for some Microsoft 365 teams; retry later.".into());
    }

    check_cancelled(cancellation)?;
    Ok(TeamsReadResult {
        projection: RemoteTeamsProjection::microsoft365(teams),
        notices,
    })
}

fn fetch_associated_team_pages(
    session: &GraphSession<'_>,
    cancellation: Option<&TransportCancellation>,
) -> Result<(Vec<GraphTeam>, bool), TeamsReadFailure> {
    let mut next = session
        .cloud()
        .graph_route(&["me", "teamwork", "associatedTeams"], None)
        .map_err(|_| TeamsReadFailure::Unavailable)?;
    let mut teams = Vec::new();
    let mut seen = BTreeSet::new();
    let mut truncated = false;

    for page_index in 0..MAX_TEAM_PAGES {
        let page: GraphCollection<GraphTeam> = get_collection(session, &next, cancellation)?;
        for team in page.value {
            let Some(id) = team.id.as_deref().and_then(normalized_identifier) else {
                continue;
            };
            if !seen.insert(id) {
                continue;
            }
            teams.push(team);
            if teams.len() == MAX_REMOTE_TEAMS {
                truncated = true;
                return Ok((teams, truncated));
            }
        }
        let Some(next_link) = page.next_link else {
            return Ok((teams, truncated));
        };
        next = validated_graph_next_link(session.cloud(), &next_link, next.path())?;
        if page_index + 1 == MAX_TEAM_PAGES {
            truncated = true;
        }
    }

    Ok((teams, truncated))
}

struct TeamChannels {
    channels: Vec<GraphChannel>,
    truncated: bool,
}

fn fetch_team_channels(
    session: &GraphSession<'_>,
    team_id: &str,
    cancellation: Option<&TransportCancellation>,
) -> Result<TeamChannels, TeamsReadFailure> {
    let mut next = team_channels_url(session.cloud(), team_id)?;
    let mut channels = Vec::new();
    let mut seen = BTreeSet::new();
    let mut truncated = false;

    for page_index in 0..MAX_CHANNEL_PAGES_PER_TEAM {
        let page: GraphCollection<GraphChannel> = get_collection(session, &next, cancellation)?;
        for channel in page.value {
            let Some(id) = channel.id.as_deref().and_then(normalized_identifier) else {
                continue;
            };
            if !seen.insert(id) {
                continue;
            }
            channels.push(channel);
            if channels.len() == MAX_CHANNELS_PER_TEAM {
                truncated = true;
                return Ok(TeamChannels {
                    channels,
                    truncated,
                });
            }
        }
        let Some(next_link) = page.next_link else {
            return Ok(TeamChannels {
                channels,
                truncated,
            });
        };
        next = validated_graph_next_link(session.cloud(), &next_link, next.path())?;
        if page_index + 1 == MAX_CHANNEL_PAGES_PER_TEAM {
            truncated = true;
        }
    }

    Ok(TeamChannels {
        channels,
        truncated,
    })
}

fn normalize_channel(channel: GraphChannel) -> Option<RemoteChannel> {
    let id = channel.id.as_deref().and_then(normalized_identifier)?;
    let display_name = channel
        .display_name
        .as_deref()
        .map(|value| safe_display(value, MAX_CHANNEL_NAME_BYTES))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Unnamed channel".into());
    let description = channel
        .description
        .as_deref()
        .map(|value| safe_display(value, MAX_CHANNEL_DESCRIPTION_BYTES))
        .unwrap_or_default();
    RemoteChannel::microsoft365(
        ProviderResourceRef::microsoft365(id),
        display_name,
        description,
        remote_channel_membership(channel.membership_type.as_deref()),
        channel.is_archived.unwrap_or(false),
    )
    .ok()
}

fn remote_channel_membership(raw: Option<&str>) -> RemoteChannelMembership {
    match raw {
        Some(value) if value.eq_ignore_ascii_case("standard") => RemoteChannelMembership::Standard,
        Some(value) if value.eq_ignore_ascii_case("private") => RemoteChannelMembership::Private,
        Some(value) if value.eq_ignore_ascii_case("shared") => RemoteChannelMembership::Shared,
        _ => RemoteChannelMembership::Unknown,
    }
}

fn team_channels_url(cloud: GraphCloud, team_id: &str) -> Result<Url, TeamsReadFailure> {
    let mut url = cloud
        .graph_route(&["teams", team_id, "channels"], None)
        .map_err(|_| TeamsReadFailure::Unavailable)?;
    url.query_pairs_mut().append_pair(
        "$select",
        "id,displayName,description,membershipType,isArchived",
    );
    Ok(url)
}

fn get_collection<T: DeserializeOwned>(
    session: &GraphSession<'_>,
    url: &Url,
    cancellation: Option<&TransportCancellation>,
) -> Result<GraphCollection<T>, TeamsReadFailure> {
    check_cancelled(cancellation)?;
    let response = session
        .get_json(url, MAX_TEAMS_RESPONSE_BYTES)
        .map_err(map_http_error)?;
    check_cancelled(cancellation)?;
    validate_teams_response_status(&response)?;
    decode_collection(&response.body)
}

/// Decodes one Graph collection page body. Pure, so fuzz targets and tests can
/// reach the production decode path without a session.
fn decode_collection<T: DeserializeOwned>(
    body: &[u8],
) -> Result<GraphCollection<T>, TeamsReadFailure> {
    serde_json::from_slice::<GraphCollection<T>>(body).map_err(|_| TeamsReadFailure::Unavailable)
}

/// Fuzzing seam: decodes `body` as every collection this adapter reads and
/// returns the number of rows seen.
#[cfg(feature = "fuzzing")]
pub(super) fn fuzz_decode(body: &[u8]) -> usize {
    let mut count = 0;
    if let Ok(page) = decode_collection::<GraphTeam>(body) {
        count += page.value.len() + usize::from(page.next_link.is_some());
    }
    if let Ok(page) = decode_collection::<GraphChannel>(body) {
        count += page.value.len() + usize::from(page.next_link.is_some());
    }
    count
}

fn check_cancelled(cancellation: Option<&TransportCancellation>) -> Result<(), TeamsReadFailure> {
    cancellation
        .is_none_or(|cancellation| !cancellation.is_cancelled())
        .then_some(())
        .ok_or(TeamsReadFailure::Cancelled)
}

fn map_http_error(error: GraphHttpError) -> TeamsReadFailure {
    match error {
        GraphHttpError::Network
        | GraphHttpError::ResponseTooLarge
        | GraphHttpError::Unsupported
        | GraphHttpError::RenewedBeforeDelivery
        | GraphHttpError::RenewalUnavailable => TeamsReadFailure::Unavailable,
    }
}

fn validate_teams_response_status(response: &GraphHttpResponse) -> Result<(), TeamsReadFailure> {
    match response.status {
        200 => Ok(()),
        401 => Err(TeamsReadFailure::ReauthenticationRequired),
        403 => Err(TeamsReadFailure::AccessDenied),
        429 => Err(TeamsReadFailure::Throttled(response.retry_after_seconds)),
        _ => Err(TeamsReadFailure::Unavailable),
    }
}

/// Follows a cursor only on the exact route that was requested; a Graph host
/// and API prefix alone could steer the bearer token to another resource.
fn validated_graph_next_link(
    cloud: GraphCloud,
    raw: &str,
    expected_path: &str,
) -> Result<Url, TeamsReadFailure> {
    cloud
        .validated_next_link(raw, expected_path)
        .ok_or(TeamsReadFailure::Unavailable)
}

#[derive(Deserialize)]
struct GraphCollection<T> {
    value: Vec<T>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphTeam {
    id: Option<String>,
    display_name: Option<String>,
    description: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphChannel {
    id: Option<String>,
    display_name: Option<String>,
    description: Option<String>,
    membership_type: Option<String>,
    is_archived: Option<bool>,
}

#[cfg(test)]
mod tests {
    const ASSOCIATED_TEAMS_URL: &str =
        "https://graph.microsoft.com/v1.0/me/teamwork/associatedTeams";
    use super::{
        GraphHttpError, GraphHttpResponse, MAX_CHANNEL_NAME_BYTES, MAX_TEAM_NAME_BYTES,
        Microsoft365Transport, TeamsReadFailure, fetch_associated_team_pages,
        fetch_teams_projection, remote_channel_membership, team_channels_url,
        validated_graph_next_link,
    };
    use crate::integrations::cloud::GraphCloud;
    use crate::integrations::microsoft365::GraphHttpClient;
    use crate::integrations::microsoft365::GraphSession;
    use crate::integrations::transport::RemoteChannelMembership;
    use crate::integrations::{
        AccountIdentity, ConnectionStatus, SyncInterest, TransportCancellation, WorkspaceTransport,
    };
    use std::collections::VecDeque;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use url::Url;

    struct TestHttpClient {
        responses: Mutex<VecDeque<Result<GraphHttpResponse, GraphHttpError>>>,
        requested_urls: Mutex<Vec<String>>,
    }

    impl TestHttpClient {
        fn new(
            responses: impl IntoIterator<Item = Result<GraphHttpResponse, GraphHttpError>>,
        ) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
                requested_urls: Mutex::new(Vec::new()),
            }
        }
    }

    impl GraphHttpClient for TestHttpClient {
        fn get_me(
            &self,
            _url: &Url,
            _access_token: &str,
        ) -> Result<GraphHttpResponse, GraphHttpError> {
            Err(GraphHttpError::Unsupported)
        }

        fn get_json(
            &self,
            url: &Url,
            _access_token: &str,
            _max_response_bytes: u64,
        ) -> Result<GraphHttpResponse, GraphHttpError> {
            self.requested_urls
                .lock()
                .expect("test URL list must not be poisoned")
                .push(url.as_str().into());
            self.responses
                .lock()
                .expect("test response list must not be poisoned")
                .pop_front()
                .expect("a response must be configured")
        }
    }

    /// Cancels exactly after a bounded request has returned, proving the
    /// adapter checks the worker-owned signal before it starts the next
    /// team/channel request.
    struct CancellingHttpClient {
        responses: Mutex<VecDeque<Result<GraphHttpResponse, GraphHttpError>>>,
        requests: AtomicUsize,
        cancellation: TransportCancellation,
        cancel_after_request: usize,
    }

    impl CancellingHttpClient {
        fn new(
            responses: impl IntoIterator<Item = Result<GraphHttpResponse, GraphHttpError>>,
            cancellation: TransportCancellation,
            cancel_after_request: usize,
        ) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
                requests: AtomicUsize::new(0),
                cancellation,
                cancel_after_request,
            }
        }
    }

    impl GraphHttpClient for CancellingHttpClient {
        fn get_me(
            &self,
            _url: &Url,
            _access_token: &str,
        ) -> Result<GraphHttpResponse, GraphHttpError> {
            Err(GraphHttpError::Unsupported)
        }

        fn get_json(
            &self,
            _url: &Url,
            _access_token: &str,
            _max_response_bytes: u64,
        ) -> Result<GraphHttpResponse, GraphHttpError> {
            let request = self.requests.fetch_add(1, Ordering::Relaxed) + 1;
            let response = self
                .responses
                .lock()
                .expect("test response list must not be poisoned")
                .pop_front()
                .expect("a response must be configured");
            if request == self.cancel_after_request {
                self.cancellation.cancel();
            }
            response
        }
    }

    struct ThrottlingHttpClient {
        responses: Mutex<VecDeque<Result<GraphHttpResponse, GraphHttpError>>>,
        requests: AtomicUsize,
    }

    impl ThrottlingHttpClient {
        fn new(
            responses: impl IntoIterator<Item = Result<GraphHttpResponse, GraphHttpError>>,
        ) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
                requests: AtomicUsize::new(0),
            }
        }
    }

    impl GraphHttpClient for ThrottlingHttpClient {
        fn get_me(
            &self,
            _url: &Url,
            _access_token: &str,
        ) -> Result<GraphHttpResponse, GraphHttpError> {
            Err(GraphHttpError::Unsupported)
        }

        fn get_json(
            &self,
            _url: &Url,
            _access_token: &str,
            _max_response_bytes: u64,
        ) -> Result<GraphHttpResponse, GraphHttpError> {
            self.requests.fetch_add(1, Ordering::Relaxed);
            self.responses
                .lock()
                .expect("test response list must not be poisoned")
                .pop_front()
                .expect("a response must be configured")
        }
    }

    fn response(status: u16, body: &str) -> Result<GraphHttpResponse, GraphHttpError> {
        Ok(GraphHttpResponse {
            status,
            retry_after_seconds: None,
            body: body.as_bytes().to_vec(),
        })
    }

    fn throttled(
        body: &str,
        retry_after_seconds: u64,
    ) -> Result<GraphHttpResponse, GraphHttpError> {
        Ok(GraphHttpResponse {
            status: 429,
            retry_after_seconds: Some(retry_after_seconds),
            body: body.as_bytes().to_vec(),
        })
    }

    #[test]
    fn associated_team_directory_normalizes_opaque_rows_and_channel_metadata() {
        let client = TestHttpClient::new([
            response(
                200,
                r#"{"value":[{"id":"team-a","displayName":"  Product\nEngineering  ","description":"Builds <things>"}]}"#,
            ),
            response(
                200,
                r#"{"value":[{"id":"channel-a","displayName":"General","description":"Main room","membershipType":"standard","isArchived":false},{"id":"channel-b","displayName":"Partners","membershipType":"shared","isArchived":true}]}"#,
            ),
        ]);

        let result = fetch_teams_projection(&GraphSession::for_test(&client), None)
            .expect("valid Teams directory must normalize");
        let teams = result.projection.teams();
        assert_eq!(teams.len(), 1);
        assert_eq!(teams[0].display_name(), "Product Engineering");
        assert_eq!(teams[0].description(), "Builds <things>");
        assert_eq!(teams[0].channels().len(), 2);
        assert_eq!(
            teams[0].channels()[1].membership(),
            RemoteChannelMembership::Shared
        );
        assert!(teams[0].channels()[1].is_archived());
        assert!(result.notices.is_empty());

        let requested = client
            .requested_urls
            .lock()
            .expect("test URL list must not be poisoned");
        assert_eq!(requested[0], ASSOCIATED_TEAMS_URL);
        let channel_url = Url::parse(&requested[1]).expect("channel route must be valid");
        assert_eq!(channel_url.path(), "/v1.0/teams/team-a/channels");
        assert!(channel_url.query_pairs().any(|(key, value)| {
            key == "$select" && value == "id,displayName,description,membershipType,isArchived"
        }));
        assert!(
            !channel_url.query_pairs().any(|(key, _)| key == "$top"),
            "the documented channel-list route does not support $top"
        );
    }

    #[test]
    fn channel_route_percent_encodes_provider_identifier_as_one_path_segment() {
        let url = team_channels_url(GraphCloud::Global, "team/id ?#")
            .expect("provider id route must build");
        assert_eq!(url.path(), "/v1.0/teams/team%2Fid%20%3F%23/channels");
        assert!(!url.as_str().contains("team/id ?#"));
    }

    #[test]
    fn associated_team_paging_follows_only_trusted_graph_links() {
        let client = TestHttpClient::new([
            response(
                200,
                r#"{"value":[{"id":"team-a"}],"@odata.nextLink":"https://graph.microsoft.com/v1.0/me/teamwork/associatedTeams?$skiptoken=opaque"}"#,
            ),
            response(200, r#"{"value":[{"id":"team-b"}]}"#),
        ]);

        let (teams, truncated) =
            fetch_associated_team_pages(&GraphSession::for_test(&client), None)
                .expect("trusted continuation must be followed");
        assert_eq!(teams.len(), 2);
        assert!(!truncated);
        let requested = client
            .requested_urls
            .lock()
            .expect("test URL list must not be poisoned");
        assert_eq!(requested[0], ASSOCIATED_TEAMS_URL);
        assert_eq!(
            requested[1],
            "https://graph.microsoft.com/v1.0/me/teamwork/associatedTeams?$skiptoken=opaque"
        );
    }

    #[test]
    fn unsafe_paging_links_never_become_requests() {
        let path = "/v1.0/me/teamwork/associatedTeams";
        for link in [
            "https://example.test/v1.0/me/teamwork/associatedTeams",
            "http://graph.microsoft.com/v1.0/me/teamwork/associatedTeams",
            "https://graph.microsoft.com/beta/me/teamwork/associatedTeams",
            "https://graph.microsoft.com/v1.0/me/chats",
            "https://graph.microsoft.com/v1.0/teams/other/channels",
        ] {
            assert!(
                validated_graph_next_link(GraphCloud::Global, link, path).is_err(),
                "{link}"
            );
        }
        assert!(
            validated_graph_next_link(
                GraphCloud::Global,
                "https://graph.microsoft.com/v1.0/me/teamwork/associatedTeams?$skiptoken=x",
                path
            )
            .is_ok()
        );
    }

    #[test]
    fn nested_channel_throttle_preserves_directory_and_stops_later_fanout() {
        let client = ThrottlingHttpClient::new([
            response(200, r#"{"value":[{"id":"team-a"},{"id":"team-b"}]}"#),
            throttled("{}", 37),
        ]);

        let result = fetch_teams_projection(&GraphSession::for_test(&client), None)
            .expect("a nested throttle must preserve the team directory");
        assert_eq!(result.projection.teams().len(), 2);
        assert!(
            result
                .notices
                .iter()
                .any(|notice| notice.contains("retry after 37 seconds"))
        );
        assert_eq!(
            client.requests.load(Ordering::Relaxed),
            2,
            "a channel throttle must prevent a later channel request"
        );
    }

    #[test]
    fn display_and_membership_normalization_are_bounded_and_safe() {
        let too_long = "a".repeat(MAX_TEAM_NAME_BYTES + 10);
        let channel_long = "b".repeat(MAX_CHANNEL_NAME_BYTES + 10);
        let client = TestHttpClient::new([
            response(
                200,
                &format!(r#"{{"value":[{{"id":"team-a","displayName":"{too_long}"}}]}}"#),
            ),
            response(
                200,
                &format!(
                    r#"{{"value":[{{"id":"channel-a","displayName":"{channel_long}","membershipType":"unknown-from-provider"}}]}}"#
                ),
            ),
        ]);

        let result = fetch_teams_projection(&GraphSession::for_test(&client), None)
            .expect("long text must be bounded rather than reject a directory");
        let team = &result.projection.teams()[0];
        assert_eq!(team.display_name().len(), MAX_TEAM_NAME_BYTES);
        assert_eq!(
            team.channels()[0].display_name().len(),
            MAX_CHANNEL_NAME_BYTES
        );
        assert_eq!(
            team.channels()[0].membership(),
            RemoteChannelMembership::Unknown
        );
        assert_eq!(
            remote_channel_membership(Some("private")),
            RemoteChannelMembership::Private
        );
    }

    #[test]
    fn malformed_directory_response_is_unavailable_not_partially_trusted() {
        let client = TestHttpClient::new([response(200, "{not json")]);
        assert!(matches!(
            fetch_teams_projection(&GraphSession::for_test(&client), None),
            Err(TeamsReadFailure::Unavailable)
        ));
    }

    #[test]
    fn teams_scope_denial_preserves_verified_identity_and_returns_an_empty_directory() {
        let client = Box::new(TestHttpClient::new([response(403, "{}")]));
        let mut transport = Microsoft365Transport::with_client("test-token", client);
        let identity = AccountIdentity {
            account_id: "u1".into(),
            display_name: "Ada".into(),
            email: None,
            tenant_id: None,
        };
        transport.identity = Some(identity.clone());
        transport.status = ConnectionStatus::Connected(identity);

        let delta = transport
            .poll(SyncInterest::Teams)
            .expect("a missing Teams directory scope must not disconnect identity");

        assert!(
            delta
                .remote_teams
                .expect("empty remote directory replacement expected")
                .teams()
                .is_empty()
        );
        assert!(delta.notices[0].contains("Team.ReadBasic.All"));
        assert!(matches!(
            transport.connection_status(),
            ConnectionStatus::Connected(identity) if identity.account_id == "u1"
        ));
    }

    #[test]
    fn cancellation_stops_directory_sync_between_bounded_channel_requests() {
        let cancellation = TransportCancellation::new();
        let client = CancellingHttpClient::new(
            [
                response(200, r#"{"value":[{"id":"team-a"},{"id":"team-b"}]}"#),
                response(200, r#"{"value":[]}"#),
            ],
            cancellation.clone(),
            2,
        );

        let result = fetch_teams_projection(&GraphSession::for_test(&client), Some(&cancellation));

        assert!(matches!(result, Err(TeamsReadFailure::Cancelled)));
        assert_eq!(
            client.requests.load(Ordering::Relaxed),
            2,
            "a cancelled channel request must prevent a later team's fanout"
        );
    }

    #[test]
    fn routes_use_the_configured_clouds_graph_host() {
        for cloud in GraphCloud::ALL {
            let url =
                super::team_channels_url(cloud, "t").unwrap_or_else(|_| panic!("route must build"));
            assert_eq!(url.scheme(), "https");
            assert_eq!(url.host_str(), Some(cloud.graph_host()));
            assert!(url.path().starts_with("/v1.0/"));
        }
    }

    #[test]
    fn directory_and_next_links_follow_the_cloud() {
        for cloud in GraphCloud::ALL {
            let own_next = format!(
                "https://{}/v1.0/me/teamwork/associatedTeams?$skiptoken=x",
                cloud.graph_host()
            );
            let body = format!(r#"{{"value":[],"@odata.nextLink":"{own_next}"}}"#);
            let client =
                TestHttpClient::new([response(200, &body), response(200, r#"{"value":[]}"#)]);
            fetch_associated_team_pages(&GraphSession::for_test_in(cloud, &client), None)
                .unwrap_or_else(|_| panic!("directory must read"));
            let requested = client.requested_urls.lock().expect("lock");
            assert_eq!(
                requested[0],
                format!(
                    "https://{}/v1.0/me/teamwork/associatedTeams",
                    cloud.graph_host()
                )
            );
            assert_eq!(requested[1], own_next);
            drop(requested);

            let cross = GraphCloud::ALL
                .into_iter()
                .find(|other| *other != cloud)
                .expect("another cloud exists");
            let body = format!(
                r#"{{"value":[],"@odata.nextLink":"https://{}/v1.0/me/teamwork/associatedTeams?$skiptoken=x"}}"#,
                cross.graph_host()
            );
            let client = TestHttpClient::new([response(200, &body)]);
            assert!(
                fetch_associated_team_pages(&GraphSession::for_test_in(cloud, &client), None)
                    .is_err()
            );
            assert_eq!(client.requested_urls.lock().expect("lock").len(), 1);
        }
    }
}

#[cfg(test)]
mod decode_collection_tests {
    use super::{GraphCollection, TeamsReadFailure, decode_collection};

    #[test]
    fn decode_collection_reads_a_page_and_its_next_link() {
        let page: GraphCollection<serde_json::Value> = decode_collection(
            br#"{"value":[{"id":"1"},{"id":"2"}],"@odata.nextLink":"https://example.invalid/next"}"#,
        )
        .expect("a well-formed page decodes");
        assert_eq!(page.value.len(), 2);
        assert_eq!(
            page.next_link.as_deref(),
            Some("https://example.invalid/next")
        );
    }

    #[test]
    fn decode_collection_maps_every_malformed_body_to_unavailable() {
        for body in [&b""[..], b"null", b"[]", b"{\"value\":3}", b"\xff\xfe"] {
            assert!(matches!(
                decode_collection::<serde_json::Value>(body),
                Err(TeamsReadFailure::Unavailable)
            ));
        }
    }
}
