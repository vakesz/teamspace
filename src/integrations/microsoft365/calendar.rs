//! Microsoft Graph default-calendar read normalization.
//!
//! This module owns the bounded, read-only default-calendar request flow. It
//! deliberately uses an explicit UTC calendar-view window instead of assuming
//! the machine or mailbox timezone, keeps Graph DTOs and continuation links
//! inside the adapter, and returns only normalized session data with opaque
//! provider references. Creating, editing, or cancelling events needs a
//! separate idempotency and reconciliation contract.

use super::GraphSession;
use super::text::{normalized_identifier, safe_display};
use super::{GraphHttpError, GraphHttpResponse, Microsoft365Transport};
use crate::integrations::cloud::GraphCloud;
use crate::integrations::transport::{
    ProviderResourceRef, RemoteCalendar, RemoteCalendarEvent, RemoteCalendarProjection,
};
use crate::integrations::{SyncInterest, TransportCancellation, TransportError, WorkspaceDelta};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

const GRAPH_CALENDAR_VIEW_SEGMENTS: [&str; 3] = ["me", "calendar", "calendarView"];
const CALENDAR_VIEW_PATH: &str = "/v1.0/me/calendar/calendarView";
const MAX_CALENDAR_RESPONSE_BYTES: u64 = 512 * 1024;
const MAX_EVENT_PAGES: usize = 3;
const MAX_REMOTE_EVENTS: usize = 250;
const CALENDAR_VIEW_PAGE_SIZE: usize = 100;
const CALENDAR_VIEW_DAYS_BEFORE_CURRENT_WEEK: i64 = 7;
const CALENDAR_VIEW_DAYS_AFTER_CURRENT_WEEK: i64 = 35;
const MAX_CALENDAR_NAME_BYTES: usize = 256;
const MAX_EVENT_SUBJECT_BYTES: usize = 512;
const MAX_DATETIME_BYTES: usize = 128;
const MAX_TIME_ZONE_BYTES: usize = 128;
const MAX_LOCATION_BYTES: usize = 512;
const MAX_ORGANIZER_BYTES: usize = 256;
const MAX_JOIN_URL_BYTES: usize = 8 * 1024;

/// A normalized calendar projection plus presentation-safe notices about
/// bounds. Provider URLs, tokens, raw response bodies, and error details do
/// not leave this module.
struct CalendarReadResult {
    projection: RemoteCalendarProjection,
    notices: Vec<String>,
}

/// Calendar-read failures intentionally remain a small taxonomy so callers
/// can preserve a verified identity while reporting a narrow provider issue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CalendarReadFailure {
    Cancelled,
    ReauthenticationRequired,
    AccessDenied,
    Throttled(Option<u64>),
    Unavailable,
}

impl CalendarReadFailure {
    fn transport_error(self) -> TransportError {
        match self {
            Self::Cancelled => TransportError::Cancelled,
            Self::ReauthenticationRequired => TransportError::ReauthenticationRequired,
            Self::AccessDenied => TransportError::Unauthorized,
            Self::Throttled(retry_after_seconds) => TransportError::Throttled {
                retry_after_seconds,
            },
            Self::Unavailable => TransportError::Unavailable(
                "Microsoft Graph calendar synchronization is temporarily unavailable".into(),
            ),
        }
    }
}

impl Microsoft365Transport {
    /// Reads a bounded, session-only view of the signed-in user's default
    /// calendar. The interval is derived from UTC so it is not silently
    /// reinterpreted by the machine's local timezone.
    pub(super) fn poll_calendar(&mut self) -> Result<WorkspaceDelta, TransportError> {
        self.poll_calendar_with_cancellation(None)
    }

    /// A remote worker provides this signal so account/profile replacement
    /// prevents the calendar's paged reads from continuing after the adapter
    /// has become stale.
    pub(super) fn poll_calendar_cancellable(
        &mut self,
        cancellation: &TransportCancellation,
    ) -> Result<WorkspaceDelta, TransportError> {
        self.poll_calendar_with_cancellation(Some(cancellation))
    }

