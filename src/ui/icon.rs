//! Fluent UI System Icons rendered through egui's cached SVG loader.
//!
//! The vendored SVGs are white (see `assets/icons/README.md`), so
//! `Image::tint` produces the exact requested color.
#![cfg_attr(
    not(test),
    expect(dead_code, reason = "consumed by the T4-T7 surfaces")
)]

use super::theme;
use eframe::egui::{
    self, Color32, CornerRadius, Image, ImageSource, Response, Sense, Ui, WidgetInfo, WidgetType,
};

/// Fixed icon pixel sizes. Keeping the set small bounds the number of
/// rasterized textures egui's loader caches per icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IconSize {
    S16,
    M20,
    L24,
}

impl IconSize {
    pub(crate) const fn px(self) -> f32 {
        match self {
            Self::S16 => 16.0,
            Self::M20 => 20.0,
            Self::L24 => 24.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Icon {
    Activity,
    Chat,
    Teams,
    Calendar,
    Call,
    Files,
    Apps,
    More,
    Search,
    ChevronLeft,
    ChevronRight,
    ChevronDown,
    PanelLeft,
    Video,
    PersonAdd,
    Emoji,
    Attach,
    Send,
    TextFormat,
    Sticker,
    Add,
    Mention,
    Filter,
    Compose,
    Drafts,
    Edit,
    ChevronUp,
    Repeat,
    CallInbound,
    CallOutbound,
    CallMissed,
    Dismiss,
    Mic,
    CallForward,
}

macro_rules! svg {
    ($name:literal) => {
        egui::include_image!(concat!("../../assets/icons/", $name, ".svg"))
    };
}

impl Icon {
    #[cfg(test)]
    const ALL: [Icon; 34] = [
        Self::Activity,
        Self::Chat,
        Self::Teams,
        Self::Calendar,
        Self::Call,
        Self::Files,
        Self::Apps,
        Self::More,
        Self::Search,
        Self::ChevronLeft,
        Self::ChevronRight,
        Self::ChevronDown,
        Self::PanelLeft,
        Self::Video,
        Self::PersonAdd,
        Self::Emoji,
        Self::Attach,
        Self::Send,
        Self::TextFormat,
        Self::Sticker,
        Self::Add,
        Self::Mention,
        Self::Filter,
        Self::Compose,
        Self::Drafts,
        Self::Edit,
        Self::ChevronUp,
        Self::Repeat,
        Self::CallInbound,
        Self::CallOutbound,
        Self::CallMissed,
        Self::Dismiss,
        Self::Mic,
        Self::CallForward,
    ];

    /// Whether a vendored filled variant exists.
    pub(crate) const fn has_filled(self) -> bool {
        matches!(
            self,
            Self::Activity | Self::Chat | Self::Teams | Self::Calendar | Self::Call | Self::Files
        )
    }

    pub(crate) fn regular_source(self) -> ImageSource<'static> {
        match self {
            Self::Activity => svg!("alert"),
            Self::Chat => svg!("chat"),
            Self::Teams => svg!("people_team"),
            Self::Calendar => svg!("calendar_ltr"),
            Self::Call => svg!("call"),
            Self::Files => svg!("cloud"),
            Self::Apps => svg!("apps"),
            Self::More => svg!("more_horizontal"),
            Self::Search => svg!("search"),
            Self::ChevronLeft => svg!("chevron_left"),
            Self::ChevronRight => svg!("chevron_right"),
            Self::ChevronDown => svg!("chevron_down"),
            Self::PanelLeft => svg!("panel_left"),
            Self::Video => svg!("video"),
            Self::PersonAdd => svg!("person_add"),
            Self::Emoji => svg!("emoji"),
            Self::Attach => svg!("attach"),
            Self::Send => svg!("send"),
            Self::TextFormat => svg!("text_font"),
            Self::Sticker => svg!("sticker"),
            Self::Add => svg!("add"),
            Self::Mention => svg!("mention"),
            Self::Filter => svg!("filter"),
            Self::Compose => svg!("compose"),
            Self::Drafts => svg!("drafts"),
            Self::Edit => svg!("edit"),
            Self::ChevronUp => svg!("chevron_up"),
            Self::Repeat => svg!("arrow_repeat_all"),
            Self::CallInbound => svg!("call_inbound"),
            Self::CallOutbound => svg!("call_outbound"),
            Self::CallMissed => svg!("call_missed"),
            Self::Dismiss => svg!("dismiss"),
            Self::Mic => svg!("mic"),
            Self::CallForward => svg!("call_forward"),
        }
    }

    /// Filled variant; icons without one fall back to the regular glyph.
    pub(crate) fn filled_source(self) -> ImageSource<'static> {
        match self {
            Self::Activity => svg!("alert_filled"),
            Self::Chat => svg!("chat_filled"),
            Self::Teams => svg!("people_team_filled"),
            Self::Calendar => svg!("calendar_ltr_filled"),
            Self::Call => svg!("call_filled"),
            Self::Files => svg!("cloud_filled"),
            other => other.regular_source(),
        }
    }

    /// Regular glyph tinted with the default icon foreground.
    pub(crate) fn image(self, size: IconSize) -> Image<'static> {
        Self::sized(self.regular_source(), size)
    }

