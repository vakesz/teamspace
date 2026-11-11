use super::icon::{Icon, icon_button};
use super::theme;
use eframe::egui::{self, Color32, RichText, Stroke};

/// A captured reaction count used to seed a session-local reaction set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReactionSeed {
    pub(crate) icon: &'static str,
    pub(crate) count: usize,
}

/// Session-local reaction state for a single post or reply.
///
/// The set starts from captured reference counts. Toggling changes only the
/// current user's contribution, so the source counts remain intact and every
/// surface can own its state without sharing it with another thread.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReactionSet {
    reactions: Vec<Reaction>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Reaction {
    icon: String,
    count: usize,
    reacted_by_current_user: bool,
}

impl ReactionSet {
    pub(crate) fn from_seed(seed: &[ReactionSeed]) -> Self {
        Self {
            reactions: seed
                .iter()
                .filter(|reaction| reaction.count > 0)
                .map(|reaction| Reaction {
                    icon: reaction.icon.into(),
                    count: reaction.count,
                    reacted_by_current_user: false,
                })
                .collect(),
        }
    }

    /// Rebuilds a reaction set from immutable reference counts plus the
    /// current user's durable selections. Counts remain derived from the
    /// current reference data instead of becoming stale snapshot data.
    pub(crate) fn from_seed_with_selected(
        seed: &[ReactionSeed],
        selected_icons: &[String],
    ) -> Self {
        let mut reactions = Self::from_seed(seed);
        for icon in selected_icons {
            if icon.trim().is_empty()
                || reactions
                    .reactions
                    .iter()
                    .any(|reaction| reaction.icon == *icon && reaction.reacted_by_current_user)
            {
                continue;
            }
            let _ = reactions.toggle(icon);
        }
        reactions
    }

    /// Returns only the current user's selections. Feature persistence uses
    /// this projection rather than storing reference-owned reaction counts.
    pub(crate) fn selected_icons(&self) -> Vec<String> {
        self.reactions
            .iter()
            .filter(|reaction| reaction.reacted_by_current_user)
            .map(|reaction| reaction.icon.clone())
            .collect()
    }

    /// Toggles the current user's contribution and returns whether it is now selected.
    pub(crate) fn toggle(&mut self, icon: &str) -> bool {
        if let Some(index) = self
            .reactions
            .iter()
            .position(|reaction| reaction.icon == icon)
        {
            let reaction = &mut self.reactions[index];
            if reaction.reacted_by_current_user {
                reaction.reacted_by_current_user = false;
                reaction.count = reaction.count.saturating_sub(1);
                if reaction.count == 0 {
                    self.reactions.remove(index);
                }
                return false;
            }

            reaction.reacted_by_current_user = true;
            reaction.count += 1;
            return true;
        }

        self.reactions.push(Reaction {
            icon: icon.into(),
            count: 1,
            reacted_by_current_user: true,
        });
        true
    }

    #[cfg(test)]
    pub(crate) fn count_for(&self, icon: &str) -> Option<usize> {
        self.reactions
            .iter()
            .find(|reaction| reaction.icon == icon)
            .map(|reaction| reaction.count)
    }

    #[cfg(test)]
    pub(crate) fn current_user_reacted(&self, icon: &str) -> bool {
        self.reactions
            .iter()
            .find(|reaction| reaction.icon == icon)
            .is_some_and(|reaction| reaction.reacted_by_current_user)
    }
}

/// Draws compact reaction pills and returns the icon whose local state changed.
pub(crate) fn draw_reaction_bar(ui: &mut egui::Ui, reactions: &ReactionSet) -> Option<String> {
    let mut selected_action = None;
    ui.horizontal_wrapped(|ui| {
        for reaction in &reactions.reactions {
            let selected = reaction.reacted_by_current_user;
            let response = ui.add(
                egui::Button::new(
                    RichText::new(format!("{} {}", reaction.icon, reaction.count))
                        .size(theme::font_size::CAPTION)
                        .color(if selected {
                            theme::brand_fg()
                        } else {
                            theme::fg1()
                        }),
                )
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
                .corner_radius(9.0),
            );
            if response
                .on_hover_text(if selected {
                    "Remove your reaction"
                } else {
                    "Add this reaction"
                })
                .clicked()
            {
                selected_action = Some(reaction.icon.clone());
            }
        }
        let add = icon_button(ui, Icon::Emoji, "Add reaction");
        egui::Popup::menu(&add).show(|ui| {
            for (icon, label) in [
                ("👍", "like"),
                ("❤", "heart"),
                ("😄", "laugh"),
                ("😮", "surprised"),
            ] {
                if ui.button(format!("{icon}  {label}")).clicked() {
                    selected_action = Some(icon.into());
                    ui.close();
                }
            }
        });
    });
    selected_action
}

#[cfg(test)]
mod tests {
    use super::{ReactionSeed, ReactionSet};

    #[test]
    fn toggling_preserves_captured_counts_and_removes_only_local_reactions() {
        let mut reactions = ReactionSet::from_seed(&[ReactionSeed {
            icon: "👍",
            count: 7,
        }]);

        assert!(reactions.toggle("👍"));
        assert_eq!(reactions.count_for("👍"), Some(8));
        assert!(reactions.current_user_reacted("👍"));

        assert!(!reactions.toggle("👍"));
        assert_eq!(reactions.count_for("👍"), Some(7));
        assert!(!reactions.current_user_reacted("👍"));

        assert!(reactions.toggle("❤"));
        assert_eq!(reactions.count_for("❤"), Some(1));
        assert!(!reactions.toggle("❤"));
        assert_eq!(reactions.count_for("❤"), None);
    }

    #[test]
    fn durable_selections_rebuild_against_current_reference_counts() {
        let selections = vec!["👍".into(), "❤".into()];
        let reactions = ReactionSet::from_seed_with_selected(
            &[ReactionSeed {
                icon: "👍",
                count: 7,
            }],
            &selections,
        );

        assert_eq!(reactions.count_for("👍"), Some(8));
        assert_eq!(reactions.count_for("❤"), Some(1));
        assert_eq!(reactions.selected_icons(), selections);
    }
}