    fn poll_calendar_with_cancellation(
        &mut self,
        cancellation: Option<&TransportCancellation>,
    ) -> Result<WorkspaceDelta, TransportError> {
        if let Err(failure) = check_cancelled(cancellation) {
            return Err(failure.transport_error());
        }
        match fetch_calendar_projection(
            &self.session,
            calendar_range_for_now()?,
            cancellation,
        ) {
            Ok(result) => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::Calendar],
                identity: None,
                remote_chats: None,
                remote_chat_messages: None,
                remote_pinned_chat_messages: None,
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: None,
                remote_channel_messages: None,
                remote_channel_files: None,
                remote_channel_replies: None,
                remote_files: None,
                remote_file_preview: None,
                remote_calendar: Some(result.projection),
                remote_capabilities: None,
                notices: result.notices,
            }),
            Err(CalendarReadFailure::AccessDenied) => Ok(WorkspaceDelta {
                // A missing calendar scope must clear stale event rows while
                // retaining a successfully verified Graph identity.
                completed: vec![SyncInterest::Calendar],
                identity: None,
                remote_chats: None,
                remote_chat_messages: None,
                remote_pinned_chat_messages: None,
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: None,
                remote_channel_messages: None,
                remote_channel_files: None,
                remote_channel_replies: None,
                remote_files: None,
                remote_file_preview: None,
                remote_calendar: Some(RemoteCalendarProjection::microsoft365(None)),
                remote_capabilities: None,
                notices: vec![
                    "Microsoft 365 calendar access was not consented. Add delegated Calendars.ReadBasic and reconnect to load the default calendar."
                        .into(),
                ],
            }),
            Err(CalendarReadFailure::ReauthenticationRequired) => {
                let error = TransportError::ReauthenticationRequired;
                self.identity = None;
                self.record_error(&error);
                Err(error)
            }
            Err(CalendarReadFailure::Cancelled) => Err(TransportError::Cancelled),
            Err(failure) => Err(failure.transport_error()),
        }
    }
}

fn calendar_range_for_now() -> Result<CalendarRange, TransportError> {
    let duration = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|_| {
        TransportError::Unavailable("the system clock cannot produce a calendar range".into())
    })?;
    let seconds = i64::try_from(duration.as_secs()).map_err(|_| {
        TransportError::Unavailable("the system clock is outside the calendar range".into())
    })?;
    CalendarRange::from_unix_seconds(seconds).ok_or_else(|| {
        TransportError::Unavailable("the system clock cannot produce a calendar range".into())
    })
}

/// An explicit offset-bearing UTC interval for one bounded Graph calendar
/// view. Graph interprets these values from their own offsets, not the
/// `Prefer: outlook.timezone` response header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CalendarRange {
    start_day: i64,
    end_day: i64,
}

impl CalendarRange {
    fn from_unix_seconds(seconds: i64) -> Option<Self> {
        let utc_day = seconds.div_euclid(86_400);
        // Unix day zero is Thursday; Monday is index zero here.
        let weekday = (utc_day + 3).rem_euclid(7);
        let monday = utc_day.checked_sub(weekday)?;
        let start_day = monday.checked_sub(CALENDAR_VIEW_DAYS_BEFORE_CURRENT_WEEK)?;
        let end_day = monday.checked_add(CALENDAR_VIEW_DAYS_AFTER_CURRENT_WEEK)?;
        (start_day < end_day).then_some(Self { start_day, end_day })
    }

    fn start_rfc3339(self) -> Option<String> {
        format_utc_midnight(self.start_day)
    }

    fn end_rfc3339(self) -> Option<String> {
        format_utc_midnight(self.end_day)
    }
}

fn format_utc_midnight(day: i64) -> Option<String> {
    let (year, month, day) = civil_from_days(day)?;
    Some(format!("{year:04}-{month:02}-{day:02}T00:00:00Z"))
}

/// Inverse of the Gregorian days-from-civil conversion used by the local
/// calendar domain. It is deliberately UTC-only because this integration
/// range must be portable across the user's device and mailbox timezones.
fn civil_from_days(days_since_unix_epoch: i64) -> Option<(i32, u32, u32)> {
    let shifted = days_since_unix_epoch.checked_add(719_468)?;
    let era = if shifted >= 0 {
        shifted / 146_097
    } else {
        (shifted - 146_096) / 146_097
    };
    let day_of_era = shifted.checked_sub(era.checked_mul(146_097)?)?;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era.checked_add(era.checked_mul(400)?)?;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    let year = year + i64::from(month <= 2);
    let year = i32::try_from(year).ok()?;
    let month = u32::try_from(month).ok()?;
    let day = u32::try_from(day).ok()?;
    Some((year, month, day))
}

