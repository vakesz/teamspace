//! Round avatars with an initials fallback and an optional presence badge.
#![cfg_attr(
    not(test),
    expect(dead_code, reason = "consumed by the T4-T7 surfaces")
)]

use super::theme;
use eframe::egui::{self, Color32, CornerRadius, Image, ImageSource, Rect, Response, Sense, Ui};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AvatarSize {
    Xs20,
    Sm24,
    Md32,
    Lg40,
}

impl AvatarSize {
    pub(crate) const fn px(self) -> f32 {
        match self {
            Self::Xs20 => 20.0,
            Self::Sm24 => 24.0,
            Self::Md32 => 32.0,
            Self::Lg40 => 40.0,
        }
    }
}

/// Silhouette of an [`Avatar`]: people are circles, teams are rounded squares.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum AvatarShape {
    #[default]
    Circle,
    RoundedSquare,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Presence {
    Available,
    Busy,
    DoNotDisturb,
    Away,
    Offline,
}

impl Presence {
    pub(crate) fn color(self) -> Color32 {
        match self {
            Self::Available => theme::presence_available(),
            Self::Busy => theme::presence_busy(),
            Self::DoNotDisturb => theme::presence_dnd(),
            Self::Away => theme::presence_away(),
            Self::Offline => theme::presence_offline(),
        }
    }
}

/// Up to two uppercase initials held inline, so painting never allocates.
struct Initials {
    buf: [u8; 16],
    len: usize,
}

impl Initials {
    fn new(name: &str) -> Self {
        let mut out = Self {
            buf: [0; 16],
            len: 0,
        };
        let mut count = 0;
        'words: for word in name.split_whitespace() {
            let Some(first) = word.chars().next() else {
                continue;
            };
            for upper in first.to_uppercase() {
                if count == 2 {
                    break 'words;
                }
                let written = upper.encode_utf8(&mut out.buf[out.len..]).len();
                out.len += written;
                count += 1;
            }
            if count == 2 {
                break;
            }
        }
        out
    }

    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

/// Up to two uppercase initials from the first two words of `name`, or `"?"`.
pub(crate) fn initials(name: &str) -> String {
    initials_with_fallback(name, "?")
}

/// Like [`initials`] with a caller-chosen fallback for names without letters.
pub(crate) fn initials_with_fallback(name: &str, fallback: &str) -> String {
    let initials = Initials::new(name);
    if initials.as_str().is_empty() {
        fallback.to_owned()
    } else {
        initials.as_str().to_owned()
    }
}

/// Deterministic (background, foreground) pair for `seed` (FNV-1a over a fixed palette).
pub(crate) fn avatar_color(seed: &str) -> (Color32, Color32) {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in seed.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    theme::avatar_palette()[hash as usize % theme::avatar_palette().len()]
}

pub(crate) struct Avatar<'a> {
    name: &'a str,
    size: AvatarSize,
    image: Option<ImageSource<'a>>,
    presence: Option<Presence>,
    shape: AvatarShape,
}

impl<'a> Avatar<'a> {
    pub(crate) fn new(name: &'a str, size: AvatarSize) -> Self {
        Self {
            name,
            size,
            image: None,
            presence: None,
            shape: AvatarShape::Circle,
        }
    }

    /// Paints a `radius::SM` rounded square instead of a circle (team avatars).
    pub(crate) fn rounded_square(mut self) -> Self {
        self.shape = AvatarShape::RoundedSquare;
        self
    }

    pub(crate) fn image(mut self, source: ImageSource<'a>) -> Self {
        self.image = Some(source);
        self
    }

    pub(crate) fn presence(mut self, presence: Presence) -> Self {
        self.presence = Some(presence);
        self
    }

    pub(crate) fn show(self, ui: &mut Ui) -> Response {
        let side = self.size.px();
        let (rect, response) = ui.allocate_exact_size(egui::vec2(side, side), Sense::hover());
        if ui.is_rect_visible(rect) {
            self.paint(ui, rect);
        }
        response
    }

