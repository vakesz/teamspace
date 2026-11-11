use crate::ui::shortcuts::take_escape;
use crate::ui::theme;
use eframe::egui::{self, RichText, Stroke};

/// A local, inspectable representation of a shared link.
///
/// The source fixtures occasionally provide only a link label. In that case we
/// preserve the useful context without inventing a destination or launching a
/// browser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LinkPreview {
    label: String,
    destination: Option<String>,
}

impl LinkPreview {
    pub(crate) fn from_text(text: &str) -> Option<Self> {
        let destination = first_url(text)?.to_owned();
        Some(Self {
            label: destination.clone(),
            destination: Some(destination),
        })
    }

    pub(crate) fn resource(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            destination: None,
        }
    }
}

pub(crate) fn draw(ctx: &egui::Context, preview: &mut Option<LinkPreview>, source: &str) {
    let Some(selected) = preview.clone() else {
        return;
    };
    if take_escape(ctx) {
        *preview = None;
        return;
    }

    let mut open = true;
    let mut close_requested = false;
    egui::Window::new("Link details")
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(480.0)
        .show(ctx, |ui| {
            ui.label(RichText::new("Local link inspector").font(theme::semibold(theme::font_size::SUBTITLE)).color(theme::fg1()));
            ui.label(RichText::new(source).font(theme::regular(theme::font_size::CAPTION)).color(theme::fg3()));
            ui.add_space(14.0);
            egui::Frame::new()
                .fill(theme::surface())
                .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
                .corner_radius(8.0)
                .inner_margin(egui::Margin::symmetric(14, 12))
                .show(ui, |ui| {
                    ui.label(RichText::new("Shared resource").font(theme::regular(theme::font_size::CAPTION)).color(theme::fg3()));
                    ui.label(RichText::new(&selected.label).font(theme::regular(theme::font_size::BODY)).color(theme::fg1()));
                    if let Some(destination) = &selected.destination {
                        ui.add_space(10.0);
                        ui.label(RichText::new("Destination").font(theme::regular(theme::font_size::CAPTION)).color(theme::fg3()));
                        ui.label(RichText::new(destination).font(theme::regular(theme::font_size::CAPTION)).color(theme::brand_fg()));
                        ui.label(
                            RichText::new(format!("Host: {}", destination_host(destination)))
                                .font(theme::regular(theme::font_size::CAPTION))
                                .color(theme::fg3()),
                        );
                    } else {
                        ui.add_space(10.0);
                        ui.label(
                            RichText::new(
                                "The local channel data includes this resource name, but not its destination.",
                            )
                            .font(theme::regular(theme::font_size::CAPTION))
                            .color(theme::fg3()),
                        );
                    }
                });
            ui.add_space(14.0);
            ui.label(
                RichText::new("No browser is opened and no network request is made.")
                    .font(theme::regular(theme::font_size::CAPTION))
                    .color(theme::fg3()),
            );
            ui.add_space(10.0);
            if ui.button("Close").clicked() {
                close_requested = true;
            }
        });
    if close_requested || !open {
        *preview = None;
    }
}

fn first_url(text: &str) -> Option<&str> {
    text.split_whitespace().find_map(|part| {
        let start = part.find("https://").or_else(|| part.find("http://"))?;
        let url = &part[start..];
        Some(url.trim_end_matches(['.', ',', ')', ']', '}']))
    })
}

fn destination_host(destination: &str) -> &str {
    destination
        .split_once("://")
        .map_or(destination, |(_, remainder)| remainder)
        .split('/')
        .next()
        .filter(|host| !host.is_empty())
        .unwrap_or(destination)
}

#[cfg(test)]
mod tests {
    use super::{LinkPreview, destination_host, draw, first_url};
    use crate::testing::render_headlessly;
    use eframe::egui;

    #[test]
    fn extracts_a_url_without_trailing_sentence_punctuation() {
        assert_eq!(
            first_url("Read https://contoso.example/docs/42."),
            Some("https://contoso.example/docs/42")
        );
        assert_eq!(
            destination_host("https://contoso.example/docs/42"),
            "contoso.example"
        );
    }

    #[test]
    fn distinguishes_a_full_url_from_a_title_only_resource() {
        let linked = LinkPreview::from_text("https://contoso.example/docs").expect("URL");
        assert_eq!(
            linked.destination.as_deref(),
            Some("https://contoso.example/docs")
        );

        let titled = LinkPreview::resource("Architecture assessment");
        assert_eq!(titled.destination, None);
    }

    #[test]
    fn inspector_renders_without_a_native_window() {
        let mut preview = Some(LinkPreview::resource("Architecture assessment"));

        render_headlessly(|ui| draw(ui.ctx(), &mut preview, "Shared in this channel"));

        assert!(preview.is_some());
    }

    #[test]
    fn escape_closes_the_inspector() {
        let ctx = egui::Context::default();
        let mut preview = Some(LinkPreview::resource("Architecture assessment"));
        ctx.begin_pass(egui::RawInput {
            events: vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            }],
            ..Default::default()
        });

        draw(&ctx, &mut preview, "Shared in this channel");
        let _ = ctx.end_pass();

        assert!(preview.is_none());
    }
}