fn fetch_calendar_projection(
    session: &GraphSession<'_>,
    range: CalendarRange,
    cancellation: Option<&TransportCancellation>,
) -> Result<CalendarReadResult, CalendarReadFailure> {
    check_cancelled(cancellation)?;
    let calendar = fetch_default_calendar(session, cancellation)?;
    check_cancelled(cancellation)?;
    let (events, truncated, mut skipped_events) =
        fetch_calendar_view(session, range, cancellation)?;
    let mut normalized_events = Vec::with_capacity(events.len());
    for event in events {
        check_cancelled(cancellation)?;
        if let Some(event) = normalize_event(event) {
            normalized_events.push(event);
        } else {
            skipped_events = skipped_events.saturating_add(1);
        }
    }

    let calendar = RemoteCalendar::microsoft365(
        ProviderResourceRef::microsoft365(calendar.id),
        calendar.name,
        calendar.can_edit,
        normalized_events,
    )
    .map_err(|_| CalendarReadFailure::Unavailable)?;
    let mut notices = Vec::new();
    if truncated {
        notices.push(format!(
            "Microsoft 365 calendar sync is showing at most {MAX_REMOTE_EVENTS} events from its bounded calendar window."
        ));
    }
    if skipped_events > 0 {
        notices.push(
            "Microsoft Graph returned some calendar events without usable timing or identifiers; they were skipped."
                .into(),
        );
    }
    check_cancelled(cancellation)?;
    Ok(CalendarReadResult {
        projection: RemoteCalendarProjection::microsoft365(Some(calendar)),
        notices,
    })
}

struct DefaultCalendar {
    id: String,
    name: String,
    can_edit: bool,
}

fn fetch_default_calendar(
    session: &GraphSession<'_>,
    cancellation: Option<&TransportCancellation>,
) -> Result<DefaultCalendar, CalendarReadFailure> {
    let url = session
        .cloud()
        .graph_route(&["me", "calendar"], None)
        .map_err(|_| CalendarReadFailure::Unavailable)?;
    let response = get_json(session, &url, cancellation)?;
    let calendar = serde_json::from_slice::<GraphCalendar>(&response.body)
        .map_err(|_| CalendarReadFailure::Unavailable)?;
    let id = calendar
        .id
        .as_deref()
        .and_then(normalized_identifier)
        .ok_or(CalendarReadFailure::Unavailable)?;
    let name = calendar
        .name
        .as_deref()
        .map(|value| safe_display(value, MAX_CALENDAR_NAME_BYTES))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Microsoft 365 calendar".into());
    Ok(DefaultCalendar {
        id,
        name,
        // An absent provider value is not proof that a mutation is safe.
        can_edit: calendar.can_edit.unwrap_or(false),
    })
}

fn fetch_calendar_view(
    session: &GraphSession<'_>,
    range: CalendarRange,
    cancellation: Option<&TransportCancellation>,
) -> Result<(Vec<GraphEvent>, bool, usize), CalendarReadFailure> {
    let mut next = calendar_view_url(session.cloud(), range)?;
    let mut events = Vec::new();
    let mut seen = BTreeSet::new();
    let mut truncated = false;
    let mut skipped_events = 0_usize;

    for page_index in 0..MAX_EVENT_PAGES {
        let page: GraphCollection<GraphEvent> = get_json_collection(session, &next, cancellation)?;
        for event in page.value {
            let Some(id) = event.id.as_deref().and_then(normalized_identifier) else {
                skipped_events = skipped_events.saturating_add(1);
                continue;
            };
            if !seen.insert(id) {
                continue;
            }
            events.push(event);
            if events.len() == MAX_REMOTE_EVENTS {
                truncated = true;
                return Ok((events, truncated, skipped_events));
            }
        }
        let Some(next_link) = page.next_link else {
            return Ok((events, truncated, skipped_events));
        };
        next = validated_graph_next_link(session.cloud(), &next_link)?;
        if page_index + 1 == MAX_EVENT_PAGES {
            truncated = true;
        }
    }
    Ok((events, truncated, skipped_events))
}

