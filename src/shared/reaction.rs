//! The app-supported, provider-neutral message-reaction palette.
//!
//! This type sits below features and integrations: it describes the bounded
//! set of reactions Teamspace can render, persist, and reconcile. Provider
//! adapters map their wire representations at their own boundary rather than
//! allowing arbitrary provider strings into application state.

/// One reaction that Teamspace can present and mutate.
///
/// The list intentionally remains closed. Microsoft 365 can return custom
/// and tenant-configured reactions, but those need a separate rendering and
/// mutation contract before they can safely become interactive here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum ReactionKind {
    Like,
    Heart,
    Laugh,
    Surprised,
    Sad,
    Angry,
}

impl ReactionKind {
    pub(crate) const ALL: [Self; 6] = [
        Self::Like,
        Self::Heart,
        Self::Laugh,
        Self::Surprised,
        Self::Sad,
        Self::Angry,
    ];

    pub(crate) const fn icon(self) -> &'static str {
        match self {
            Self::Like => "👍",
            Self::Heart => "❤",
            Self::Laugh => "😄",
            Self::Surprised => "😮",
            Self::Sad => "😢",
            Self::Angry => "😡",
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Like => "like",
            Self::Heart => "heart",
            Self::Laugh => "laugh",
            Self::Surprised => "surprised",
            Self::Sad => "sad",
            Self::Angry => "angry",
        }
    }

    pub(crate) fn from_seed(value: &str) -> Option<Self> {
        match value {
            "like" => Some(Self::Like),
            "heart" => Some(Self::Heart),
            "laugh" => Some(Self::Laugh),
            "surprised" => Some(Self::Surprised),
            "sad" => Some(Self::Sad),
            "angry" => Some(Self::Angry),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ReactionKind;

    #[test]
    fn supported_palette_has_stable_seed_labels() {
        let labels = ReactionKind::ALL.map(ReactionKind::label);
        assert_eq!(
            labels,
            ["like", "heart", "laugh", "surprised", "sad", "angry"]
        );
        assert_eq!(ReactionKind::from_seed("sad"), Some(ReactionKind::Sad));
        assert_eq!(ReactionKind::from_seed("custom"), None);
    }
}
