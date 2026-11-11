//! Microsoft Graph reaction-wire normalization for Teamspace's fixed palette.
//!
//! Graph v1 accepts Unicode in `setReaction` and `unsetReaction` payloads,
//! while read projections can contain arbitrary Unicode, legacy names, and
//! custom reaction metadata. Only exact byte-for-byte matches for the Unicode
//! values this app writes cross this module boundary; all other values remain
//! provider-private until they have a separately verified safe UX.

use crate::integrations::RemoteMessageReaction;
use crate::shared::reaction::ReactionKind;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use zeroize::Zeroizing;

/// Deserializes a Graph `reactions` array one entry at a time. A null or
/// non-array value is empty and a malformed entry is skipped, so optional
/// presentation metadata never discards an otherwise valid message row.
pub(super) fn deserialize_reactions<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let Some(serde_json::Value::Array(values)) =
        Option::<serde_json::Value>::deserialize(deserializer)?
    else {
        return Ok(Vec::new());
    };
    Ok(values
        .into_iter()
        .filter_map(|value| serde_json::from_value(value).ok())
        .collect())
}

/// The `setReaction`/`unsetReaction` request body, shared by chat and channel
/// reactions. Only the app palette's exact Unicode can be serialized.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SetReaction<'a> {
    reaction_type: &'a str,
}

/// Serializes the write body for `kind` into a short-lived zeroizing buffer.
/// The HTTP seam consumes it for one status-only non-idempotent request and
/// never exposes a provider response body to callers.
pub(super) fn reaction_request_body(kind: ReactionKind) -> Option<Zeroizing<Vec<u8>>> {
    serde_json::to_vec(&SetReaction {
        reaction_type: graph_reaction_type(kind),
    })
    .ok()
    .map(Zeroizing::new)
}

/// The exact Unicode payload used for an application-supported kind.
///
/// This deliberately does not forward a read-side Graph string. A response
/// can contain other Unicode, legacy aliases, or custom metadata that must
/// never become a writable provider value.
pub(super) const fn graph_reaction_type(kind: ReactionKind) -> &'static str {
    kind.icon()
}

/// Maps Graph's read-side reaction values to the application palette.
///
/// Graph returns either the Unicode emoji or one of its legacy names (`like`,
/// `heart`, `laugh`, `surprised`, `sad`, `angry`, any case), and an emoji may
/// carry a trailing emoji variation selector (U+FE0F). These are all read-side
/// display aliases of the same kind. Custom and unknown values stay hidden.
/// Writes still use only [`graph_reaction_type`]; whether Graph's unset call
/// removes a reaction stored under a legacy name is not verified.
pub(super) fn normalize_graph_reaction_type(value: &str) -> Option<ReactionKind> {
    let trimmed = value.trim_end_matches('\u{FE0F}');
    match trimmed {
        "👍" => return Some(ReactionKind::Like),
        "❤" => return Some(ReactionKind::Heart),
        "😄" => return Some(ReactionKind::Laugh),
        "😮" => return Some(ReactionKind::Surprised),
        "😢" => return Some(ReactionKind::Sad),
        "😡" => return Some(ReactionKind::Angry),
        _ => {}
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "like" => Some(ReactionKind::Like),
        "heart" => Some(ReactionKind::Heart),
        "laugh" => Some(ReactionKind::Laugh),
        "surprised" => Some(ReactionKind::Surprised),
        "sad" => Some(ReactionKind::Sad),
        "angry" => Some(ReactionKind::Angry),
        _ => None,
    }
}

