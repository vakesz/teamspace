use crate::app::presence::LocalPresence;
use crate::app::session::{RestoredSession, restore_session, save_email, save_signed_out};
use crate::data::workspace::workspace;
use crate::ui::shortcuts::{singleline_submit_pressed, take_escape};
use crate::ui::theme;
use eframe::egui::{self, Color32, RichText, Stroke};

pub(crate) struct AuthState {
    authenticated: bool,
    email: String,
    message: Option<String>,
    profile_open: bool,
    profile_just_opened: bool,
    /// Set when the account menu's Settings item was chosen; the shell takes
    /// it once per frame.
    settings_requested: bool,
    local_presence: LocalPresence,
    presence_dirty: bool,
}

impl Default for AuthState {
    fn default() -> Self {
        // Side-effect free: signed out, no disk access. The real app restores
        // the stored session explicitly through `AuthState::restored`.
        Self::from_restored_session(RestoredSession {
            authenticated: false,
            email: String::new(),
        })
    }
}

impl AuthState {
    /// Restores the persisted session. Only the real application path uses
    /// this; `Default` never touches the disk.
    pub(crate) fn restored() -> Self {
        Self::from_restored_session(restore_session())
    }

    /// Authenticated local-preview state that never reads the user's stored
    /// session, so previews and screenshots are deterministic.
    #[cfg(any(test, feature = "screenshot"))]
    pub(crate) fn local_preview() -> Self {
        Self::from_restored_session(RestoredSession {
            authenticated: true,
            email: String::new(),
        })
    }