fn calendar_view_url(cloud: GraphCloud, range: CalendarRange) -> Result<Url, CalendarReadFailure> {
    let start = range
        .start_rfc3339()
        .ok_or(CalendarReadFailure::Unavailable)?;
    let end = range
        .end_rfc3339()
        .ok_or(CalendarReadFailure::Unavailable)?;
    let mut url = cloud
        .graph_route(&GRAPH_CALENDAR_VIEW_SEGMENTS, None)
        .map_err(|_| CalendarReadFailure::Unavailable)?;
    // `calendarView` documents `startDateTime` and `endDateTime` as required
    // ISO-8601 query parameters, supports `$select`, and allows `$top` up to
    // 1000. Keep the requested page smaller than our hard total bound.
    url.query_pairs_mut()
        .append_pair("startDateTime", &start)
        .append_pair("endDateTime", &end)
        .append_pair(
            "$select",
            "id,subject,start,end,location,organizer,isOnlineMeeting,onlineMeeting,isCancelled",
        )
        .append_pair("$top", &CALENDAR_VIEW_PAGE_SIZE.to_string());
    Ok(url)
}

fn normalize_event(event: GraphEvent) -> Option<RemoteCalendarEvent> {
    let id = event.id.as_deref().and_then(normalized_identifier)?;
    let start = normalize_date_time(event.start.as_ref())?;
    let end = normalize_date_time(event.end.as_ref())?;
    let subject = event
        .subject
        .as_deref()
        .map(|value| safe_display(value, MAX_EVENT_SUBJECT_BYTES))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Untitled event".into());
    let location = event
        .location
        .as_ref()
        .and_then(|location| location.display_name.as_deref())
        .map(|value| safe_display(value, MAX_LOCATION_BYTES))
        .unwrap_or_default();
    let organizer = event
        .organizer
        .as_ref()
        .and_then(|recipient| recipient.email_address.as_ref())
        .and_then(|address| address.name.as_deref().or(address.address.as_deref()))
        .map(|value| safe_display(value, MAX_ORGANIZER_BYTES))
        .unwrap_or_default();
    let is_online_meeting = event.is_online_meeting.unwrap_or(false);
    let join_url = event
        .online_meeting
        .as_ref()
        .and_then(|meeting| meeting.join_url.as_deref())
        .and_then(normalized_https_join_url);
    RemoteCalendarEvent::microsoft365(
        ProviderResourceRef::microsoft365(id),
        subject,
        start.date_time,
        end.date_time,
        start.time_zone,
        location,
        organizer,
        is_online_meeting,
        join_url,
        event.is_cancelled.unwrap_or(false),
    )
    .ok()
}

struct NormalizedDateTime {
    date_time: String,
    time_zone: String,
}

fn normalize_date_time(value: Option<&GraphDateTimeTimeZone>) -> Option<NormalizedDateTime> {
    let value = value?;
    let date_time = value
        .date_time
        .as_deref()
        .map(|value| safe_display(value, MAX_DATETIME_BYTES))
        .filter(|value| !value.is_empty())?;
    let time_zone = value
        .time_zone
        .as_deref()
        .map(|value| safe_display(value, MAX_TIME_ZONE_BYTES))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "UTC".into());
    Some(NormalizedDateTime {
        date_time,
        time_zone,
    })
}

/// Meeting join links are not opened or logged by the adapter. Retaining a
/// bounded HTTPS URL supports the app's explicit UI handoff without treating
/// arbitrary event text as a browser destination.
fn normalized_https_join_url(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.len() > MAX_JOIN_URL_BYTES
        || trimmed.chars().any(char::is_control)
        || !has_explicit_https_authority(trimmed)
    {
        return None;
    }
    let url = Url::parse(trimmed).ok()?;
    (url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none())
    .then(|| trimmed.into())
}

