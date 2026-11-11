use super::theme;
use eframe::egui::{self, RichText};
use std::collections::VecDeque;
use std::time::Duration;

const DISMISS_LABEL: &str = "Dismiss notification";
const UNDO_LABEL: &str = "Undo";

/// Most toasts visible at once. A new one beyond this evicts the oldest.
const MAX_TOASTS: usize = 4;
const INFO_DURATION_SECS: f64 = 4.0;
const ERROR_DURATION_SECS: f64 = 8.0;
const UNDO_DURATION_SECS: f64 = 6.0;
const TOAST_MAX_WIDTH: f32 = 360.0;

/// How a toast is styled and how long it stays.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Severity {
    #[default]
    Info,
    Success,
    Error,
}

impl Severity {
    const fn duration(self) -> f64 {
        match self {
            Self::Error => ERROR_DURATION_SECS,
            Self::Info | Self::Success => INFO_DURATION_SECS,
        }
    }

    const fn prefix(self) -> &'static str {
        match self {
            Self::Info => "",
            Self::Success => "Success: ",
            Self::Error => "Error: ",
        }
    }
}

/// The local delete an "Undo" toast can reverse. The owning feature keeps the
/// removed record; the toast only says which feature to ask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UndoKind {
    ChatMessage,
    ChannelPost,
    CallHistory,
    SpeedDial,
}

struct Toast {
    id: u64,
    severity: Severity,
    message: String,
    /// Seconds left; frozen while the toast is hovered or focused.
    remaining: f64,
    undo: Option<UndoKind>,
}

/// Short-lived, shell-level feedback for local preview actions.
///
/// Surface modules only request a message through `show*`; queueing, timing,
/// pausing, dismissal, and rendering stay here so feedback is not duplicated
/// inside page content.
#[derive(Default)]
pub(crate) struct ToastState {
    toasts: VecDeque<Toast>,
    next_id: u64,
    last_tick: Option<f64>,
    stack_rect: Option<egui::Rect>,
    focus_within: bool,
    triggered_undo: Option<UndoKind>,
}

impl ToastState {
    /// An informational toast (the default severity).
    pub(crate) fn show(&mut self, message: impl Into<String>) {
        self.push(Severity::Info, message.into(), None);
    }

    pub(crate) fn show_success(&mut self, message: impl Into<String>) {
        self.push(Severity::Success, message.into(), None);
    }

    pub(crate) fn show_error(&mut self, message: impl Into<String>) {
        self.push(Severity::Error, message.into(), None);
    }

    /// An informational toast with an "Undo" button. A newer toast of the same
    /// kind replaces the older one, so Undo always reverses the latest delete.
    pub(crate) fn show_undo(&mut self, message: impl Into<String>, kind: UndoKind) {
        self.push(Severity::Info, message.into(), Some(kind));
    }

    /// The text of the toasts now showing, oldest first.
    #[cfg(test)]
    pub(crate) fn messages(&self) -> Vec<String> {
        self.toasts
            .iter()
            .map(|toast| toast.message.clone())
            .collect()
    }

    pub(crate) fn clear(&mut self) {
        self.toasts.clear();
        self.last_tick = None;
        self.stack_rect = None;
        self.focus_within = false;
    }

    /// The undo the user requested since the last call, if any.
    pub(crate) fn take_undo(&mut self) -> Option<UndoKind> {
        self.triggered_undo.take()
    }

    fn push(&mut self, severity: Severity, message: String, undo: Option<UndoKind>) {
        let duration = if undo.is_some() {
            UNDO_DURATION_SECS
        } else {
            severity.duration()
        };
        self.toasts.retain(|toast| {
            let same_undo = undo.is_some() && toast.undo == undo;
            let same_text = undo.is_none()
                && toast.undo.is_none()
                && toast.severity == severity
                && toast.message == message;
            !(same_undo || same_text)
        });
        self.next_id += 1;
        self.toasts.push_back(Toast {
            id: self.next_id,
            severity,
            message,
            remaining: duration,
            undo,
        });
        while self.toasts.len() > MAX_TOASTS {
            // Keep an Undo reachable for as long as possible.
            let victim = self
                .toasts
                .iter()
                .position(|toast| toast.undo.is_none())
                .unwrap_or(0);
            self.toasts.remove(victim);
        }
    }