    fn from_restored_session(session: RestoredSession) -> Self {
        Self {
            authenticated: session.authenticated,
            email: session.email,
            message: None,
            profile_open: false,
            profile_just_opened: false,
            settings_requested: false,
            local_presence: LocalPresence::default(),
            presence_dirty: false,
        }
    }
    /// Renders the sign-in surface when needed and reports whether it owns the frame.
    pub(crate) fn render_gate(&mut self, ctx: &egui::Context) -> bool {
        if self.authenticated {
            return false;
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::canvas()))
            .show(ctx, |ui| {
                ui.centered_and_justified(|ui| {
                    egui::Frame::new()
                        .fill(theme::surface())
                        .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
                        .corner_radius(14.0)
                        .inner_margin(egui::Margin::symmetric(34, 30))
                        .show(ui, |ui| self.render_form(ui));
                });
            });
        true
    }

    pub(crate) fn display_name(&self) -> &str {
        if self.email.is_empty() {
            &workspace().current_user
        } else {
            &self.email
        }
    }

    /// Whether Settings was chosen from the account menu since the last call.
    pub(crate) fn take_settings_request(&mut self) -> bool {
        std::mem::take(&mut self.settings_requested)
    }

    #[cfg(test)]
    pub(crate) const fn request_settings_for_test(&mut self) {
        self.settings_requested = true;
    }

    pub(crate) fn toggle_profile(&mut self) {
        self.profile_open = !self.profile_open;
        self.profile_just_opened = self.profile_open;
    }

    /// Ends the session from outside the account menu (the command palette).
    pub(crate) fn sign_out(&mut self) {
        self.finish_sign_out(save_signed_out());
    }

    pub(crate) fn dismiss_profile_menu(&mut self) {
        self.profile_open = false;
        self.profile_just_opened = false;
    }

    pub(crate) const fn is_profile_open(&self) -> bool {
        self.profile_open
    }

    pub(crate) const fn is_authenticated(&self) -> bool {
        self.authenticated
    }

    /// Returns the profile-scoped local preview presence. It is never a
    /// Microsoft 365 account status and is intentionally not a transport
    /// action.
    pub(crate) const fn local_presence(&self) -> LocalPresence {
        self.local_presence
    }

    pub(crate) fn restore_local_presence(&mut self, presence: LocalPresence) {
        self.local_presence = presence;
        self.presence_dirty = false;
    }

    pub(crate) fn set_local_presence(&mut self, presence: LocalPresence) -> bool {
        if self.local_presence == presence {
            return false;
        }
        self.local_presence = presence;
        self.presence_dirty = true;
        true
    }

    pub(crate) const fn is_presence_dirty(&self) -> bool {
        self.presence_dirty
    }

    pub(crate) fn mark_presence_persisted(&mut self) {
        self.presence_dirty = false;
    }

    /// Identifies the current local-preview profile for on-device state. This
    /// is not a Microsoft identity or an authentication token.
    pub(crate) fn local_profile(&self) -> &str {
        if self.email.is_empty() {
            "local-preview"
        } else {
            &self.email
        }
    }

    /// Returns a persistence warning only after authentication, so the app can
    /// present it in its shared toast surface instead of losing it with the
    /// sign-in screen.
    pub(crate) fn take_authenticated_notice(&mut self) -> Option<String> {
        self.authenticated.then(|| self.message.take()).flatten()
    }

    pub(crate) fn render_profile_menu(&mut self, ctx: &egui::Context) {
        if !self.profile_open {
            return;
        }
        if take_escape(ctx) {
            self.profile_open = false;
            return;
        }

        let just_opened = std::mem::take(&mut self.profile_just_opened);
        let response = egui::Window::new("Account")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-18.0, 62.0))
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(self.display_name())
                        .strong()
                        .color(theme::fg1()),
                );
                ui.label(
                    RichText::new("Local preview workspace")
                        .font(theme::regular(theme::font_size::SMALL))
                        .color(theme::fg3()),
                );
                ui.separator();
                ui.label(
                    RichText::new(format!("Status · {}", self.local_presence.label()))
                        .font(theme::semibold(theme::font_size::SMALL))
                        .color(theme::fg1()),
                );
                for presence in LocalPresence::ALL {
                    let response = ui
                        .horizontal(|ui| {
                            let (dot, _) = ui
                                .allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                            ui.painter()
                                .circle_filled(dot.center(), 4.0, presence.color());
                            ui.selectable_label(
                                self.local_presence == presence,
                                RichText::new(presence.label())
                                    .font(theme::regular(theme::font_size::CAPTION)),
                            )
                        })
                        .inner;
                    if response.clicked() {
                        self.set_local_presence(presence);
                    }
                }
                ui.label(
                    RichText::new(
                        "This changes only this local preview; it does not update Microsoft Teams.",
                    )
                    .font(theme::regular(theme::font_size::MICRO))
                    .color(theme::fg3()),
                );
                ui.separator();
                if ui.button("Settings").clicked() {
                    self.settings_requested = true;
                    self.profile_open = false;
                }
                if ui.button("Sign out").clicked() {
                    self.finish_sign_out(save_signed_out());
                }
            });
        if !just_opened && response.is_some_and(|response| response.response.clicked_elsewhere()) {
            self.profile_open = false;
        }
    }

    fn render_form(&mut self, ui: &mut egui::Ui) {
        ui.set_max_width(390.0);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new("T")
                    .font(theme::semibold(theme::font_size::DISPLAY))
                    .color(theme::brand_fg()),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new("Choose local profile")
                    .font(theme::semibold(theme::font_size::TITLE))
                    .color(theme::fg1()),
            );
            ui.label(
                RichText::new("Open the Teamspace preview with a profile stored on this device")
                    .font(theme::regular(theme::font_size::CAPTION))
                    .color(theme::fg3()),
            );
        });
        ui.add_space(24.0);

        ui.label(
            RichText::new("Profile email")
                .font(theme::semibold(theme::font_size::CAPTION))
                .color(theme::fg1()),
        );
        let email_input = ui.add_sized(
            [ui.available_width(), 40.0],
            egui::TextEdit::singleline(&mut self.email).hint_text("name@company.com"),
        );
        ui.add_space(16.0);
        let submit_requested = ui
            .add_sized(
                [ui.available_width(), 42.0],
                egui::Button::new(
                    RichText::new("Continue")
                        .font(theme::semibold(theme::font_size::BODY))
                        .color(theme::fg_on_brand()),
                )
                .fill(theme::brand())
                .corner_radius(7.0),
            )
            .clicked()
            || singleline_submit_pressed(&email_input, ui);
        if submit_requested {
            self.submit_profile();
        }
        self.render_message(ui, theme::danger());

        ui.add_space(18.0);
        ui.separator();
        ui.label(
            RichText::new("Local profile picker · no Microsoft account is connected")
                .font(theme::regular(theme::font_size::MICRO))
                .color(theme::fg3()),
        );
        ui.label(
            RichText::new(
                "Profiles keep drafts and local data separate on this device. This is not a \
                 security boundary: no password is requested, and anyone who can use this \
                 computer can open any profile.",
            )
            .font(theme::regular(theme::font_size::MICRO))
            .color(theme::fg3()),
        );
    }

    fn render_message(&self, ui: &mut egui::Ui, color: Color32) {
        if let Some(message) = &self.message {
            ui.add_space(8.0);
            ui.label(
                RichText::new(message)
                    .font(theme::regular(theme::font_size::CAPTION))
                    .color(color),
            );
        }
    }

    fn submit_profile(&mut self) {
        self.email = normalized_work_email(&self.email);
        if let Err(message) = validate_work_email(&self.email) {
            self.message = Some(message.into());
            return;
        }
        self.finish_sign_in(save_email(&self.email));
    }

    fn finish_sign_in(&mut self, persistence: std::io::Result<()>) {
        self.authenticated = true;
        self.profile_open = false;
        self.message = persistence.err().map(|_| {
            "Signed in locally, but the session could not be saved. You may need to sign in again after restarting the app."
                .into()
        });
    }

    fn clear_authenticated_state(&mut self) {
        self.authenticated = false;
        self.email.clear();
        self.message = None;
        self.profile_open = false;
        self.profile_just_opened = false;
        self.local_presence = LocalPresence::default();
        self.presence_dirty = false;
    }

    fn finish_sign_out(&mut self, persistence: std::io::Result<()>) {
        self.clear_authenticated_state();
        if persistence.is_err() {
            self.message = Some(
                "Signed out for this app session, but the sign-out state could not be saved. Closing the app may restore the local preview."
                    .into(),
            );
        }
    }
}

