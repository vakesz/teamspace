/// A destination discovered in one of the non-chat workspace surfaces.
///
/// The top-bar search only needs this small, serializable description; each
/// owning state module retains its own selection and rendering details.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileViewTarget {
    Recent,
    Shared,
    Downloads,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WorkspaceSearchTarget {
    Activity(usize),
    TeamChannel(String),
    TeamPost {
        channel: String,
        post_key: String,
    },
    CalendarEvent {
        event_key: String,
        title: String,
        week_offset: i32,
        day: usize,
        time: String,
    },
    File {
        name: String,
        source: String,
        when: String,
        kind: String,
        view: FileViewTarget,
    },
    CallContact(String),
}

pub(crate) struct WorkspaceSearchResult {
    pub(crate) label: String,
    pub(crate) detail: String,
    pub(crate) target: WorkspaceSearchTarget,
}
