//! Centralized design tokens, type ramp, and one-time theme installation.
//!
//! Modelled on the Fluent 2 / Microsoft Teams light theme. Feature code should
//! use these tokens instead of raw colors or literal font sizes.

use eframe::egui::{
    self, Color32, Context, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke,
    TextStyle, Visuals,
};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::Arc;

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// Which of the two palettes is painting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Scheme {
    #[default]
    Light,
    Dark,
}

impl Scheme {
    const fn egui(self) -> egui::Theme {
        match self {
            Self::Light => egui::Theme::Light,
            Self::Dark => egui::Theme::Dark,
        }
    }
}

/// Declares every colour token once, with its light and dark value. Each token
/// becomes a field of [`Palette`], a value in [`LIGHT`] and [`DARK`], and a
/// same-named accessor that reads the active palette, so call sites write
/// `theme::fg1()`. A token without both values does not compile.
macro_rules! palette {
    ($($(#[$meta:meta])* $name:ident : $ty:ty = $light:expr, $dark:expr;)*) => {
        /// Every colour token for one colour scheme.
        #[derive(Clone, Copy)]
        pub(crate) struct Palette {
            $($(#[$meta])* $name: $ty,)*
        }

        const LIGHT: Palette = Palette { $($name: $light,)* };
        const DARK: Palette = Palette { $($name: $dark,)* };

        $(
            $(#[$meta])*
            pub(crate) fn $name() -> $ty {
                active().$name
            }
        )*
    };
}

thread_local! {
    /// The scheme colour accessors read. egui builds its UI on one thread, and
    /// tests run on many, so this is per thread: a dark test cannot leak into
    /// a light one. It is set once per frame by [`activate`].
    static ACTIVE: Cell<Scheme> = const { Cell::new(Scheme::Light) };
}

/// The palette for `scheme`.
const fn palette_for(scheme: Scheme) -> &'static Palette {
    match scheme {
        Scheme::Light => &LIGHT,
        Scheme::Dark => &DARK,
    }
}

fn active() -> &'static Palette {
    palette_for(ACTIVE.get())
}

/// Makes `scheme` the one colour accessors return on this thread.
pub(crate) fn set_scheme(scheme: Scheme) {
    ACTIVE.set(scheme);
}

/// The scheme colour accessors currently return.
#[cfg(test)]
pub(crate) fn scheme() -> Scheme {
    ACTIVE.get()
}

/// Resolves the user's choice for this frame (`System` follows the OS, and
/// reads as light when the OS does not say) and makes it current.
pub(crate) fn activate(ctx: &Context, preference: ThemePreference) -> Scheme {
    let wanted = preference.egui();
    if ctx.options(|options| options.theme_preference) != wanted {
        ctx.set_theme(wanted);
        // egui picks its style at the start of a pass, so repaint to apply it.
        ctx.request_repaint();
    }
    let resolved = match ctx.theme() {
        egui::Theme::Light => Scheme::Light,
        egui::Theme::Dark => Scheme::Dark,
    };
    set_scheme(resolved);
    resolved
}

palette! {
// Brand
/// Fill behind `fg_on_brand` text. The same in both schemes.
brand: Color32 = hex(0x5B5FC7), hex(0x5B5FC7);
/// Brand-coloured text, links, outlines and selected-icon tints on a surface.
/// The fill colour is too dark to read there in the dark scheme.
brand_fg: Color32 = hex(0x5B5FC7), hex(0xA6ABFF);
brand_tint: Color32 = hex(0xE8EBFA), hex(0x2F3157);
brand_tint_subtle: Color32 = hex(0xF5F4FC), hex(0x252644);
brand_stroke_subtle: Color32 = hex(0xD5CFF0), hex(0x3F4180);

// Backgrounds
canvas: Color32 = hex(0xFAFAFA), hex(0x1F1F1F);
surface: Color32 = hex(0xFFFFFF), hex(0x292929);
surface_subtle: Color32 = hex(0xF5F5F5), hex(0x333333);
surface_hover: Color32 = hex(0xF0F0F0), hex(0x3D3D3D);
sidebar_tint: Color32 = hex(0xE6F3F2), hex(0x1B2A29);
rail_tint: Color32 = hex(0xCCE6E4), hex(0x142423);
topbar_tint: Color32 = hex(0xD8E6DF), hex(0x18251F);

// Strokes
stroke: Color32 = hex(0xD1D1D1), hex(0x666666);
stroke_subtle: Color32 = hex(0xE0E0E0), hex(0x424242);

// Foregrounds
fg1: Color32 = hex(0x242424), hex(0xFFFFFF);
fg2: Color32 = hex(0x424242), hex(0xD6D6D6);
fg3: Color32 = hex(0x616161), hex(0xADADAD);
fg_disabled: Color32 = hex(0xBDBDBD), hex(0x5C5C5C);
fg_on_brand: Color32 = hex(0xFFFFFF), hex(0xFFFFFF);

// Chat bubbles
outgoing_bubble: Color32 = hex(0xE8EBF9), hex(0x373A68);
incoming_bubble: Color32 = hex(0xFFFFFF), hex(0x333333);

// Presence
presence_available: Color32 = hex(0x6BB700), hex(0x92C353);
presence_busy: Color32 = hex(0xC4314B), hex(0xE5707F);
presence_dnd: Color32 = hex(0xA4262C), hex(0xD7525F);
presence_away: Color32 = hex(0xFFAA44), hex(0xFFB86B);
presence_offline: Color32 = hex(0x8A8886), hex(0x9A9896);

// Status
danger: Color32 = hex(0xC4314B), hex(0xEF7C8B);

// Transient surfaces
shadow: Color32 = Color32::from_black_alpha(12), Color32::from_black_alpha(70);
toast_bg: Color32 = hex(0x242730), hex(0x3A3D4A);
toast_fg: Color32 = hex(0xFFFFFF), hex(0xFFFFFF);
/// Outline and action tints for toast severities, readable on `toast_bg`.
toast_error_accent: Color32 = hex(0xFF8A99), hex(0xFF8A99);
toast_success_accent: Color32 = hex(0x7FD1A0), hex(0x7FD1A0);
toast_action: Color32 = hex(0xA9B8FF), hex(0xA9B8FF);

/// Matched text in search results: a strong brand foreground on a tint.
search_hit: Color32 = hex(0x3F4296), hex(0xD6D9FF);
search_hit_bg: Color32 = hex(0xE8EBFA), hex(0x373A68);

/// Accent for conversations created on this device. The mid-tones read on
/// both schemes and carry white initials, so they are shared.
local_conversation_accent: Color32 = hex(0x7882C9), hex(0x7882C9);

/// Deterministic accents for Microsoft 365 conversations, picked by title.
remote_conversation_accents: [Color32; 5] = [
    hex(0x5D7BB3),
    hex(0xA36899),
    hex(0x468B84),
    hex(0xC1844E),
    hex(0x7079A5),
], [
    hex(0x5D7BB3),
    hex(0xA36899),
    hex(0x468B84),
    hex(0xC1844E),
    hex(0x7079A5),
];

/// Fluent persona avatar colors as (background, foreground) pairs. Light: a
/// pale tint with a dark same-hue foreground. Dark: a deep tint with a pale
/// same-hue foreground. At least 4.5:1 contrast in both.
avatar_palette: [(Color32, Color32); 8] = [
    (hex(0xDCE8F5), hex(0x0F548C)),
    (hex(0xDFF6DD), hex(0x0B6A0B)),
    (hex(0xFBE5E8), hex(0xA4262C)),
    (hex(0xFFE9D6), hex(0x8A3707)),
    (hex(0xEFE4F7), hex(0x5C2E91)),
    (hex(0xD7F0F0), hex(0x0B5A5A)),
    (hex(0xFBF0C4), hex(0x6B4E00)),
    (hex(0xE8EBFA), hex(0x3F4296)),
], [
    (hex(0x1B3A57), hex(0xA9D3F2)),
    (hex(0x1E3B1E), hex(0xA8DBA6)),
    (hex(0x4A1E24), hex(0xF4B1B7)),
    (hex(0x4A2B12), hex(0xF5C9A0)),
    (hex(0x33214A), hex(0xD4B8F0)),
    (hex(0x143B3B), hex(0x93D6D6)),
    (hex(0x4A3A0A), hex(0xEBD27A)),
    (hex(0x2B2E5C), hex(0xB5BAF5)),
];
}

/// Which colour scheme the user asked for. `System` follows the operating
/// system.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ThemePreference {
    Light,
    Dark,
    #[default]
    System,
}

impl ThemePreference {
    pub(crate) const ALL: [Self; 3] = [Self::Light, Self::Dark, Self::System];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Light => "Light",
            Self::Dark => "Dark",
            Self::System => "System",
        }
    }

    const fn egui(self) -> egui::ThemePreference {
        match self {
            Self::Light => egui::ThemePreference::Light,
            Self::Dark => egui::ThemePreference::Dark,
            Self::System => egui::ThemePreference::System,
        }
    }
}