/// `url::Url` is permissive for special schemes and can normalize a malformed
/// one-slash form such as `https:/host`. Graph meeting links must use an
/// explicit `https://` authority before the later browser boundary receives
/// them.
fn has_explicit_https_authority(value: &str) -> bool {
    let Some(scheme_end) = value.find(':') else {
        return false;
    };
    let Some(authority) = value.get(scheme_end + 1..) else {
        return false;
    };
    value[..scheme_end].eq_ignore_ascii_case("https")
        && authority.starts_with("//")
        && authority
            .get(2..)
            .is_some_and(|rest| !rest.starts_with('/'))
}

fn get_json_collection<T: DeserializeOwned>(
    session: &GraphSession<'_>,
    url: &Url,
    cancellation: Option<&TransportCancellation>,
) -> Result<GraphCollection<T>, CalendarReadFailure> {
    let response = get_json(session, url, cancellation)?;
    decode_collection(&response.body)
}

/// Decodes one Graph collection page body. Pure, so fuzz targets and tests can
/// reach the production decode path without a session.
fn decode_collection<T: DeserializeOwned>(
    body: &[u8],
) -> Result<GraphCollection<T>, CalendarReadFailure> {
    serde_json::from_slice::<GraphCollection<T>>(body).map_err(|_| CalendarReadFailure::Unavailable)
}

/// Fuzzing seam: decodes `body` as every collection this adapter reads and
/// returns the number of rows seen.
#[cfg(feature = "fuzzing")]
pub(super) fn fuzz_decode(body: &[u8]) -> usize {
    let mut count = 0;
    if let Ok(page) = decode_collection::<GraphEvent>(body) {
        count += page.value.len() + usize::from(page.next_link.is_some());
    }
    count
}

fn get_json(
    session: &GraphSession<'_>,
    url: &Url,
    cancellation: Option<&TransportCancellation>,
) -> Result<GraphHttpResponse, CalendarReadFailure> {
    check_cancelled(cancellation)?;
    let response = session
        .get_json(url, MAX_CALENDAR_RESPONSE_BYTES)
        .map_err(map_http_error)?;
    check_cancelled(cancellation)?;
    validate_calendar_response_status(&response)?;
    Ok(response)
}

fn check_cancelled(
    cancellation: Option<&TransportCancellation>,
) -> Result<(), CalendarReadFailure> {
    cancellation
        .is_none_or(|cancellation| !cancellation.is_cancelled())
        .then_some(())
        .ok_or(CalendarReadFailure::Cancelled)
}

fn map_http_error(error: GraphHttpError) -> CalendarReadFailure {
    match error {
        GraphHttpError::Network
        | GraphHttpError::ResponseTooLarge
        | GraphHttpError::Unsupported
        | GraphHttpError::RenewedBeforeDelivery
        | GraphHttpError::RenewalUnavailable => CalendarReadFailure::Unavailable,
    }
}

fn validate_calendar_response_status(
    response: &GraphHttpResponse,
) -> Result<(), CalendarReadFailure> {
    match response.status {
        200 => Ok(()),
        401 => Err(CalendarReadFailure::ReauthenticationRequired),
        403 => Err(CalendarReadFailure::AccessDenied),
        429 => Err(CalendarReadFailure::Throttled(response.retry_after_seconds)),
        _ => Err(CalendarReadFailure::Unavailable),
    }
}

fn validated_graph_next_link(cloud: GraphCloud, raw: &str) -> Result<Url, CalendarReadFailure> {
    // A Graph-issued continuation is opaque, but it must continue this
    // exact bounded read. Do not let it steer a token-bearing request to
    // another otherwise trusted Graph v1.0 route.
    cloud
        .validated_next_link(raw, CALENDAR_VIEW_PATH)
        .ok_or(CalendarReadFailure::Unavailable)
}

