use super::{ProviderResourceRef, RemoteMessageInstant, TransportError};

/// A provider-normalized snapshot of the signed-in user's default calendar.
///
/// Calendar data is strictly session-only. Presentation code may display its
/// safe fields and retain opaque provider targets, but must never merge it
/// with local-preview events or persist it in a profile snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RemoteCalendarProjection {
    calendar: Option<RemoteCalendar>,
}

impl RemoteCalendarProjection {
    /// Constructs a normalized response only inside integrations after the
    /// provider has validated its default-calendar and calendar-view data.
    /// `None` is a valid empty replacement when a delegated scope is absent.
    pub(in crate::integrations) fn microsoft365(calendar: Option<RemoteCalendar>) -> Self {
        Self { calendar }
    }

    pub(crate) fn calendar(&self) -> Option<&RemoteCalendar> {
        self.calendar.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn for_test(calendar: Option<RemoteCalendar>) -> Self {
        Self::microsoft365(calendar)
    }
}

/// A bounded view of a provider-owned default calendar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteCalendar {
    // Retain the provider-issued calendar identity inside the integration
    // model for a future provider-aware mutation contract. It deliberately
    // does not cross the current read-only calendar presentation seam.
    #[allow(dead_code)]
    target: ProviderResourceRef,
    display_name: String,
    can_edit: bool,
    events: Vec<RemoteCalendarEvent>,
}

impl RemoteCalendar {
    pub(in crate::integrations) fn microsoft365(
        target: ProviderResourceRef,
        display_name: String,
        can_edit: bool,
        events: Vec<RemoteCalendarEvent>,
    ) -> Result<Self, TransportError> {
        target.require_microsoft365("calendar")?;
        target.validate("calendar id")?;
        Ok(Self {
            target,
            display_name,
            can_edit,
            events,
        })
    }

    pub(crate) fn display_name(&self) -> &str {
        &self.display_name
    }

    pub(crate) const fn can_edit(&self) -> bool {
        self.can_edit
    }

    pub(crate) fn events(&self) -> &[RemoteCalendarEvent] {
        &self.events
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        provider_id: impl Into<String>,
        display_name: impl Into<String>,
        can_edit: bool,
        events: Vec<RemoteCalendarEvent>,
    ) -> Self {
        Self::microsoft365(
            ProviderResourceRef::microsoft365(provider_id),
            display_name.into(),
            can_edit,
            events,
        )
        .expect("test remote calendar targets must be valid Microsoft 365 identifiers")
    }
}

/// A presentation-safe event from the signed-in user's default calendar.
///
/// It carries no HTML body, attachments, or mutable Graph fields. A future
/// remote edit/delete contract must introduce its own delivery semantics
/// before this opaque event target is ever used as a command target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteCalendarEvent {
    target: ProviderResourceRef,
    title: String,
    starts_at: String,
    ends_at: String,
    /// The start and end as instants, when the provider zone is UTC (the
    /// default Graph answers in) or the text carries its own offset. `None`
    /// otherwise, so the provider text is shown as it came.
    starts_instant: Option<RemoteMessageInstant>,
    ends_instant: Option<RemoteMessageInstant>,
    time_zone: String,
    location: String,
    organizer: String,
    online_meeting: bool,
    join_url: Option<String>,
    cancelled: bool,
}

/// A Graph date-time is local to `time_zone` and has no offset of its own, so
/// only a UTC one (or one that spells its offset) names an exact instant.
fn event_instant(value: &str, time_zone: &str) -> Option<RemoteMessageInstant> {
    RemoteMessageInstant::parse(value).or_else(|| {
        time_zone
            .eq_ignore_ascii_case("UTC")
            .then(|| RemoteMessageInstant::parse(&format!("{}Z", value.trim())))
            .flatten()
    })
}

impl RemoteCalendarEvent {
    #[allow(clippy::too_many_arguments)]
    pub(in crate::integrations) fn microsoft365(
        target: ProviderResourceRef,
        title: String,
        starts_at: String,
        ends_at: String,
        time_zone: String,
        location: String,
        organizer: String,
        online_meeting: bool,
        join_url: Option<String>,
        cancelled: bool,
    ) -> Result<Self, TransportError> {
        target.require_microsoft365("calendar event")?;
        target.validate("calendar event id")?;
        Ok(Self {
            target,
            title,
            starts_instant: event_instant(&starts_at, &time_zone),
            ends_instant: event_instant(&ends_at, &time_zone),
            starts_at,
            ends_at,
            time_zone,
            location,
            organizer,
            online_meeting,
            join_url,
            cancelled,
        })
    }

    pub(crate) fn target(&self) -> ProviderResourceRef {
        self.target.clone()
    }

    pub(crate) fn title(&self) -> &str {
        &self.title
    }

    pub(crate) fn starts_at(&self) -> &str {
        &self.starts_at
    }

    pub(crate) fn ends_at(&self) -> &str {
        &self.ends_at
    }

    pub(crate) const fn starts_instant(&self) -> Option<RemoteMessageInstant> {
        self.starts_instant
    }

    pub(crate) const fn ends_instant(&self) -> Option<RemoteMessageInstant> {
        self.ends_instant
    }

    pub(crate) fn time_zone(&self) -> &str {
        &self.time_zone
    }

    pub(crate) fn location(&self) -> &str {
        &self.location
    }

    pub(crate) fn organizer(&self) -> &str {
        &self.organizer
    }

    pub(crate) const fn has_online_meeting(&self) -> bool {
        self.online_meeting
    }

    /// A provider-returned, integration-normalized meeting URL. The projection
    /// itself never opens it: the Calendar feature may retain it only for an
    /// explicit user click, and the app shell revalidates HTTPS before its
    /// one-shot system-browser handoff.
    pub(crate) fn join_url(&self) -> Option<&str> {
        self.join_url.as_deref()
    }

    pub(crate) const fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn for_test(
        provider_id: impl Into<String>,
        title: impl Into<String>,
        starts_at: impl Into<String>,
        ends_at: impl Into<String>,
        time_zone: impl Into<String>,
        location: impl Into<String>,
        organizer: impl Into<String>,
        online_meeting: bool,
        join_url: Option<String>,
        cancelled: bool,
    ) -> Self {
        Self::microsoft365(
            ProviderResourceRef::microsoft365(provider_id),
            title.into(),
            starts_at.into(),
            ends_at.into(),
            time_zone.into(),
            location.into(),
            organizer.into(),
            online_meeting,
            join_url,
            cancelled,
        )
        .expect("test remote calendar event targets must be valid Microsoft 365 identifiers")
    }
}