    pub(crate) fn draw(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|input| input.time);
        if self.toasts.is_empty() {
            self.last_tick = None;
            self.stack_rect = None;
            self.focus_within = false;
            return;
        }
        let elapsed = self.last_tick.map_or(0.0, |last| (now - last).max(0.0));
        self.last_tick = Some(now);

        let hovered = self
            .stack_rect
            .zip(ctx.pointer_hover_pos())
            .is_some_and(|(rect, pos)| rect.contains(pos));
        let paused = hovered || self.focus_within;
        if !paused {
            for toast in &mut self.toasts {
                toast.remaining -= elapsed;
            }
            self.toasts.retain(|toast| toast.remaining > 0.0);
            if self.toasts.is_empty() {
                self.stack_rect = None;
                self.focus_within = false;
                return;
            }
            let soonest = self
                .toasts
                .iter()
                .map(|toast| toast.remaining)
                .fold(f64::INFINITY, f64::min);
            ctx.request_repaint_after(Duration::from_secs_f64(soonest));
        }

        let mut dismiss: Option<u64> = None;
        let mut undo: Option<(u64, UndoKind)> = None;
        let mut focus_within = false;
        let area = egui::Area::new(egui::Id::new("app_toast"))
            .order(egui::Order::Tooltip)
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-20.0, -20.0))
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = theme::space::SM;
                for toast in &self.toasts {
                    let accent = match toast.severity {
                        Severity::Info => None,
                        Severity::Success => Some(theme::toast_success_accent()),
                        Severity::Error => Some(theme::toast_error_accent()),
                    };
                    egui::Frame::new()
                        .fill(theme::toast_bg())
                        .stroke(match accent {
                            Some(color) => egui::Stroke::new(1.0_f32, color),
                            None => egui::Stroke::NONE,
                        })
                        .corner_radius(8.0)
                        .inner_margin(egui::Margin::symmetric(12, 9))
                        .show(ui, |ui| {
                            ui.push_id(toast.id, |ui| {
                                ui.set_max_width(TOAST_MAX_WIDTH);
                                ui.horizontal(|ui| {
                                    let text = RichText::new(&toast.message)
                                        .font(theme::regular(theme::font_size::CAPTION))
                                        .color(theme::toast_fg());
                                    let label = ui.label(text);
                                    let spoken =
                                        format!("{}{}", toast.severity.prefix(), toast.message);
                                    label.widget_info(|| {
                                        egui::WidgetInfo::labeled(
                                            egui::WidgetType::Label,
                                            true,
                                            &spoken,
                                        )
                                    });
                                    // A polite live region, so a screen reader
                                    // announces the toast without moving focus.
                                    // Errors interrupt, as they need attention.
                                    let live = if toast.severity == Severity::Error {
                                        egui::accesskit::Live::Assertive
                                    } else {
                                        egui::accesskit::Live::Polite
                                    };
                                    ui.ctx().accesskit_node_builder(label.id, |node| {
                                        node.set_live(live);
                                    });
                                    if let Some(kind) = toast.undo {
                                        let button = ui.button(
                                            RichText::new(UNDO_LABEL).color(theme::toast_action()),
                                        );
                                        button.widget_info(|| {
                                            egui::WidgetInfo::labeled(
                                                egui::WidgetType::Button,
                                                true,
                                                UNDO_LABEL,
                                            )
                                        });
                                        focus_within |= button.has_focus();
                                        if button.clicked() {
                                            undo = Some((toast.id, kind));
                                        }
                                    }
                                    let close =
                                        ui.button(RichText::new("×").color(theme::toast_fg()));
                                    close.widget_info(|| {
                                        egui::WidgetInfo::labeled(
                                            egui::WidgetType::Button,
                                            true,
                                            DISMISS_LABEL,
                                        )
                                    });
                                    focus_within |= close.has_focus();
                                    if close.on_hover_text(DISMISS_LABEL).clicked() {
                                        dismiss = Some(toast.id);
                                    }
                                });
                            });
                        });
                }
            });
        self.stack_rect = Some(area.response.rect);
        self.focus_within = focus_within;
        if let Some((id, kind)) = undo {
            self.toasts.retain(|toast| toast.id != id);
            self.triggered_undo = Some(kind);
        } else if let Some(id) = dismiss {
            self.toasts.retain(|toast| toast.id != id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Severity, ToastState, UndoKind};
    use eframe::egui;

    fn render_at(toast: &mut ToastState, ctx: &egui::Context, time: f64) {
        ctx.begin_pass(egui::RawInput {
            time: Some(time),
            ..Default::default()
        });
        toast.draw(ctx);
        let _ = ctx.end_pass();
    }

    fn render_hovering(toast: &mut ToastState, ctx: &egui::Context, time: f64) {
        let pos = toast.stack_rect.expect("drawn").center();
        ctx.begin_pass(egui::RawInput {
            time: Some(time),
            events: vec![egui::Event::PointerMoved(pos)],
            ..Default::default()
        });
        toast.draw(ctx);
        let _ = ctx.end_pass();
    }

    fn messages(toast: &ToastState) -> Vec<&str> {
        toast.toasts.iter().map(|t| t.message.as_str()).collect()
    }

    #[test]
    fn a_toast_expires_after_the_info_duration() {
        let ctx = crate::ui::testing::themed_context();
        let mut toast = ToastState::default();
        toast.show("First action");
        render_at(&mut toast, &ctx, 10.0);
        render_at(&mut toast, &ctx, 13.9);
        assert_eq!(messages(&toast), ["First action"]);
        render_at(&mut toast, &ctx, 14.0);
        assert!(toast.toasts.is_empty());
    }

    #[test]
    fn messages_queue_instead_of_replacing_each_other() {
        let mut toast = ToastState::default();
        toast.show_error("Transport notice");
        toast.show("New Microsoft 365 message");
        assert_eq!(
            messages(&toast),
            ["Transport notice", "New Microsoft 365 message"]
        );
    }

    #[test]
    fn the_queue_is_bounded_and_evicts_the_oldest() {
        let mut toast = ToastState::default();
        for n in 1..=6 {
            toast.show(format!("m{n}"));
        }
        assert_eq!(messages(&toast), ["m3", "m4", "m5", "m6"]);
    }

    #[test]
    fn eviction_spares_an_undo_toast() {
        let mut toast = ToastState::default();
        toast.show_undo("Deleted", UndoKind::SpeedDial);
        for n in 1..=4 {
            toast.show(format!("m{n}"));
        }
        assert_eq!(messages(&toast), ["Deleted", "m2", "m3", "m4"]);
    }

    #[test]
    fn an_identical_message_refreshes_instead_of_stacking() {
        let mut toast = ToastState::default();
        toast.show("Saved");
        toast.show("Other");
        toast.toasts[0].remaining = 1.0;
        toast.show("Saved");
        assert_eq!(messages(&toast), ["Other", "Saved"]);
        assert_eq!(toast.toasts[1].remaining, 4.0);
    }

    #[test]
    fn errors_stay_longer_than_info() {
        let ctx = crate::ui::testing::themed_context();
        let mut toast = ToastState::default();
        toast.show("Info");
        toast.show_error("Boom");
        render_at(&mut toast, &ctx, 0.0);
        render_at(&mut toast, &ctx, 4.5);
        assert_eq!(messages(&toast), ["Boom"]);
        assert_eq!(toast.toasts[0].severity, Severity::Error);
        render_at(&mut toast, &ctx, 8.0);
        assert!(toast.toasts.is_empty());
    }

    #[test]
    fn hovering_pauses_the_countdown() {
        let ctx = crate::ui::testing::themed_context();
        let mut toast = ToastState::default();
        toast.show("Hold me");
        render_at(&mut toast, &ctx, 0.0);
        render_at(&mut toast, &ctx, 1.0);
        // The pointer enters, then rests for far longer than the duration.
        render_hovering(&mut toast, &ctx, 2.0);
        render_hovering(&mut toast, &ctx, 30.0);
        assert_eq!(messages(&toast), ["Hold me"]);
        assert!(toast.toasts[0].remaining > 1.9);
    }

    #[test]
    fn an_undo_toast_lasts_six_seconds_and_replaces_its_predecessor() {
        let ctx = crate::ui::testing::themed_context();
        let mut toast = ToastState::default();
        toast.show_undo("Removed A", UndoKind::CallHistory);
        toast.show_undo("Removed B", UndoKind::CallHistory);
        toast.show_undo("Removed C", UndoKind::SpeedDial);
        assert_eq!(messages(&toast), ["Removed B", "Removed C"]);
        render_at(&mut toast, &ctx, 0.0);
        render_at(&mut toast, &ctx, 5.9);
        assert_eq!(toast.toasts.len(), 2);
        render_at(&mut toast, &ctx, 6.0);
        assert!(toast.toasts.is_empty());
    }

    fn harness_for(toast: ToastState) -> egui_kittest::Harness<'static, ToastState> {
        let mut installed = false;
        egui_kittest::Harness::new_state(
            move |ctx, toast: &mut ToastState| {
                if !installed {
                    crate::ui::theme::apply(ctx);
                    installed = true;
                    ctx.request_discard("install theme fonts");
                    return;
                }
                toast.draw(ctx);
            },
            toast,
        )
    }

    #[test]
    fn close_button_has_an_accessible_dismiss_label() {
        use egui_kittest::kittest::Queryable;
        let mut toast = ToastState::default();
        toast.show("Saved");
        let mut harness = harness_for(toast);
        harness.run();
        harness.get_by_label("Dismiss notification");
    }

    #[test]
    fn dismiss_removes_only_that_toast() {
        use egui_kittest::kittest::Queryable;
        let mut toast = ToastState::default();
        toast.show("First");
        toast.show("Second");
        let mut harness = harness_for(toast);
        harness.run();
        harness
            .get_all_by_label("Dismiss notification")
            .next()
            .unwrap()
            .click();
        harness.run();
        assert_eq!(messages(harness.state()), ["Second"]);
    }

    #[test]
    fn severity_is_part_of_the_spoken_label() {
        use egui_kittest::kittest::Queryable;
        let mut toast = ToastState::default();
        toast.show_error("Could not save");
        let mut harness = harness_for(toast);
        harness.run();
        harness.get_by_label("Error: Could not save");
    }

    #[test]
    fn undo_button_reports_its_kind_once() {
        use egui_kittest::kittest::Queryable;
        let mut toast = ToastState::default();
        toast.show_undo("Removed Ada", UndoKind::SpeedDial);
        let mut harness = harness_for(toast);
        harness.run();
        harness.get_by_label("Undo").click();
        harness.run();
        assert_eq!(harness.state_mut().take_undo(), Some(UndoKind::SpeedDial));
        assert_eq!(harness.state_mut().take_undo(), None);
        assert!(harness.state().toasts.is_empty());
    }

    #[test]
    fn toasts_draw_on_a_layer_above_modals() {
        let ctx = crate::ui::testing::themed_context();
        let mut toast = ToastState::default();
        toast.show("On top");
        render_at(&mut toast, &ctx, 0.0);
        render_at(&mut toast, &ctx, 0.1);
        let layer = egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("app_toast"));
        assert!(ctx.memory(|memory| memory.areas().visible_last_frame(&layer)));
        // Modals use Foreground or lower; Tooltip sorts above all of them.
        assert!(egui::Order::Tooltip > egui::Order::Foreground);
    }
}