/// Type ramp, in points.
pub(crate) mod font_size {
    pub(crate) const MICRO: f32 = 10.0;
    pub(crate) const SMALL: f32 = 11.0;
    pub(crate) const CAPTION: f32 = 12.0;
    pub(crate) const BODY: f32 = 14.0;
    pub(crate) const SUBTITLE: f32 = 16.0;
    pub(crate) const TITLE: f32 = 20.0;
    pub(crate) const DISPLAY: f32 = 32.0;
}

/// Spacing scale, in points.
pub(crate) mod space {
    pub(crate) const XXS: f32 = 2.0;
    pub(crate) const XS: f32 = 4.0;
    pub(crate) const SM: f32 = 6.0;
    pub(crate) const MD: f32 = 8.0;
    pub(crate) const LG: f32 = 12.0;
    pub(crate) const XL: f32 = 16.0;
    pub(crate) const XXL: f32 = 24.0;
}

/// Corner radii, in points.
pub(crate) mod radius {
    pub(crate) const SM: u8 = 4;
    pub(crate) const MD: u8 = 6;
    pub(crate) const LG: u8 = 8;
    pub(crate) const XL: u8 = 12;
}

const INTER_REGULAR: &str = "Inter";
const INTER_SEMIBOLD: &str = "Inter-SemiBold";