/// Aggregates only supported reaction kinds from one bounded Graph message.
///
/// The closures keep Graph DTO shape private to each read adapter. The
/// fixed outer order makes feature/UI state deterministic even if Graph
/// reorders reactions in a later refresh.
pub(super) fn normalize_reaction_summaries<T>(
    reactions: &[T],
    current_account_id: Option<&str>,
    reaction_type: impl Fn(&T) -> Option<&str>,
    reacting_user_id: impl Fn(&T) -> Option<&str>,
) -> Vec<RemoteMessageReaction> {
    ReactionKind::ALL
        .into_iter()
        .filter_map(|kind| {
            let mut count = 0_usize;
            let mut reacted_by_current_user = false;
            for reaction in reactions {
                if reaction_type(reaction).and_then(normalize_graph_reaction_type) != Some(kind) {
                    continue;
                }
                count = count.saturating_add(1);
                reacted_by_current_user |= current_account_id
                    .is_some_and(|account_id| reacting_user_id(reaction) == Some(account_id));
            }
            (count != 0).then_some(RemoteMessageReaction::new(
                kind,
                count,
                reacted_by_current_user,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn write_body_is_the_exact_unicode_for_every_palette_kind() {
        for kind in ReactionKind::ALL {
            let body = super::reaction_request_body(kind).expect("body must serialize");
            let value: serde_json::Value =
                serde_json::from_slice(&body).expect("body must be JSON");
            assert_eq!(value, serde_json::json!({"reactionType": kind.icon()}));
        }
    }

    use super::{graph_reaction_type, normalize_graph_reaction_type, normalize_reaction_summaries};
    use crate::shared::reaction::ReactionKind;
    use serde::Deserialize;

    #[test]
    fn maps_canonical_values_and_rejects_unknown_ones() {
        assert_eq!(
            normalize_graph_reaction_type("👍"),
            Some(ReactionKind::Like)
        );
        assert_eq!(
            normalize_graph_reaction_type("❤"),
            Some(ReactionKind::Heart)
        );
        assert_eq!(
            normalize_graph_reaction_type("😄"),
            Some(ReactionKind::Laugh)
        );
        assert_eq!(
            normalize_graph_reaction_type("😮"),
            Some(ReactionKind::Surprised)
        );
        assert_eq!(normalize_graph_reaction_type("😢"), Some(ReactionKind::Sad));
        assert_eq!(
            normalize_graph_reaction_type("😡"),
            Some(ReactionKind::Angry)
        );
        for unverified in ["😂", "💘", "likes", "like me", "👍👍"] {
            assert_eq!(
                normalize_graph_reaction_type(unverified),
                None,
                "{unverified}"
            );
        }
        assert_eq!(normalize_graph_reaction_type("custom"), None);
        assert_eq!(normalize_graph_reaction_type("💘"), None);
    }

    #[test]
    fn legacy_names_case_and_variation_selectors_normalize() {
        for (value, kind) in [
            ("like", ReactionKind::Like),
            ("Heart", ReactionKind::Heart),
            ("LAUGH", ReactionKind::Laugh),
            ("surprised", ReactionKind::Surprised),
            ("sad", ReactionKind::Sad),
            ("angry", ReactionKind::Angry),
            ("❤\u{FE0F}", ReactionKind::Heart),
            ("👍\u{FE0F}", ReactionKind::Like),
            ("😡\u{FE0F}", ReactionKind::Angry),
        ] {
            assert_eq!(
                normalize_graph_reaction_type(value),
                Some(kind),
                "{value:?}"
            );
        }
        assert_eq!(normalize_graph_reaction_type("custom"), None);
        assert_eq!(normalize_graph_reaction_type("\u{FE0F}"), None);
    }

    #[test]
    fn lenient_reactions_treat_null_and_non_array_as_empty_and_skip_bad_entries() {
        #[derive(Deserialize)]
        struct Holder {
            #[serde(default, deserialize_with = "super::deserialize_reactions")]
            reactions: Vec<Entry>,
        }
        #[derive(Deserialize)]
        struct Entry {
            #[serde(rename = "reactionType")]
            kind: String,
        }
        for empty in [r#"{"reactions":null}"#, r#"{"reactions":"x"}"#, "{}"] {
            let holder: Holder = serde_json::from_str(empty).expect(empty);
            assert!(holder.reactions.is_empty(), "{empty}");
        }
        let holder: Holder = serde_json::from_str(
            r#"{"reactions":[{"reactionType":"like"},{"reactionType":7},null,{"reactionType":"sad"}]}"#,
        )
        .expect("one bad entry must not poison the list");
        let kinds = holder
            .reactions
            .iter()
            .map(|e| e.kind.as_str())
            .collect::<Vec<_>>();
        assert_eq!(kinds, ["like", "sad"]);
    }

    #[test]
    fn emits_only_canonical_unicode_for_writes() {
        assert_eq!(graph_reaction_type(ReactionKind::Like), "👍");
        assert_eq!(graph_reaction_type(ReactionKind::Heart), "❤");
        assert_eq!(graph_reaction_type(ReactionKind::Laugh), "😄");
        assert_eq!(graph_reaction_type(ReactionKind::Surprised), "😮");
        assert_eq!(graph_reaction_type(ReactionKind::Sad), "😢");
        assert_eq!(graph_reaction_type(ReactionKind::Angry), "😡");
    }

    #[test]
    fn summaries_are_ordered_aggregated_across_aliases_and_hide_unknown_values() {
        let reactions = [
            ("custom", "current"),
            ("❤️", "other"),
            ("heart", "current"),
            ("💘", "current"),
            ("👍", "other"),
            ("👍", "current"),
            ("❤", "current"),
            ("😡", "other"),
        ];
        let summaries = normalize_reaction_summaries(
            &reactions,
            Some("current"),
            |reaction| Some(reaction.0),
            |reaction| Some(reaction.1),
        );
        assert_eq!(summaries.len(), 3);
        assert_eq!(summaries[0].kind(), ReactionKind::Like);
        assert_eq!(summaries[0].count(), 2);
        assert!(summaries[0].reacted_by_current_user());
        assert_eq!(summaries[1].kind(), ReactionKind::Heart);
        assert_eq!(summaries[1].count(), 3);
        assert!(summaries[1].reacted_by_current_user());
        assert_eq!(summaries[2].kind(), ReactionKind::Angry);
        assert_eq!(summaries[2].count(), 1);
        assert!(!summaries[2].reacted_by_current_user());
    }
}