    pub(crate) fn image_filled(self, size: IconSize) -> Image<'static> {
        Self::sized(self.filled_source(), size)
    }

    fn sized(source: ImageSource<'static>, size: IconSize) -> Image<'static> {
        Image::new(source)
            .fit_to_exact_size(egui::vec2(size.px(), size.px()))
            .tint(theme::fg2())
    }
}

const BUTTON_SIZE: f32 = 32.0;

/// A 32x32 icon button with a hover tooltip. Returns the click response.
pub(crate) fn icon_button(ui: &mut Ui, icon: Icon, tooltip: &str) -> Response {
    icon_button_selected(ui, icon, false, tooltip)
}

/// Like [`icon_button`]; a selected button uses the filled glyph and brand tint.
pub(crate) fn icon_button_selected(
    ui: &mut Ui,
    icon: Icon,
    selected: bool,
    tooltip: &str,
) -> Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(BUTTON_SIZE, BUTTON_SIZE), Sense::click());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, ui.is_enabled(), tooltip));
    if ui.is_rect_visible(rect) {
        if response.hovered() {
            ui.painter().rect_filled(
                rect,
                CornerRadius::same(theme::radius::SM),
                theme::surface_hover(),
            );
        }
        let tint: Color32 = if selected {
            theme::brand_fg()
        } else {
            theme::fg2()
        };
        let image = if selected {
            icon.image_filled(IconSize::M20)
        } else {
            icon.image(IconSize::M20)
        };
        let side = IconSize::M20.px();
        image.tint(tint).paint_at(
            ui,
            egui::Rect::from_center_size(rect.center(), egui::vec2(side, side)),
        );
    }
    response.on_hover_text(tooltip)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn icons_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/icons")
    }

    #[test]
    fn vendored_svgs_are_white_and_never_use_the_upstream_fill() {
        let mut count = 0;
        for entry in std::fs::read_dir(icons_dir()).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "svg") {
                let svg = std::fs::read_to_string(&path).unwrap();
                assert!(!svg.contains("#212121"), "{path:?} keeps the upstream fill");
                assert!(svg.contains("#FFFFFF"), "{path:?} is not white");
                count += 1;
            }
        }
        assert_eq!(count, 34 + 6);
    }

    #[test]
    fn every_icon_resolves_to_a_vendored_file() {
        for icon in Icon::ALL {
            for source in [icon.regular_source(), icon.filled_source()] {
                let ImageSource::Bytes { uri, bytes } = source else {
                    panic!("{icon:?} is not an embedded image");
                };
                assert!(uri.ends_with(".svg"), "{uri}");
                assert!(!bytes.as_ref().is_empty());
            }
        }
        assert_eq!(Icon::ALL.iter().filter(|i| i.has_filled()).count(), 6);
    }

    #[test]
    fn render_smoke() {
        let ctx = crate::ui::testing::themed_context();
        crate::ui::testing::run_ui(&ctx, |ui| {
            for icon in Icon::ALL {
                icon_button(ui, icon, "tip");
                icon_button_selected(ui, icon, true, "tip");
            }
            ui.add(Icon::Chat.image(IconSize::L24));
            ui.add(Icon::Chat.image_filled(IconSize::S16));
        });
    }
}