    /// Paints into `rect` (assumed square, sized for `self.size`).
    pub(crate) fn paint(self, ui: &Ui, rect: Rect) {
        let side = self.size.px();
        let painter = ui.painter();
        let corner = match self.shape {
            AvatarShape::Circle => CornerRadius::same((side / 2.0) as u8),
            AvatarShape::RoundedSquare => CornerRadius::same(theme::radius::SM),
        };
        if let Some(source) = self.image {
            Image::new(source)
                .fit_to_exact_size(rect.size())
                .corner_radius(corner)
                .paint_at(ui, rect);
        } else {
            let (bg, fg) = avatar_color(self.name);
            match self.shape {
                AvatarShape::Circle => {
                    painter.circle_filled(rect.center(), side / 2.0, bg);
                }
                AvatarShape::RoundedSquare => {
                    painter.rect_filled(rect, corner, bg);
                }
            }
            let initials = Initials::new(self.name);
            let text = if initials.as_str().is_empty() {
                "?"
            } else {
                initials.as_str()
            };
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                text,
                theme::semibold(side * 0.4),
                fg,
            );
        }
        if let Some(presence) = self.presence {
            let dot = (side * 0.3).max(6.0);
            let center = rect.right_bottom() - egui::vec2(dot / 2.0, dot / 2.0);
            painter.circle_filled(center, dot / 2.0 + 2.0, theme::surface());
            painter.circle_filled(center, dot / 2.0, presence.color());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_take_the_first_two_words_uppercased() {
        assert_eq!(initials("Wren Castellanos"), "WC");
        assert_eq!(initials("ada lovelace byron"), "AL");
        assert_eq!(initials("You"), "Y");
        assert_eq!(initials("  élan  vital "), "ÉV");
    }

    #[test]
    fn initials_fall_back_when_the_name_is_blank() {
        assert_eq!(initials(""), "?");
        assert_eq!(initials("   "), "?");
        assert_eq!(initials_with_fallback("", "T"), "T");
        assert_eq!(initials_with_fallback("", ""), "");
    }

    #[test]
    fn multi_char_uppercase_is_capped_at_two_chars() {
        assert_eq!(initials("ß"), "SS");
        assert_eq!(initials("ß x"), "SS");
    }

    #[test]
    fn avatar_color_is_deterministic_and_in_the_palette() {
        assert_eq!(avatar_color("Ada Lovelace"), avatar_color("Ada Lovelace"));
        for name in ["", "A", "Ada", "Grace Hopper", "Linus"] {
            assert!(theme::avatar_palette().contains(&avatar_color(name)));
        }
        // Pinned golden values: a hasher change would recolor every avatar.
        assert_eq!(
            avatar_color(""),
            theme::avatar_palette()[(0x811c_9dc5_u32 % 8) as usize]
        );
        assert_eq!(
            avatar_color("a"),
            theme::avatar_palette()[0xe40c_292c_u32 as usize % 8]
        );
    }

    #[test]
    fn presence_colors_map_to_tokens() {
        assert_eq!(Presence::Busy.color(), theme::presence_busy());
        assert_eq!(Presence::DoNotDisturb.color(), theme::presence_dnd());
        assert_eq!(Presence::Offline.color(), theme::presence_offline());
    }

    #[test]
    fn render_smoke() {
        let ctx = crate::ui::testing::themed_context();
        crate::ui::testing::run_ui(&ctx, |ui| {
            for size in [
                AvatarSize::Xs20,
                AvatarSize::Sm24,
                AvatarSize::Md32,
                AvatarSize::Lg40,
            ] {
                for presence in [None, Some(Presence::Available), Some(Presence::Away)] {
                    let mut avatar = Avatar::new("Ada Lovelace", size);
                    if let Some(presence) = presence {
                        avatar = avatar.presence(presence);
                    }
                    avatar.show(ui);
                }
                Avatar::new("", size).show(ui);
                Avatar::new("Ada", size)
                    .image(egui::include_image!("../../assets/icons/chat.svg"))
                    .show(ui);
            }
        });
    }
}