#[derive(Deserialize)]
struct GraphCollection<T> {
    value: Vec<T>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphCalendar {
    id: Option<String>,
    name: Option<String>,
    can_edit: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphEvent {
    id: Option<String>,
    subject: Option<String>,
    start: Option<GraphDateTimeTimeZone>,
    end: Option<GraphDateTimeTimeZone>,
    location: Option<GraphLocation>,
    organizer: Option<GraphRecipient>,
    is_online_meeting: Option<bool>,
    online_meeting: Option<GraphOnlineMeetingInfo>,
    is_cancelled: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphDateTimeTimeZone {
    date_time: Option<String>,
    time_zone: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphLocation {
    display_name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphRecipient {
    email_address: Option<GraphEmailAddress>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphEmailAddress {
    name: Option<String>,
    address: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphOnlineMeetingInfo {
    join_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{
        CALENDAR_VIEW_PAGE_SIZE, CalendarRange, CalendarReadFailure, GraphHttpError,
        GraphHttpResponse, Microsoft365Transport, calendar_view_url, fetch_calendar_projection,
        format_utc_midnight, normalized_https_join_url, validate_calendar_response_status,
        validated_graph_next_link,
    };
    use crate::integrations::cloud::GraphCloud;
    use crate::integrations::microsoft365::GraphHttpClient;
    use crate::integrations::microsoft365::GraphSession;
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

    fn response(status: u16, body: &str) -> Result<GraphHttpResponse, GraphHttpError> {
        Ok(GraphHttpResponse {
            status,
            retry_after_seconds: None,
            body: body.as_bytes().to_vec(),
        })
    }

    #[test]
    fn calendar_projection_normalizes_bounded_event_metadata_and_https_join_link() {
        let client = TestHttpClient::new([
            response(
                200,
                r#"{"id":"calendar-1","name":"  Work\nCalendar ","canEdit":true}"#,
            ),
            response(
                200,
                r#"{"value":[{"id":"event-1","subject":"  Sprint\nplanning  ","start":{"dateTime":"2026-11-03T09:00:00.0000000","timeZone":"UTC"},"end":{"dateTime":"2026-11-03T10:00:00.0000000","timeZone":"UTC"},"location":{"displayName":"Room 5"},"organizer":{"emailAddress":{"name":"Ada Lovelace","address":"ada@example.test"}},"isOnlineMeeting":true,"onlineMeeting":{"joinUrl":"https://teams.microsoft.com/l/meetup-join/example"}}]}"#,
            ),
        ]);
        let range =
            CalendarRange::from_unix_seconds(1_793_620_800).expect("fixture range must be valid");

        let result = fetch_calendar_projection(&GraphSession::for_test(&client), range, None)
            .expect("valid calendar response must normalize");
        let calendar = result
            .projection
            .calendar()
            .expect("a default calendar must be present");
        assert_eq!(calendar.display_name(), "Work Calendar");
        assert!(calendar.can_edit());
        assert_eq!(calendar.events().len(), 1);
        let event = &calendar.events()[0];
        assert_eq!(event.title(), "Sprint planning");
        assert_eq!(event.starts_at(), "2026-11-03T09:00:00.0000000");
        assert_eq!(event.time_zone(), "UTC");
        assert_eq!(event.location(), "Room 5");
        assert_eq!(event.organizer(), "Ada Lovelace");
        assert!(event.has_online_meeting());
        assert_eq!(
            event.join_url(),
            Some("https://teams.microsoft.com/l/meetup-join/example")
        );
        assert!(result.notices.is_empty());

        let requested = client
            .requested_urls
            .lock()
            .expect("test URL list must not be poisoned");
        assert_eq!(requested[0], "https://graph.microsoft.com/v1.0/me/calendar");
        let view = Url::parse(&requested[1]).expect("calendar-view request must be a URL");
        assert_eq!(view.path(), "/v1.0/me/calendar/calendarView");
        assert!(
            view.query_pairs().any(|(key, value)| {
                key == "$top" && value == CALENDAR_VIEW_PAGE_SIZE.to_string()
            })
        );
        assert!(
            view.query_pairs()
                .any(|(key, value)| { key == "$select" && value.contains("onlineMeeting") })
        );
    }

    #[test]
    fn calendar_range_is_offset_bearing_and_includes_the_prior_week() {
        let range = CalendarRange::from_unix_seconds(0).expect("epoch range must be representable");
        assert_eq!(
            range.start_rfc3339().as_deref(),
            Some("1969-12-22T00:00:00Z")
        );
        assert_eq!(range.end_rfc3339().as_deref(), Some("1970-02-02T00:00:00Z"));
        assert_eq!(
            format_utc_midnight(0).as_deref(),
            Some("1970-01-01T00:00:00Z")
        );
    }

    #[test]
    fn calendar_view_route_carries_only_documented_bounded_parameters() {
        let range = CalendarRange::from_unix_seconds(0).expect("fixture range must be valid");
        let url =
            calendar_view_url(GraphCloud::Global, range).expect("calendar-view URL must build");
        assert!(
            url.query_pairs()
                .any(|(key, value)| { key == "startDateTime" && value == "1969-12-22T00:00:00Z" })
        );
        assert!(
            url.query_pairs()
                .any(|(key, value)| { key == "endDateTime" && value == "1970-02-02T00:00:00Z" })
        );
        assert!(
            url.query_pairs()
                .any(|(key, value)| key == "$top" && value == "100")
        );
    }

    #[test]
    fn calendar_paging_follows_only_trusted_graph_links() {
        let client = TestHttpClient::new([
            response(200, r#"{"id":"calendar-1"}"#),
            response(
                200,
                r#"{"value":[{"id":"event-1","start":{"dateTime":"2026-11-03T09:00:00","timeZone":"UTC"},"end":{"dateTime":"2026-11-03T10:00:00","timeZone":"UTC"}}],"@odata.nextLink":"https://graph.microsoft.com/v1.0/me/calendar/calendarView?$skiptoken=opaque"}"#,
            ),
            response(
                200,
                r#"{"value":[{"id":"event-2","start":{"dateTime":"2026-11-04T09:00:00","timeZone":"UTC"},"end":{"dateTime":"2026-11-04T10:00:00","timeZone":"UTC"}}]}"#,
            ),
        ]);
        let range = CalendarRange::from_unix_seconds(0).expect("fixture range must be valid");
        let result = fetch_calendar_projection(&GraphSession::for_test(&client), range, None)
            .expect("trusted continuation must be followed");
        assert_eq!(
            result
                .projection
                .calendar()
                .expect("calendar expected")
                .events()
                .len(),
            2
        );
        let requested = client
            .requested_urls
            .lock()
            .expect("test URL list must not be poisoned");
        assert_eq!(requested.len(), 3);
        assert_eq!(
            requested[2],
            "https://graph.microsoft.com/v1.0/me/calendar/calendarView?$skiptoken=opaque"
        );
    }

    #[test]
    fn untrusted_calendar_paging_links_are_rejected() {
        assert!(
            validated_graph_next_link(
                GraphCloud::Global,
                "https://example.test/v1.0/me/calendar/calendarView"
            )
            .is_err()
        );
        assert!(
            validated_graph_next_link(
                GraphCloud::Global,
                "http://graph.microsoft.com/v1.0/me/calendar/calendarView"
            )
            .is_err()
        );
        assert!(
            validated_graph_next_link(
                GraphCloud::Global,
                "https://graph.microsoft.com/beta/me/calendar/calendarView"
            )
            .is_err()
        );
        assert!(
            validated_graph_next_link(
                GraphCloud::Global,
                "https://graph.microsoft.com/v1.0/me/calendar/calendarView"
            )
            .is_ok()
        );
        assert!(
            validated_graph_next_link(
                GraphCloud::Global,
                "https://graph.microsoft.com/v1.0/me/events"
            )
            .is_err(),
            "a calendar continuation must not redirect a token-bearing request to another Graph route"
        );
    }

    #[test]
    fn malformed_event_timing_is_skipped_with_a_safe_notice() {
        let client = TestHttpClient::new([
            response(200, r#"{"id":"calendar-1"}"#),
            response(
                200,
                r#"{"value":[{"id":"event-1","start":{"timeZone":"UTC"},"end":{"dateTime":"2026-11-03T10:00:00","timeZone":"UTC"}}]}"#,
            ),
        ]);
        let range = CalendarRange::from_unix_seconds(0).expect("fixture range must be valid");

        let result = fetch_calendar_projection(&GraphSession::for_test(&client), range, None)
            .expect("a malformed event must not poison the calendar projection");
        assert!(
            result
                .projection
                .calendar()
                .expect("calendar expected")
                .events()
                .is_empty()
        );
        assert_eq!(
            result.notices,
            vec![String::from(
                "Microsoft Graph returned some calendar events without usable timing or identifiers; they were skipped."
            )]
        );
    }

    #[test]
    fn calendar_status_maps_reauthentication_and_throttling_without_error_bodies() {
        assert_eq!(
            validate_calendar_response_status(&GraphHttpResponse {
                status: 401,
                retry_after_seconds: None,
                body: Vec::new(),
            }),
            Err(CalendarReadFailure::ReauthenticationRequired)
        );
        assert_eq!(
            validate_calendar_response_status(&GraphHttpResponse {
                status: 429,
                retry_after_seconds: Some(30),
                body: Vec::new(),
            }),
            Err(CalendarReadFailure::Throttled(Some(30)))
        );
    }

    #[test]
    fn calendar_scope_denial_preserves_verified_identity_and_clears_projection() {
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
            .poll(SyncInterest::Calendar)
            .expect("a missing calendar scope must not disconnect identity");
        assert!(
            delta
                .remote_calendar
                .expect("empty remote calendar replacement expected")
                .calendar()
                .is_none()
        );
        assert!(delta.notices[0].contains("Calendars.ReadBasic"));
        assert!(matches!(
            transport.connection_status(),
            ConnectionStatus::Connected(identity) if identity.account_id == "u1"
        ));
    }

    #[test]
    fn cancellation_stops_calendar_sync_before_the_view_request() {
        let cancellation = TransportCancellation::new();
        let client = CancellingHttpClient::new(
            [response(200, r#"{"id":"calendar-1"}"#)],
            cancellation.clone(),
            1,
        );
        let range = CalendarRange::from_unix_seconds(0).expect("fixture range must be valid");

        let result =
            fetch_calendar_projection(&GraphSession::for_test(&client), range, Some(&cancellation));

        assert!(matches!(result, Err(CalendarReadFailure::Cancelled)));
        assert_eq!(
            client.requests.load(Ordering::Relaxed),
            1,
            "cancellation after default-calendar discovery must prevent calendarView"
        );
    }

    #[test]
    fn join_links_require_bounded_https_without_credentials() {
        assert_eq!(
            normalized_https_join_url("https://teams.microsoft.com/l/meetup-join/example"),
            Some("https://teams.microsoft.com/l/meetup-join/example".into())
        );
        assert_eq!(
            normalized_https_join_url("http://teams.microsoft.com/x"),
            None
        );
        assert_eq!(
            normalized_https_join_url("https:/teams.microsoft.com/l/meetup-join/example"),
            None
        );
        assert_eq!(
            normalized_https_join_url("https:////teams.microsoft.com/l/meetup-join/example"),
            None
        );
        assert_eq!(
            normalized_https_join_url("https://user:pass@example.test/x"),
            None
        );
        assert_eq!(
            normalized_https_join_url("https://example.test/\nnext"),
            None
        );
    }

    #[test]
    fn calendar_routes_and_next_links_follow_the_cloud() {
        let range = CalendarRange::from_unix_seconds(0).expect("fixture range must be valid");
        for cloud in GraphCloud::ALL {
            let url = calendar_view_url(cloud, range)
                .unwrap_or_else(|_| panic!("calendar-view URL must build"));
            assert_eq!(url.host_str(), Some(cloud.graph_host()));
            assert_eq!(url.path(), "/v1.0/me/calendar/calendarView");
            let own = format!(
                "https://{}/v1.0/me/calendar/calendarView?$skiptoken=x",
                cloud.graph_host()
            );
            assert!(validated_graph_next_link(cloud, &own).is_ok());
            for other in GraphCloud::ALL.into_iter().filter(|other| *other != cloud) {
                let cross = format!(
                    "https://{}/v1.0/me/calendar/calendarView?$skiptoken=x",
                    other.graph_host()
                );
                assert!(validated_graph_next_link(cloud, &cross).is_err());
            }
        }
    }
}

#[cfg(test)]
mod decode_collection_tests {
    use super::{CalendarReadFailure, GraphCollection, decode_collection};

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
                Err(CalendarReadFailure::Unavailable)
            ));
        }
    }
}