/// Inter Regular at `size` points (the primary proportional family).
pub(crate) fn regular(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(INTER_REGULAR.into()))
}

/// Inter SemiBold (600) at `size` points, for names, titles and selected rows.
pub(crate) fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(INTER_SEMIBOLD.into()))
}

/// The monospace family at `size` points, for inline and block code.
pub(crate) fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}

fn font_definitions() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        INTER_REGULAR.into(),
        Arc::new(FontData::from_static(include_bytes!(
            "../../assets/fonts/Inter-Regular.ttf"
        ))),
    );
    fonts.font_data.insert(
        INTER_SEMIBOLD.into(),
        Arc::new(FontData::from_static(include_bytes!(
            "../../assets/fonts/Inter-SemiBold.ttf"
        ))),
    );
    let fallbacks = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let proportional = fonts.families.entry(FontFamily::Proportional).or_default();
    proportional.insert(0, INTER_REGULAR.into());
    for (name, primary) in [
        (INTER_REGULAR, INTER_REGULAR),
        (INTER_SEMIBOLD, INTER_SEMIBOLD),
    ] {
        let mut chain = vec![primary.to_owned()];
        chain.extend(fallbacks.iter().cloned());
        fonts.families.insert(FontFamily::Name(name.into()), chain);
    }
    fonts
}