fn normalized_work_email(email: &str) -> String {
    email.trim().to_owned()
}

fn validate_work_email(email: &str) -> Result<(), &'static str> {
    let email = email.trim();
    let email_ok = email.split_once('@').is_some_and(|(name, domain)| {
        !name.is_empty()
            && !name.chars().any(char::is_whitespace)
            && domain.contains('.')
            && !domain.contains('@')
            && !domain.chars().any(char::is_whitespace)
            && domain.split('.').all(|part| !part.is_empty())
    });
    if !email_ok {
        return Err("Enter a valid email address.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{AuthState, normalized_work_email, validate_work_email};
    use crate::app::presence::LocalPresence;
    use eframe::egui;

    #[test]
    fn profile_email_validation_rejects_malformed_addresses() {
        for invalid in [
            "not-an-email",
            "alex@example.com@invalid",
            "alex@.example.com",
        ] {
            assert_eq!(
                validate_work_email(invalid),
                Err("Enter a valid email address."),
                "{invalid}"
            );
        }
        assert_eq!(validate_work_email("alex@example.com"), Ok(()));
        assert_eq!(validate_work_email("  alex@example.com  "), Ok(()));
    }

    #[test]
    fn email_normalization_removes_only_outer_whitespace() {
        assert_eq!(
            normalized_work_email("  alex@example.com  "),
            "alex@example.com"
        );
        assert_eq!(
            normalized_work_email("alex @example.com"),
            "alex @example.com"
        );
    }

    #[test]
    fn invalid_submission_keeps_the_user_signed_out_without_persisting() {
        let mut auth = AuthState {
            authenticated: false,
            email: "invalid".into(),
            message: None,
            profile_open: true,
            profile_just_opened: false,
            ..Default::default()
        };

        auth.submit_profile();

        assert!(!auth.authenticated);
        assert_eq!(
            auth.message.as_deref(),
            Some("Enter a valid email address.")
        );
        assert!(auth.profile_open);
    }

    #[test]
    fn display_name_uses_the_signed_in_email_and_falls_back_to_the_preview_user() {
        let mut auth = AuthState {
            authenticated: true,
            email: String::new(),
            message: None,
            profile_open: false,
            profile_just_opened: false,
            ..Default::default()
        };

        assert_eq!(
            auth.display_name(),
            crate::data::workspace::workspace().current_user
        );

        auth.email = "alex@example.com".into();
        assert_eq!(auth.display_name(), "alex@example.com");
    }

    #[test]
    fn authentication_accessor_reflects_the_session_state() {
        let mut auth = AuthState::from_restored_session(crate::app::session::RestoredSession {
            authenticated: true,
            email: String::new(),
        });

        assert!(auth.is_authenticated());
        auth.authenticated = false;
        assert!(!auth.is_authenticated());
    }

    #[test]
    fn local_presence_is_dirty_only_after_a_real_change_and_resets_on_sign_out() {
        let mut auth = AuthState::default();

        assert_eq!(auth.local_presence(), LocalPresence::Available);
        assert!(!auth.is_presence_dirty());
        assert!(!auth.set_local_presence(LocalPresence::Available));
        assert!(auth.set_local_presence(LocalPresence::Away));
        assert_eq!(auth.local_presence(), LocalPresence::Away);
        assert!(auth.is_presence_dirty());

        auth.mark_presence_persisted();
        assert!(!auth.is_presence_dirty());
        auth.restore_local_presence(LocalPresence::DoNotDisturb);
        assert_eq!(auth.local_presence(), LocalPresence::DoNotDisturb);
        assert!(!auth.is_presence_dirty());

        auth.clear_authenticated_state();
        assert_eq!(auth.local_presence(), LocalPresence::Available);
        assert!(!auth.is_presence_dirty());
    }

    #[test]
    fn restored_signed_out_session_opens_the_sign_in_gate_without_an_identity() {
        let auth = AuthState::from_restored_session(crate::app::session::RestoredSession {
            authenticated: false,
            email: String::new(),
        });

        assert!(!auth.is_authenticated());
        assert!(auth.email.is_empty());
        assert!(!auth.profile_open);
    }

    #[test]
    fn clearing_session_state_removes_credentials_and_the_profile_popover() {
        let mut auth = AuthState {
            authenticated: true,
            email: "alex@example.com".into(),
            message: Some("Welcome back".into()),
            profile_open: true,
            profile_just_opened: false,
            ..Default::default()
        };

        auth.clear_authenticated_state();

        assert!(!auth.authenticated);
        assert!(auth.email.is_empty());
        assert!(!auth.profile_open);
        assert!(auth.message.is_none());
    }

    #[test]
    fn failed_sign_out_still_ends_the_current_session_and_explains_the_risk() {
        let mut auth = AuthState {
            authenticated: true,
            email: "alex@example.com".into(),
            message: None,
            profile_open: true,
            profile_just_opened: false,
            ..Default::default()
        };
        assert!(auth.set_local_presence(LocalPresence::Offline));

        auth.finish_sign_out(Err(std::io::Error::other("configuration unavailable")));

        assert!(!auth.authenticated);
        assert!(auth.email.is_empty());
        assert!(!auth.profile_open);
        assert_eq!(auth.local_presence(), LocalPresence::Available);
        assert!(!auth.is_presence_dirty());
        assert_eq!(
            auth.message.as_deref(),
            Some(
                "Signed out for this app session, but the sign-out state could not be saved. Closing the app may restore the local preview."
            )
        );
    }

    #[test]
    fn failed_sign_in_authenticates_locally_and_queues_a_single_persistence_notice() {
        let mut auth = AuthState {
            authenticated: false,
            email: "alex@example.com".into(),
            message: Some("A stale form message".into()),
            profile_open: true,
            profile_just_opened: false,
            ..Default::default()
        };

        auth.finish_sign_in(Err(std::io::Error::other("configuration unavailable")));

        assert!(auth.authenticated);
        assert!(!auth.profile_open);
        assert_eq!(
            auth.take_authenticated_notice().as_deref(),
            Some(
                "Signed in locally, but the session could not be saved. You may need to sign in again after restarting the app."
            )
        );
        assert!(auth.take_authenticated_notice().is_none());
    }

    #[test]
    fn escape_closes_the_profile_popover_without_signing_out() {
        let ctx = egui::Context::default();
        let mut auth = AuthState {
            authenticated: true,
            profile_open: true,
            ..Default::default()
        };
        assert!(auth.set_local_presence(LocalPresence::Away));
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

        auth.render_profile_menu(&ctx);
        let _ = ctx.end_pass();

        assert!(!auth.profile_open);
        assert!(auth.authenticated);
        assert_eq!(auth.local_presence(), LocalPresence::Away);
    }

    #[test]
    fn account_popover_ignores_its_opening_click_then_closes_on_an_outside_click() {
        let ctx = crate::ui::testing::themed_context();
        let mut auth = AuthState::default();
        auth.toggle_profile();
        let opening_click = egui::pos2(10.0, 10.0);
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1_000.0, 700.0));

        ctx.begin_pass(egui::RawInput {
            screen_rect: Some(screen_rect),
            events: click_events(opening_click),
            ..Default::default()
        });
        auth.render_profile_menu(&ctx);
        let _ = ctx.end_pass();
        assert!(auth.profile_open);

        ctx.begin_pass(egui::RawInput {
            screen_rect: Some(screen_rect),
            events: click_events(opening_click),
            ..Default::default()
        });
        auth.render_profile_menu(&ctx);
        let _ = ctx.end_pass();

        assert!(!auth.profile_open);
    }

    fn click_events(position: egui::Pos2) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(position),
            egui::Event::PointerButton {
                pos: position,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerButton {
                pos: position,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
        ]
    }
}