fn visuals(scheme: Scheme) -> Visuals {
    let p = palette_for(scheme);
    let mut v = match scheme {
        Scheme::Light => Visuals::light(),
        Scheme::Dark => Visuals::dark(),
    };
    v.panel_fill = p.canvas;
    v.window_fill = p.surface;
    v.extreme_bg_color = p.surface;
    v.faint_bg_color = p.surface_subtle;
    v.override_text_color = None;
    v.hyperlink_color = p.brand_fg;
    v.window_stroke = Stroke::new(1.0_f32, p.stroke_subtle);
    v.window_corner_radius = CornerRadius::same(radius::LG);
    v.menu_corner_radius = CornerRadius::same(radius::MD);
    v.selection.bg_fill = p.brand_tint;
    v.selection.stroke = Stroke::new(1.0_f32, p.brand_fg);

    let corner = CornerRadius::same(radius::SM);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.surface;
    w.noninteractive.weak_bg_fill = p.surface;
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.stroke_subtle);
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, p.fg1);
    w.inactive.bg_fill = p.surface_subtle;
    w.inactive.weak_bg_fill = p.surface_subtle;
    w.inactive.bg_stroke = Stroke::new(1.0_f32, p.stroke);
    w.inactive.fg_stroke = Stroke::new(1.0_f32, p.fg1);
    w.hovered.bg_fill = p.surface_hover;
    w.hovered.weak_bg_fill = p.surface_hover;
    w.hovered.bg_stroke = Stroke::new(1.0_f32, p.stroke);
    w.hovered.fg_stroke = Stroke::new(1.0_f32, p.fg1);
    w.active.bg_fill = p.brand_tint;
    w.active.weak_bg_fill = p.brand_tint;
    w.active.bg_stroke = Stroke::new(1.0_f32, p.brand_fg);
    w.active.fg_stroke = Stroke::new(1.0_f32, p.fg1);
    w.open.bg_fill = p.surface_hover;
    w.open.weak_bg_fill = p.surface_hover;
    w.open.bg_stroke = Stroke::new(1.0_f32, p.stroke);
    w.open.fg_stroke = Stroke::new(1.0_f32, p.fg1);
    for state in [
        &mut w.noninteractive,
        &mut w.inactive,
        &mut w.hovered,
        &mut w.active,
        &mut w.open,
    ] {
        state.corner_radius = corner;
    }
    v
}

/// Installs fonts, both schemes' visuals and the shared style. Call exactly
/// once at app creation; [`activate`] then picks the scheme each frame.
pub(crate) fn apply(ctx: &Context) {
    ctx.set_fonts(font_definitions());
    ctx.set_visuals_of(Scheme::Light.egui(), visuals(Scheme::Light));
    ctx.set_visuals_of(Scheme::Dark.egui(), visuals(Scheme::Dark));
    // An OS that does not report its appearance reads as light, as before.
    ctx.options_mut(|options| options.fallback_theme = egui::Theme::Light);
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 6.0);
        style.spacing.interact_size.y = 32.0;
        let text_styles: BTreeMap<TextStyle, FontId> = [
            (TextStyle::Small, FontId::proportional(font_size::SMALL)),
            (TextStyle::Body, FontId::proportional(font_size::BODY)),
            (TextStyle::Button, FontId::proportional(font_size::BODY)),
            (TextStyle::Heading, FontId::proportional(font_size::TITLE)),
            (TextStyle::Monospace, FontId::monospace(13.0)),
        ]
        .into();
        style.text_styles = text_styles;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brand_and_type_ramp_match_the_teams_reference() {
        assert_eq!(brand(), Color32::from_rgb(0x5B, 0x5F, 0xC7));
        assert_eq!(font_size::BODY, 14.0);
        assert_eq!(font_size::TITLE, 20.0);
    }

    /// CONTRIBUTING forbids raw colors and literal font sizes in feature code:
    /// every color and size must come from this module.
    #[test]
    fn feature_code_uses_theme_tokens_for_colors_and_font_sizes() {
        use std::path::Path;

        // `features/chat/storage_map.rs` decodes stored data, not a styling
        // choice.
        const EXEMPT: [&str; 1] = ["features/chat/storage_map.rs"];
        const FORBIDDEN: [&str; 11] = [
            "Color32::from_rgb",
            "Color32::from_rgba",
            "Color32::from_gray",
            "Color32::from_black_alpha",
            "Color32::from_white_alpha",
            "Color32::WHITE",
            "Color32::BLACK",
            "Color32::RED",
            "Color32::GRAY",
            "Color32::GREEN",
            "Color32::BLUE",
        ];

        fn visit(dir: &Path, root: &Path, offenders: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).expect("source directory must list") {
                let path = entry.expect("entry").path();
                let relative = path
                    .strip_prefix(root)
                    .expect("inside root")
                    .to_string_lossy()
                    .replace('\\', "/");
                if path.is_dir() {
                    if !matches!(relative.as_str(), "integrations" | "testing")
                        && path.file_name().is_none_or(|name| name != "tests")
                    {
                        visit(&path, root, offenders);
                    }
                    continue;
                }
                if path.extension().is_none_or(|extension| extension != "rs")
                    || relative == "ui/theme.rs"
                    || relative == "ui/testing.rs"
                    || relative.ends_with("/tests.rs")
                    || EXEMPT.contains(&relative.as_str())
                {
                    continue;
                }
                let source = std::fs::read_to_string(&path).expect("source must read");
                let production = source
                    .split("#[cfg(test)]\nmod ")
                    .next()
                    .unwrap_or_default();
                for (number, line) in production.lines().enumerate() {
                    let literal_size =
                        ["size(", "proportional(", "monospace("].iter().any(|call| {
                            line.match_indices(call).any(|(index, _)| {
                                line[index + call.len()..].starts_with(|c: char| c.is_ascii_digit())
                            })
                        });
                    if literal_size || FORBIDDEN.iter().any(|pattern| line.contains(pattern)) {
                        offenders.push(format!("{relative}:{}: {}", number + 1, line.trim()));
                    }
                }
            }
        }

        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        visit(&root, &root, &mut offenders);
        assert!(
            offenders.is_empty(),
            "raw colors or font sizes:\n{}",
            offenders.join("\n")
        );
    }

    fn luminance(c: Color32) -> f32 {
        let lin = |v: u8| {
            let v = f32::from(v) / 255.0;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
    }

    /// WCAG 2.x contrast ratio between two opaque colours.
    fn contrast(a: Color32, b: Color32) -> f32 {
        let (hi, lo) = (
            luminance(a).max(luminance(b)),
            luminance(a).min(luminance(b)),
        );
        (hi + 0.05) / (lo + 0.05)
    }

    const SCHEMES: [Scheme; 2] = [Scheme::Light, Scheme::Dark];

    /// Normal text: WCAG AA 4.5:1.
    const TEXT: f32 = 4.5;
    /// Large or bold glyphs and graphics: WCAG AA 3:1.
    const LARGE: f32 = 3.0;

    fn check(scheme: Scheme, name: &str, fg: Color32, bg: Color32, minimum: f32) {
        let ratio = contrast(fg, bg);
        assert!(
            ratio >= minimum,
            "{scheme:?}: {name} is {ratio:.2}:1, needs {minimum}:1"
        );
    }

    #[test]
    fn text_tokens_meet_aa_on_every_surface_in_both_schemes() {
        for scheme in SCHEMES {
            let p = palette_for(scheme);
            let surfaces = [
                ("canvas", p.canvas),
                ("surface", p.surface),
                ("surface_subtle", p.surface_subtle),
                ("surface_hover", p.surface_hover),
                ("sidebar_tint", p.sidebar_tint),
                ("rail_tint", p.rail_tint),
                ("topbar_tint", p.topbar_tint),
                ("incoming_bubble", p.incoming_bubble),
                ("outgoing_bubble", p.outgoing_bubble),
                ("brand_tint", p.brand_tint),
                ("brand_tint_subtle", p.brand_tint_subtle),
            ];
            for (fg_name, fg) in [("fg1", p.fg1), ("fg2", p.fg2), ("fg3", p.fg3)] {
                for (bg_name, bg) in surfaces {
                    check(scheme, &format!("{fg_name} on {bg_name}"), fg, bg, TEXT);
                }
            }
        }
    }

    #[test]
    fn accent_text_meets_aa_in_both_schemes() {
        for scheme in SCHEMES {
            let p = palette_for(scheme);
            check(scheme, "fg_on_brand on brand", p.fg_on_brand, p.brand, TEXT);
            check(scheme, "toast_fg on toast_bg", p.toast_fg, p.toast_bg, TEXT);
            for (name, fg) in [
                ("toast_error_accent", p.toast_error_accent),
                ("toast_success_accent", p.toast_success_accent),
                ("toast_action", p.toast_action),
            ] {
                check(scheme, &format!("{name} on toast_bg"), fg, p.toast_bg, TEXT);
            }
            check(
                scheme,
                "search_hit on search_hit_bg",
                p.search_hit,
                p.search_hit_bg,
                TEXT,
            );
            for (bg_name, bg) in [
                ("canvas", p.canvas),
                ("surface", p.surface),
                ("surface_subtle", p.surface_subtle),
                ("incoming_bubble", p.incoming_bubble),
            ] {
                check(
                    scheme,
                    &format!("brand_fg on {bg_name}"),
                    p.brand_fg,
                    bg,
                    TEXT,
                );
                check(scheme, &format!("danger on {bg_name}"), p.danger, bg, TEXT);
            }
        }
    }

    /// `brand_fg` is the brand colour used as text, so it stays readable on
    /// the tinted sidebar too (large text and icons at minimum).
    #[test]
    fn brand_fg_is_legible_on_tinted_chrome() {
        for scheme in SCHEMES {
            let p = palette_for(scheme);
            for (name, bg) in [
                ("sidebar_tint", p.sidebar_tint),
                ("rail_tint", p.rail_tint),
                ("topbar_tint", p.topbar_tint),
                ("brand_tint", p.brand_tint),
            ] {
                check(
                    scheme,
                    &format!("brand_fg on {name}"),
                    p.brand_fg,
                    bg,
                    LARGE,
                );
            }
        }
    }

    #[test]
    fn conversation_accents_carry_white_initials() {
        for scheme in SCHEMES {
            let p = palette_for(scheme);
            let accents = std::iter::once(p.local_conversation_accent)
                .chain(p.remote_conversation_accents)
                .chain([p.brand]);
            for accent in accents {
                check(
                    scheme,
                    "fg_on_brand on accent",
                    p.fg_on_brand,
                    accent,
                    LARGE,
                );
            }
        }
    }

    #[test]
    fn avatar_palette_foregrounds_meet_text_contrast_in_both_schemes() {
        for scheme in SCHEMES {
            for (bg, fg) in palette_for(scheme).avatar_palette {
                check(scheme, &format!("avatar {fg:?} on {bg:?}"), fg, bg, TEXT);
            }
        }
    }

    #[test]
    fn the_dark_palette_is_really_dark_and_distinct_from_light() {
        let (light, dark) = (palette_for(Scheme::Light), palette_for(Scheme::Dark));
        for (name, l, d) in [
            ("canvas", light.canvas, dark.canvas),
            ("surface", light.surface, dark.surface),
            ("sidebar_tint", light.sidebar_tint, dark.sidebar_tint),
            (
                "incoming_bubble",
                light.incoming_bubble,
                dark.incoming_bubble,
            ),
            ("fg1", light.fg1, dark.fg1),
            ("fg2", light.fg2, dark.fg2),
            ("fg3", light.fg3, dark.fg3),
            ("stroke_subtle", light.stroke_subtle, dark.stroke_subtle),
        ] {
            assert_ne!(l, d, "{name} is identical in both schemes");
        }
        for background in [dark.canvas, dark.surface, dark.surface_subtle] {
            assert!(luminance(background) < 0.05, "{background:?} is not dark");
        }
        assert!(luminance(dark.fg1) > luminance(dark.canvas));
    }

    /// Resets the thread's scheme so a dark test cannot leak into the next
    /// test that reuses the thread.
    struct SchemeGuard;

    impl Drop for SchemeGuard {
        fn drop(&mut self) {
            set_scheme(Scheme::Light);
        }
    }

    #[test]
    fn accessors_follow_the_active_scheme() {
        let _guard = SchemeGuard;
        assert_eq!(scheme(), Scheme::Light);
        assert_eq!(fg1(), LIGHT.fg1);
        set_scheme(Scheme::Dark);
        assert_eq!(fg1(), DARK.fg1);
        assert_eq!(avatar_palette(), DARK.avatar_palette);
        set_scheme(Scheme::Light);
        assert_eq!(canvas(), LIGHT.canvas);
    }

    #[test]
    fn brand_fg_equals_brand_in_light_so_light_is_unchanged() {
        assert_eq!(LIGHT.brand_fg, LIGHT.brand);
        assert_eq!(DARK.brand, LIGHT.brand);
    }

    #[test]
    fn visuals_follow_the_scheme() {
        let (light, dark) = (visuals(Scheme::Light), visuals(Scheme::Dark));
        assert!(!light.dark_mode);
        assert!(dark.dark_mode);
        assert_eq!(light.panel_fill, LIGHT.canvas);
        assert_eq!(dark.panel_fill, DARK.canvas);
        assert_eq!(dark.window_fill, DARK.surface);
        assert_eq!(dark.hyperlink_color, DARK.brand_fg);
        assert_eq!(dark.widgets.noninteractive.fg_stroke.color, DARK.fg1);
        assert_eq!(dark.widgets.inactive.bg_fill, DARK.surface_subtle);
    }

    #[test]
    fn apply_installs_both_schemes_into_egui() {
        let ctx = Context::default();
        apply(&ctx);
        ctx.set_theme(egui::ThemePreference::Dark);
        assert_eq!(ctx.style().visuals.panel_fill, DARK.canvas);
        ctx.set_theme(egui::ThemePreference::Light);
        assert_eq!(ctx.style().visuals.panel_fill, LIGHT.canvas);
    }

    fn system_ctx(system: Option<egui::Theme>) -> Context {
        let ctx = Context::default();
        apply(&ctx);
        let _ = ctx.run(
            egui::RawInput {
                system_theme: system,
                ..Default::default()
            },
            |_| {},
        );
        ctx
    }

    #[test]
    fn system_follows_the_os_and_falls_back_to_light_when_it_is_silent() {
        let _guard = SchemeGuard;
        let silent = system_ctx(None);
        assert_eq!(activate(&silent, ThemePreference::System), Scheme::Light);
        let dark_os = system_ctx(Some(egui::Theme::Dark));
        assert_eq!(activate(&dark_os, ThemePreference::System), Scheme::Dark);
        assert_eq!(scheme(), Scheme::Dark);
        let light_os = system_ctx(Some(egui::Theme::Light));
        assert_eq!(activate(&light_os, ThemePreference::System), Scheme::Light);
    }

    #[test]
    fn an_explicit_choice_overrides_the_os() {
        let _guard = SchemeGuard;
        let dark_os = system_ctx(Some(egui::Theme::Dark));
        assert_eq!(activate(&dark_os, ThemePreference::Light), Scheme::Light);
        let light_os = system_ctx(Some(egui::Theme::Light));
        assert_eq!(activate(&light_os, ThemePreference::Dark), Scheme::Dark);
        // Switching back follows the OS again.
        assert_eq!(activate(&light_os, ThemePreference::System), Scheme::Light);
    }

    #[test]
    fn apply_installs_inter_families_with_fallbacks() {
        let fonts = font_definitions();
        let proportional = &fonts.families[&FontFamily::Proportional];
        assert_eq!(proportional[0], "Inter");
        assert!(proportional.len() > 1, "egui defaults stay as fallback");
        let semibold = &fonts.families[&FontFamily::Name("Inter-SemiBold".into())];
        assert_eq!(semibold[0], "Inter-SemiBold");
        assert!(semibold.len() > 1);
        apply(&Context::default());
    }
}
