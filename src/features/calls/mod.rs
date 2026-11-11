mod persistence;
#[cfg(feature = "fuzzing")]
pub(crate) use persistence::fuzz_snapshot;

pub(crate) use self::persistence::LocalCallsStore;
pub(crate) use self::persistence::StoredCalls;
use self::persistence::{
    CALLS_SCHEMA_VERSION, StoredAudioDevice, StoredCallForwarding, StoredCallHistoryEntry,
};
use crate::features::search::workspace::{WorkspaceSearchResult, WorkspaceSearchTarget};
use crate::ui::a11y::clickable;
use crate::ui::avatar::{Avatar, AvatarSize};
use crate::ui::icon::{Icon, IconSize, icon_button};
use crate::ui::modal;
use crate::ui::shortcuts::{singleline_submit_pressed, take_escape};
use crate::ui::theme::{self, font_size};
use crate::ui::toast::{ToastState, UndoKind};
use eframe::egui::{self, Align, Color32, Layout, RichText, Vec2};

pub(crate) struct CallsState {
    call_target: String,
    call_filter: CallFilter,
    call_forwarding: CallForwarding,
    audio_device: AudioDevice,
    speed_dial: Vec<String>,
    local_call_history: Vec<LocalCallHistoryEntry>,
    history_open: bool,
    contacts_open: bool,
    /// Draft text belongs to the transient Contacts modal, not the durable
    /// speed-dial projection. Keeping it here prevents a cancelled contact
    /// form from reappearing after a profile restart.
    local_contact_draft: String,
    local_contact_focus_requested: bool,
    active_call: Option<ActiveCall>,
    next_local_history_id: u64,
    persistence_dirty: bool,
    /// The last removed history entry and speed dial contact, held in memory
    /// for the Undo toast. Both removals are persisted immediately; undo
    /// re-inserts and persists again.
    undo_history: Option<(usize, LocalCallHistoryEntry)>,
    undo_speed_dial: Option<(usize, String)>,
}

impl Default for CallsState {
    fn default() -> Self {
        Self {
            call_target: String::new(),
            call_filter: CallFilter::All,
            call_forwarding: CallForwarding::Disabled,
            audio_device: AudioDevice::Internal,
            speed_dial: Vec::new(),
            local_call_history: Vec::new(),
            history_open: true,
            contacts_open: false,
            local_contact_draft: String::new(),
            local_contact_focus_requested: false,
            active_call: None,
            next_local_history_id: 1,
            persistence_dirty: false,
            undo_history: None,
            undo_speed_dial: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LocalCallHistoryEntry {
    id: u64,
    contact: String,
}

struct ActiveCall {
    participant: String,
    mode: CallMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CallMode {
    Audio,
    Video,
}

impl CallMode {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Audio => "Audio",
            Self::Video => "Video",
        }
    }
}

const CALL_CONTACTS: &[&str] = &["Wren Castellanos", "Dara Whitlock", "Kiran Doyle"];
// Keep these in step with the feature storage bounds. Enforcing them at the
// state boundary means a normal local interaction cannot create a snapshot
// that the persistence adapter will later reject.
const MAX_SPEED_DIAL_ENTRIES: usize = 128;
const MAX_LOCAL_CALL_HISTORY_ENTRIES: usize = 512;
const MAX_DURABLE_CALL_CONTACT_BYTES: usize = 512;

#[derive(Clone, Copy, PartialEq, Eq)]
enum CallFilter {
    All,
    Missed,
    Incoming,
    Outgoing,
    Voicemail,
}

/// Why a custom local contact could not be added to Speed Dial.
///
/// These errors are intentionally feature-local: adding a contact only
/// changes the profile's local speed-dial list and never creates a provider
/// contact or sends a network request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LocalContactError {
    Empty,
    Invalid,
    Duplicate,
    Full,
}

impl LocalContactError {
    const fn message(self) -> &'static str {
        match self {
            Self::Empty => "Enter a name before adding a local contact.",
            Self::Invalid => "That local contact cannot be saved.",
            Self::Duplicate => "That contact is already in local speed dial.",
            Self::Full => "Local speed dial is full.",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CallForwarding {
    Disabled,
    Voicemail,
}

impl CallForwarding {
    const fn label(self) -> &'static str {
        match self {
            Self::Disabled => "Don't forward",
            Self::Voicemail => "Forward to voicemail",
        }
    }

    const fn from_stored(value: StoredCallForwarding) -> Self {
        match value {
            StoredCallForwarding::Disabled => Self::Disabled,
            StoredCallForwarding::Voicemail => Self::Voicemail,
        }
    }

    const fn stored(self) -> StoredCallForwarding {
        match self {
            Self::Disabled => StoredCallForwarding::Disabled,
            Self::Voicemail => StoredCallForwarding::Voicemail,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioDevice {
    Internal,
    Headset,
    SystemDefault,
}

impl AudioDevice {
    const fn label(self) -> &'static str {
        match self {
            Self::Internal => "Internal mic and speakers",
            Self::Headset => "External headset",
            Self::SystemDefault => "System default",
        }
    }

    const fn from_stored(value: StoredAudioDevice) -> Self {
        match value {
            StoredAudioDevice::Internal => Self::Internal,
            StoredAudioDevice::Headset => Self::Headset,
            StoredAudioDevice::SystemDefault => Self::SystemDefault,
        }
    }

    const fn stored(self) -> StoredAudioDevice {
        match self {
            Self::Internal => StoredAudioDevice::Internal,
            Self::Headset => StoredAudioDevice::Headset,
            Self::SystemDefault => StoredAudioDevice::SystemDefault,
        }
    }
}

impl CallFilter {
    const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Missed => "Missed",
            Self::Incoming => "Incoming",
            Self::Outgoing => "Outgoing",
            Self::Voicemail => "Voicemail",
        }
    }
}

impl CallsState {
    /// Rebuilds durable local Calls choices while intentionally resetting the
    /// dial target, filters, dialogs, and any active call session.
    pub(crate) fn from_stored(stored: StoredCalls) -> Self {
        let mut local_call_history = stored
            .local_call_history
            .into_iter()
            .map(|entry| LocalCallHistoryEntry {
                id: entry.id,
                contact: entry.contact,
            })
            .collect::<Vec<_>>();
        // Entries are written newest-first. Canonicalizing by monotonic local
        // id keeps that order stable even if an older snapshot was reordered.
        local_call_history.sort_by_key(|entry| std::cmp::Reverse(entry.id));
        local_call_history.truncate(MAX_LOCAL_CALL_HISTORY_ENTRIES);
        let next_after_restored_id = local_call_history
            .iter()
            .map(|entry| entry.id)
            .max()
            .map_or(1, |id| id.saturating_add(1));

        Self {
            call_forwarding: CallForwarding::from_stored(stored.call_forwarding),
            audio_device: AudioDevice::from_stored(stored.audio_device),
            speed_dial: stored.speed_dial,
            local_call_history,
            history_open: stored.history_open,
            next_local_history_id: stored
                .next_local_history_id
                .max(next_after_restored_id)
                .max(1),
            ..Self::default()
        }
    }

    /// Captures only user-owned Calls settings and completed local history.
    /// Active-call and media/session state is deliberately not serializable.
    pub(crate) fn stored(&self) -> StoredCalls {
        StoredCalls {
            schema_version: CALLS_SCHEMA_VERSION,
            call_forwarding: self.call_forwarding.stored(),
            audio_device: self.audio_device.stored(),
            speed_dial: self.speed_dial.clone(),
            history_open: self.history_open,
            next_local_history_id: self.next_local_history_id,
            local_call_history: self
                .local_call_history
                .iter()
                .map(|entry| StoredCallHistoryEntry {
                    id: entry.id,
                    contact: entry.contact.clone(),
                })
                .collect(),
        }
    }

    pub(crate) const fn is_persistence_dirty(&self) -> bool {
        self.persistence_dirty
    }

    pub(crate) fn mark_persisted(&mut self) {
        self.persistence_dirty = false;
    }

    pub(crate) const fn has_keyboard_modal(&self) -> bool {
        self.contacts_open
    }

    pub(crate) fn dismiss_contextual_overlays(&mut self) {
        self.close_contacts_dialog();
    }

    pub(crate) fn search_results(&self, query: &str) -> Vec<WorkspaceSearchResult> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return Vec::new();
        }

        let mut contacts = CALL_CONTACTS
            .iter()
            .copied()
            .filter(|name| name.to_lowercase().contains(&query))
            .map(|name| WorkspaceSearchResult {
                label: name.into(),
                detail: "Calls contact".into(),
                target: WorkspaceSearchTarget::CallContact(name.into()),
            })
            .collect::<Vec<_>>();
        for name in &self.speed_dial {
            if name.to_lowercase().contains(&query)
                && !contacts
                    .iter()
                    .any(|result| result.label.trim().to_lowercase() == name.trim().to_lowercase())
            {
                contacts.push(WorkspaceSearchResult {
                    label: name.clone(),
                    detail: "Calls · local speed dial".into(),
                    target: WorkspaceSearchTarget::CallContact(name.clone()),
                });
            }
        }
        for entry in &self.local_call_history {
            let name = &entry.contact;
            if name.to_lowercase().contains(&query)
                && !contacts
                    .iter()
                    .any(|result| result.label.trim().to_lowercase() == name.trim().to_lowercase())
            {
                contacts.push(WorkspaceSearchResult {
                    label: name.clone(),
                    detail: "Calls · local history".into(),
                    target: WorkspaceSearchTarget::CallContact(name.clone()),
                });
            }
        }
        contacts
    }

    pub(crate) fn select_search_result(&mut self, contact: String) {
        self.close_contacts_dialog();
        self.call_target = contact;
    }

    /// Opens the local Contacts modal with a fresh transient contact draft.
    /// Existing contacts stay in the durable speed-dial projection; only the
    /// unsaved input is discarded when a new modal session begins.
    fn open_contacts_dialog(&mut self) {
        if !self.contacts_open {
            self.local_contact_draft.clear();
            self.local_contact_focus_requested = true;
        }
        self.contacts_open = true;
    }

    /// Closes the Contacts modal without persisting its unsaved contact text.
    fn close_contacts_dialog(&mut self) {
        self.contacts_open = false;
        self.local_contact_draft.clear();
        self.local_contact_focus_requested = false;
    }

    pub(crate) fn draw_sidebar(
        &mut self,
        ui: &mut egui::Ui,
        toast: &mut ToastState,
        meeting_active: bool,
    ) {
        ui.add_space(20.0);
        ui.horizontal(|ui| {
            ui.add_space(18.0);
            ui.label(
                RichText::new("Calls")
                    .font(theme::semibold(font_size::TITLE))
                    .color(theme::fg1()),
            );
            ui.label(
                RichText::new("Personal")
                    .font(theme::regular(font_size::CAPTION))
                    .color(theme::brand_fg()),
            );
        });
        ui.add_space(18.0);
        let mut submitted_from_keyboard = false;
        ui.horizontal(|ui| {
            ui.add_space(16.0);
            let response = ui.add_sized(
                [ui.available_width() - 30.0, 34.0],
                egui::TextEdit::singleline(&mut self.call_target).hint_text("Type a name"),
            );
            submitted_from_keyboard = singleline_submit_pressed(&response, ui);
        });
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.add_space(16.0);
            let blocked_message = self.call_start_blocked_message(meeting_active);
            let can_call = blocked_message.is_none();
            let call_availability_hint = blocked_message.unwrap_or("Start a local audio call");
            let submitted_from_button = ui
                .add_enabled(can_call, primary_button(Icon::Call, "Call", can_call))
                .on_hover_text(call_availability_hint)
                .clicked();
            if submitted_from_button || submitted_from_keyboard {
                if let Some(message) = self.call_start_blocked_message(meeting_active) {
                    toast.show(format!("{message}."));
                } else if let Some(target) = self.start_call_from_target(meeting_active) {
                    toast.show(format!("Started a local outgoing call to {target}"));
                }
            }
        });
        if self.active_call.is_some() {
            ui.horizontal(|ui| {
                ui.add_space(16.0);
                ui.label(
                    RichText::new("End the active local call before starting another.")
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg3()),
                );
            });
        } else if meeting_active {
            ui.horizontal(|ui| {
                ui.add_space(16.0);
                ui.label(
                    RichText::new("Leave the active local meeting before starting a call.")
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg3()),
                );
            });
        }
        self.draw_call_settings(ui, toast);
    }

    pub(crate) fn draw_page(
        &mut self,
        ui: &mut egui::Ui,
        toast: &mut ToastState,
        meeting_active: bool,
    ) {
        ui.horizontal(|ui| {
            ui.add_space(20.0);
            ui.label(
                RichText::new("Calls")
                    .font(theme::semibold(font_size::SUBTITLE))
                    .color(theme::fg1()),
            );
            ui.add_space(12.0);
            ui.label(
                RichText::new("Personal")
                    .font(theme::semibold(font_size::BODY))
                    .color(theme::fg1()),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("View contacts").clicked() {
                    self.open_contacts_dialog();
                }
            });
        });
        ui.add_space(10.0);
        ui.separator();
        // The desktop reference keeps history and speed dial visible together.
        // At narrower widths, stack the two functional regions rather than
        // squeezing either one into an unusable column.
        if ui.available_width() >= 740.0 {
            let available_height = ui.available_height();
            let history_width = (ui.available_width() * 0.57).max(390.0);
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(
                    Vec2::new(history_width, available_height),
                    Layout::top_down(Align::Min),
                    |ui| self.draw_history(ui, toast),
                );
                ui.separator();
                ui.allocate_ui_with_layout(
                    Vec2::new(ui.available_width(), available_height),
                    Layout::top_down(Align::Min),
                    |ui| self.draw_speed_dial(ui, toast, meeting_active),
                );
            });
        } else {
            self.draw_history(ui, toast);
            ui.separator();
            self.draw_speed_dial(ui, toast, meeting_active);
        }
        self.draw_contacts_dialog(ui.ctx(), toast, meeting_active);
    }

    fn draw_history(&mut self, ui: &mut egui::Ui, toast: &mut ToastState) {
        ui.add_space(16.0);
        ui.horizontal(|ui| {
            ui.add_space(22.0);
            let history_open = self.history_open;
            if history_toggle(ui, history_open)
                .on_hover_text(if history_open {
                    "Collapse call history"
                } else {
                    "Expand call history"
                })
                .clicked()
            {
                self.toggle_history();
            }
            if self.history_open {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    for filter in [
                        CallFilter::Voicemail,
                        CallFilter::Outgoing,
                        CallFilter::Incoming,
                        CallFilter::Missed,
                        CallFilter::All,
                    ] {
                        let selected = self.call_filter == filter;
                        if filter_chip(ui, filter.label(), selected).clicked() {
                            self.call_filter = filter;
                            toast.show(format!("Showing {} calls", filter.label().to_lowercase()));
                        }
                    }
                });
            }
        });
        if !self.history_open {
            return;
        }
        ui.add_space(10.0);
        let mut local_history_removal = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut selected_contact = None;
                if matches!(self.call_filter, CallFilter::All | CallFilter::Outgoing) {
                    for entry in &self.local_call_history {
                        let name = &entry.contact;
                        let mut removal_requested = false;
                        let mut action_interacted = false;
                        let response = draw_call_row(
                            ui,
                            name,
                            "Outgoing",
                            "Just now",
                            "Local preview",
                            |ui| {
                                let more = icon_button(ui, Icon::More, "More actions");
                                egui::Popup::menu(&more).show(|ui| {
                                    if ui
                                        .button(
                                            RichText::new("Remove from local history")
                                                .color(theme::danger()),
                                        )
                                        .clicked()
                                    {
                                        removal_requested = true;
                                        ui.close();
                                    }
                                });
                                action_interacted = more.clicked();
                            },
                        )
                        .on_hover_text("Choose this person for a local call");
                        if removal_requested {
                            local_history_removal = Some(entry.id);
                        } else if !action_interacted && response.clicked() {
                            selected_contact = Some(name.clone());
                        }
                    }
                }
                for (name, direction, when, duration) in call_history() {
                    if !self.call_filter.includes(direction) {
                        continue;
                    }
                    if draw_call_row(ui, name, direction, when, duration, |_| {})
                        .on_hover_text("Choose this person for a local call")
                        .clicked()
                    {
                        selected_contact = Some(name.into());
                    }
                }
                if let Some(contact) = selected_contact {
                    self.select_search_result(contact.clone());
                    toast.show(format!("Ready to call {contact}"));
                }
            });
        if let Some(id) = local_history_removal
            && let Some(contact) = self.remove_local_history_entry(id)
        {
            toast.show_undo(
                format!("Removed {contact} from local call history"),
                UndoKind::CallHistory,
            );
        }
    }

    fn draw_call_settings(&mut self, ui: &mut egui::Ui, toast: &mut ToastState) {
        ui.allocate_ui_with_layout(ui.available_size(), Layout::bottom_up(Align::Min), |ui| {
            ui.add_space(12.0);
            ui.separator();
            ui.horizontal(|ui| {
                ui.add_space(16.0);
                let forwarding_button =
                    dropdown_button(ui, Icon::CallForward, self.call_forwarding.label());
                egui::Popup::menu(&forwarding_button).show(|ui| {
                    for forwarding in [CallForwarding::Disabled, CallForwarding::Voicemail] {
                        if ui
                            .selectable_label(
                                self.call_forwarding == forwarding,
                                forwarding.label(),
                            )
                            .clicked()
                        {
                            if self.set_call_forwarding(forwarding) {
                                toast.show(format!(
                                    "Call forwarding set to {}",
                                    forwarding.label().to_lowercase()
                                ));
                            }
                            ui.close();
                        }
                    }
                });
            });
            ui.add_space(5.0);
            ui.horizontal(|ui| {
                ui.add_space(16.0);
                let device_button = dropdown_button(ui, Icon::Mic, self.audio_device.label());
                egui::Popup::menu(&device_button).show(|ui| {
                    for device in [
                        AudioDevice::Internal,
                        AudioDevice::Headset,
                        AudioDevice::SystemDefault,
                    ] {
                        if ui
                            .selectable_label(self.audio_device == device, device.label())
                            .clicked()
                        {
                            if self.set_audio_device(device) {
                                toast.show(format!(
                                    "Audio device set to {}",
                                    device.label().to_lowercase()
                                ));
                            }
                            ui.close();
                        }
                    }
                });
            });
            ui.add_space(8.0);
        });
    }

    fn draw_speed_dial(&mut self, ui: &mut egui::Ui, toast: &mut ToastState, meeting_active: bool) {
        let mut call_requested = None;
        let mut removal_requested = None;
        let can_start_call = !meeting_active && self.active_call.is_none();
        let call_availability_hint = if meeting_active {
            "Leave the active local meeting before starting a call"
        } else if self.active_call.is_some() {
            "End the active local call before starting another"
        } else {
            "Start a local audio call"
        };

        ui.add_space(16.0);
        ui.horizontal(|ui| {
            ui.add_space(20.0);
            ui.label(
                RichText::new("Speed dial")
                    .font(theme::semibold(font_size::BODY))
                    .color(theme::fg1()),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add(secondary_button(Icon::Add, "Add"))
                    .on_hover_text("Choose or add a local contact for speed dial")
                    .clicked()
                {
                    self.open_contacts_dialog();
                }
            });
        });

        if self.speed_dial.is_empty() {
            ui.add_space(84.0);
            ui.vertical_centered(|ui| {
                let (art, _) = ui.allocate_exact_size(Vec2::new(64.0, 54.0), egui::Sense::hover());
                draw_speed_dial_empty_art(ui.painter(), art);
                ui.add_space(6.0);
                ui.label(
                    RichText::new("Add people to speed dial for quick access.")
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg1()),
                );
                ui.add_space(8.0);
                if ui.button("Add a contact").clicked() {
                    self.open_contacts_dialog();
                }
            });
            return;
        }

        ui.add_space(12.0);
        for (index, person) in self.speed_dial.iter().enumerate() {
            let response = egui::Frame::new()
                .fill(theme::surface_subtle())
                .corner_radius(theme::radius::LG)
                .inner_margin(egui::Margin::symmetric(14, 10))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        Avatar::new(person, AvatarSize::Md32).show(ui);
                        ui.label(
                            RichText::new(person)
                                .font(theme::regular(font_size::BODY))
                                .color(theme::fg1()),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if icon_button(ui, Icon::Dismiss, "Remove from speed dial").clicked() {
                                removal_requested = Some(index);
                            }
                            if ui
                                .add_enabled(can_start_call, secondary_button(Icon::Call, "Call"))
                                .on_hover_text(call_availability_hint)
                                .clicked()
                            {
                                call_requested = Some(person.clone());
                            }
                        });
                    });
                });
            response.response.on_hover_text("Local speed dial contact");
            ui.add_space(8.0);
        }

        if let Some(index) = removal_requested
            && let Some(person) = self.remove_speed_dial(index)
        {
            toast.show_undo(
                format!("Removed {person} from speed dial"),
                UndoKind::SpeedDial,
            );
        }
        if let Some(person) = call_requested
            && let Some(person) = self.start_local_call(&person)
        {
            toast.show(format!("Started a local outgoing call to {person}"));
        }
    }

    fn draw_contacts_dialog(
        &mut self,
        ctx: &egui::Context,
        toast: &mut ToastState,
        meeting_active: bool,
    ) {
        if !self.contacts_open {
            return;
        }
        if take_escape(ctx) {
            self.close_contacts_dialog();
            return;
        }

        let mut selected_contact = None::<String>;
        let mut speed_dial_addition = None::<String>;
        let mut speed_dial_removal = None;
        let mut custom_contact_add_requested = false;
        let mut close_requested = false;
        let focus_local_contact = std::mem::take(&mut self.local_contact_focus_requested);
        let call_availability_hint = if meeting_active {
            "Leave the active local meeting before starting a call"
        } else if self.active_call.is_some() {
            "End the active local call before starting another"
        } else {
            "Start a local audio call"
        };
        let _ = modal::show(ctx, "call_contacts", 420.0, |ui| {
            modal::heading(ui, "Contacts");
            ui.add_space(4.0);
            ui.label(
                RichText::new("Local preview contacts")
                    .font(theme::semibold(font_size::CAPTION))
                    .color(theme::fg1()),
            );
            ui.label(
                RichText::new(
                    "Starting a call adds it to local call history. Contacts stay on this device.",
                )
                .font(theme::regular(font_size::CAPTION))
                .color(theme::fg3()),
            );
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(8.0);
            ui.label(
                RichText::new("Add a local contact")
                    .font(theme::semibold(font_size::CAPTION))
                    .color(theme::fg1()),
            );
            ui.label(
                RichText::new(
                    "Adds only to local Speed Dial; it does not create a real Teams contact.",
                )
                .font(theme::regular(font_size::CAPTION))
                .color(theme::fg3()),
            );
            ui.add_space(5.0);
            ui.horizontal(|ui| {
                let input = ui.add_sized(
                    [(ui.available_width() - 118.0).max(120.0), 30.0],
                    egui::TextEdit::singleline(&mut self.local_contact_draft)
                        .hint_text("Name or group"),
                );
                if focus_local_contact {
                    input.request_focus();
                }
                if singleline_submit_pressed(&input, ui) {
                    custom_contact_add_requested = true;
                }
                if ui.button("Add to Speed Dial").clicked() {
                    custom_contact_add_requested = true;
                }
            });
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(8.0);
            ui.label(
                RichText::new("Suggested contacts")
                    .font(theme::semibold(font_size::CAPTION))
                    .color(theme::fg1()),
            );
            ui.add_space(4.0);
            for &contact in CALL_CONTACTS {
                ui.horizontal(|ui| {
                    Avatar::new(contact, AvatarSize::Md32).show(ui);
                    ui.label(
                        RichText::new(contact)
                            .font(theme::regular(font_size::CAPTION))
                            .color(theme::fg1()),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add_enabled(
                                !meeting_active && self.active_call.is_none(),
                                secondary_button(Icon::Call, "Call"),
                            )
                            .on_hover_text(call_availability_hint)
                            .clicked()
                        {
                            selected_contact = Some(contact.into());
                        }
                        let is_on_speed_dial =
                            self.speed_dial.iter().any(|person| person == contact);
                        if ui
                            .small_button(if is_on_speed_dial {
                                "Remove speed dial"
                            } else {
                                "Add speed dial"
                            })
                            .clicked()
                        {
                            if is_on_speed_dial {
                                speed_dial_removal =
                                    self.speed_dial.iter().position(|person| person == contact);
                            } else {
                                speed_dial_addition = Some(contact.into());
                            }
                        }
                    });
                });
                ui.add_space(5.0);
            }
            if !self.speed_dial.is_empty() {
                ui.separator();
                ui.label(
                    RichText::new("Speed dial")
                        .font(theme::semibold(font_size::CAPTION))
                        .color(theme::fg1()),
                );
                for (index, person) in self.speed_dial.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                !meeting_active && self.active_call.is_none(),
                                secondary_button(Icon::Call, person),
                            )
                            .on_hover_text(call_availability_hint)
                            .clicked()
                        {
                            selected_contact = Some(person.clone());
                        }
                        if icon_button(ui, Icon::Dismiss, "Remove from speed dial").clicked() {
                            speed_dial_removal = Some(index);
                        }
                    });
                }
            }
            ui.add_space(8.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Close").clicked() {
                    close_requested = true;
                }
            });
        });

        if let Some(contact) = speed_dial_addition
            && self.add_speed_dial(&contact)
        {
            toast.show(format!("Added {contact} to speed dial"));
        }
        if custom_contact_add_requested {
            match self.add_local_contact_draft() {
                Ok(contact) => toast.show(format!("Added {contact} to local speed dial")),
                Err(error) => toast.show_error(error.message()),
            }
        }
        if let Some(index) = speed_dial_removal
            && let Some(person) = self.remove_speed_dial(index)
        {
            toast.show_undo(
                format!("Removed {person} from speed dial"),
                UndoKind::SpeedDial,
            );
        }
        if let Some(contact) = selected_contact {
            if meeting_active {
                toast.show_error("Leave the active local meeting before starting a call.");
            } else if let Some(contact) = self.start_local_call(&contact) {
                toast.show(format!("Started a local outgoing call to {contact}"));
            }
            close_requested = true;
        }
        if close_requested {
            self.close_contacts_dialog();
        }
    }

    fn start_local_call(&mut self, target: &str) -> Option<String> {
        self.start_call(target, CallMode::Audio)
    }

    fn call_start_blocked_message(&self, meeting_active: bool) -> Option<&'static str> {
        if meeting_active {
            Some("Leave the active local meeting before starting a call")
        } else if self.active_call.is_some() {
            Some("End the active local call before starting another")
        } else if self.call_target.trim().is_empty() {
            Some("Type a name before starting a local call")
        } else {
            None
        }
    }

    pub(crate) fn start_call(&mut self, target: &str, mode: CallMode) -> Option<String> {
        if self.active_call.is_some() {
            return None;
        }
        let target = self.record_outgoing_call(target)?;
        self.call_target.clear();
        self.active_call = Some(ActiveCall {
            participant: target.clone(),
            mode,
        });
        Some(target)
    }

    pub(crate) const fn has_active_call(&self) -> bool {
        self.active_call.is_some()
    }

    fn end_active_call(&mut self) -> Option<String> {
        self.active_call.take().map(|call| call.participant)
    }

    pub(crate) fn draw_active_call(&mut self, ctx: &egui::Context, toast: &mut ToastState) {
        let Some((participant, mode)) = self
            .active_call
            .as_ref()
            .map(|call| (call.participant.clone(), call.mode))
        else {
            return;
        };
        let audio_device = self.audio_device.label();
        let mut end_requested = false;
        egui::Window::new(format!("{} call", mode.label()))
            .collapsible(false)
            .resizable(false)
            .default_width(390.0)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new("Connected locally")
                        .font(theme::semibold(font_size::SUBTITLE))
                        .color(theme::fg1()),
                );
                ui.label(
                    RichText::new(&participant)
                        .font(theme::regular(font_size::BODY))
                        .color(theme::fg1()),
                );
                ui.add_space(10.0);
                ui.label(
                    RichText::new(format!("Audio device: {audio_device}"))
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg3()),
                );
                ui.label(
                    RichText::new("This session-local call does not use a network connection.")
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg3()),
                );
                ui.add_space(14.0);
                if ui
                    .add(
                        egui::Button::new(RichText::new("End call").color(theme::fg_on_brand()))
                            .fill(theme::danger())
                            .corner_radius(theme::radius::MD),
                    )
                    .clicked()
                {
                    end_requested = true;
                }
            });
        if end_requested && let Some(participant) = self.end_active_call() {
            toast.show(format!("Ended local call with {participant}"));
        }
    }

    fn start_call_from_target(&mut self, meeting_active: bool) -> Option<String> {
        if meeting_active {
            return None;
        }
        let target = self.call_target.clone();
        self.start_local_call(&target)
    }

    pub(crate) fn record_outgoing_call(&mut self, target: &str) -> Option<String> {
        let target = target.trim();
        if !is_valid_durable_contact(target) {
            return None;
        }
        let target = target.to_owned();
        let id = self.next_local_history_id;
        self.next_local_history_id = id.checked_add(1)?;
        self.local_call_history.insert(
            0,
            LocalCallHistoryEntry {
                id,
                contact: target.clone(),
            },
        );
        self.local_call_history
            .truncate(MAX_LOCAL_CALL_HISTORY_ENTRIES);
        self.history_open = true;
        self.mark_persistence_dirty();
        Some(target)
    }

    /// Removes one user-owned local call record without changing a currently
    /// connected local call. The active session is transient and remains
    /// meaningful even if its already-created history record is forgotten.
    fn remove_local_history_entry(&mut self, id: u64) -> Option<String> {
        let index = self
            .local_call_history
            .iter()
            .position(|entry| entry.id == id)?;
        let entry = self.local_call_history.remove(index);
        let contact = entry.contact.clone();
        self.undo_history = Some((index, entry));
        // IDs are never reused: a later local call must remain distinguishable
        // from a record the user deliberately removed.
        self.mark_persistence_dirty();
        Some(contact)
    }

    /// Restores the last removed call history entry, keeping the list in its
    /// original order. IDs are never reused, so it cannot collide.
    pub(crate) fn undo_remove_history_entry(&mut self) -> bool {
        let Some((index, entry)) = self.undo_history.take() else {
            return false;
        };
        if self.local_call_history.len() >= MAX_LOCAL_CALL_HISTORY_ENTRIES
            || self
                .local_call_history
                .iter()
                .any(|known| known.id == entry.id)
        {
            return false;
        }
        // History is newest first and IDs only grow, so the ID alone fixes the
        // position even if newer calls were made while the toast was up.
        let _ = index;
        let position = self
            .local_call_history
            .iter()
            .position(|known| known.id < entry.id)
            .unwrap_or(self.local_call_history.len());
        self.local_call_history.insert(position, entry);
        self.mark_persistence_dirty();
        true
    }

    /// Restores the last removed speed dial contact at its old position unless
    /// the list filled up or the same contact was added again meanwhile.
    pub(crate) fn undo_remove_speed_dial(&mut self) -> bool {
        let Some((index, contact)) = self.undo_speed_dial.take() else {
            return false;
        };
        if self.speed_dial.len() >= MAX_SPEED_DIAL_ENTRIES
            || self
                .speed_dial
                .iter()
                .any(|person| person.trim().to_lowercase() == contact.to_lowercase())
        {
            return false;
        }
        let index = index.min(self.speed_dial.len());
        self.speed_dial.insert(index, contact);
        self.mark_persistence_dirty();
        true
    }

    fn toggle_history(&mut self) {
        self.history_open = !self.history_open;
        self.mark_persistence_dirty();
    }

    fn add_speed_dial(&mut self, contact: &str) -> bool {
        let contact = contact.trim();
        if self.speed_dial.len() >= MAX_SPEED_DIAL_ENTRIES
            || !is_valid_durable_contact(contact)
            || self
                .speed_dial
                .iter()
                .any(|person| person.trim().to_lowercase() == contact.to_lowercase())
        {
            return false;
        }
        self.speed_dial.push(contact.into());
        self.mark_persistence_dirty();
        true
    }

    /// Commits the Contacts modal's transient draft into the local Speed Dial
    /// projection. The draft remains visible on every rejected input, so a
    /// user can correct it without recreating a contact dialog; it clears only
    /// after the same validated `add_speed_dial` path accepts it.
    fn add_local_contact_draft(&mut self) -> Result<String, LocalContactError> {
        let contact = self.local_contact_draft.trim().to_owned();
        if contact.is_empty() {
            return Err(LocalContactError::Empty);
        }
        if !is_valid_durable_contact(&contact) {
            return Err(LocalContactError::Invalid);
        }
        if self
            .speed_dial
            .iter()
            .any(|person| person.trim().to_lowercase() == contact.to_lowercase())
        {
            return Err(LocalContactError::Duplicate);
        }
        if self.speed_dial.len() >= MAX_SPEED_DIAL_ENTRIES {
            return Err(LocalContactError::Full);
        }

        // All validator branches above mirror `add_speed_dial`; retaining the
        // shared mutation path keeps custom contacts subject to the exact same
        // local persistence contract as suggested contacts.
        if !self.add_speed_dial(&contact) {
            return Err(LocalContactError::Full);
        }
        self.local_contact_draft.clear();
        self.local_contact_focus_requested = true;
        Ok(contact)
    }

    fn remove_speed_dial(&mut self, index: usize) -> Option<String> {
        if index >= self.speed_dial.len() {
            return None;
        }
        let contact = self.speed_dial.remove(index);
        self.undo_speed_dial = Some((index, contact.clone()));
        self.mark_persistence_dirty();
        Some(contact)
    }

    fn set_call_forwarding(&mut self, forwarding: CallForwarding) -> bool {
        if self.call_forwarding == forwarding {
            return false;
        }
        self.call_forwarding = forwarding;
        self.mark_persistence_dirty();
        true
    }

    fn set_audio_device(&mut self, device: AudioDevice) -> bool {
        if self.audio_device == device {
            return false;
        }
        self.audio_device = device;
        self.mark_persistence_dirty();
        true
    }

    fn mark_persistence_dirty(&mut self) {
        self.persistence_dirty = true;
    }
}

fn is_valid_durable_contact(contact: &str) -> bool {
    !contact.is_empty()
        && contact.len() <= MAX_DURABLE_CALL_CONTACT_BYTES
        && !contact.chars().any(char::is_control)
}

fn draw_speed_dial_empty_art(painter: &egui::Painter, rect: egui::Rect) {
    let center = rect.center();
    painter.circle_filled(
        center + egui::vec2(-14.0, -6.0),
        13.0,
        theme::avatar_palette()[4].0,
    );
    painter.circle_filled(
        center + egui::vec2(7.0, 3.0),
        12.0,
        theme::avatar_palette()[5].0,
    );
    painter.circle_filled(
        center + egui::vec2(-14.0, -10.0),
        4.0,
        theme::avatar_palette()[4].1,
    );
    painter.circle_filled(
        center + egui::vec2(7.0, -1.0),
        4.0,
        theme::avatar_palette()[5].1,
    );
    painter.circle_filled(
        center + egui::vec2(18.0, 16.0),
        13.0,
        theme::presence_available(),
    );
    painter.text(
        center + egui::vec2(18.0, 16.0),
        egui::Align2::CENTER_CENTER,
        "+",
        theme::regular(font_size::TITLE),
        theme::fg_on_brand(),
    );
}

impl CallFilter {
    fn includes(self, direction: &str) -> bool {
        match self {
            Self::All => true,
            filter => direction == filter.label(),
        }
    }
}

fn call_history() -> [(&'static str, &'static str, &'static str, &'static str); 10] {
    [
        ("Kiran Doyle", "Missed", "Today", "No answer"),
        ("Dara Whitlock", "Incoming", "Yesterday", "12m 18s"),
        ("Wren Castellanos", "Voicemail", "Friday", "0m 21s"),
        ("Dara Whitlock", "Incoming", "2026. 10. 27.", "6m 03s"),
        ("Kiran Doyle", "Outgoing", "2026. 10. 22.", "14m 51s"),
        ("Dara Whitlock", "Incoming", "2026. 10. 19.", "27m 44s"),
        ("Dara Whitlock", "Outgoing", "2026. 10. 12.", "9m 36s"),
        ("Dara Whitlock", "Incoming", "2026. 10. 12.", "21m 07s"),
        ("Wren Castellanos", "Outgoing", "2026. 10. 02.", "3m 49s"),
        ("Dara Whitlock", "Outgoing", "2026. 09. 24.", "16m 30s"),
    ]
}

fn direction_style(direction: &str) -> (Icon, Color32) {
    // Voicemail has no dedicated glyph vendored; it reuses the plain call icon.
    match direction {
        "Missed" => (Icon::CallMissed, theme::danger()),
        "Incoming" => (Icon::CallInbound, theme::fg3()),
        "Outgoing" => (Icon::CallOutbound, theme::fg3()),
        "Voicemail" => (Icon::Call, theme::brand_fg()),
        _ => (Icon::Call, theme::fg3()),
    }
}

fn draw_call_row(
    ui: &mut egui::Ui,
    name: &str,
    direction: &str,
    when: &str,
    duration: &str,
    action: impl FnOnce(&mut egui::Ui),
) -> egui::Response {
    let (icon, color) = direction_style(direction);
    let background = ui.painter().add(egui::Shape::Noop);
    let row_right = ui.cursor().left() + ui.available_width() - theme::space::XL;
    let response = ui
        .horizontal(|ui| {
            ui.add_space(theme::space::XL);
            Avatar::new(name, AvatarSize::Md32).show(ui);
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(name)
                        .font(theme::semibold(font_size::BODY))
                        .color(theme::fg1()),
                );
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = theme::space::XS;
                    ui.add(icon.image(IconSize::S16).tint(color));
                    ui.label(
                        RichText::new(direction)
                            .font(theme::regular(font_size::CAPTION))
                            .color(color),
                    );
                });
            });
            let remaining = (row_right - ui.cursor().left()).max(0.0);
            ui.allocate_ui_with_layout(
                egui::vec2(remaining, 36.0),
                Layout::right_to_left(Align::Center),
                |ui| {
                    action(ui);
                    ui.with_layout(Layout::top_down(Align::Max), |ui| {
                        ui.label(
                            RichText::new(when)
                                .font(theme::regular(font_size::CAPTION))
                                .color(theme::fg3()),
                        );
                        ui.label(
                            RichText::new(duration)
                                .font(theme::regular(font_size::CAPTION))
                                .color(theme::fg3()),
                        );
                    });
                },
            );
        })
        .response;
    if ui.rect_contains_pointer(response.rect) {
        ui.painter().set(
            background,
            egui::epaint::RectShape::filled(
                response.rect,
                egui::CornerRadius::same(theme::radius::LG),
                theme::surface_hover(),
            ),
        );
    }
    ui.add_space(theme::space::SM);
    clickable(
        ui,
        response,
        &format!("{name}, {direction}, {when}, {duration}"),
    )
}

fn primary_button(icon: Icon, label: &str, enabled: bool) -> egui::Button<'static> {
    let (tint, text, fill) = if enabled {
        (theme::fg_on_brand(), theme::fg_on_brand(), theme::brand())
    } else {
        (
            theme::fg_disabled(),
            theme::fg_disabled(),
            theme::surface_hover(),
        )
    };
    egui::Button::image_and_text(
        icon.image(IconSize::S16).tint(tint),
        RichText::new(label.to_owned()).color(text),
    )
    .fill(fill)
    .corner_radius(theme::radius::MD)
}

fn secondary_button(icon: Icon, label: &str) -> egui::Button<'static> {
    egui::Button::image_and_text(
        icon.image(IconSize::S16).tint(theme::fg2()),
        RichText::new(label.to_owned()).color(theme::fg1()),
    )
    .fill(theme::surface_subtle())
    .stroke(egui::Stroke::new(1.0_f32, theme::stroke_subtle()))
    .corner_radius(theme::radius::MD)
}

/// Rounded filter pill; selected uses the brand tint.
fn filter_chip(ui: &mut egui::Ui, label: &str, selected: bool) -> egui::Response {
    let (fill, text) = if selected {
        (theme::brand(), theme::fg_on_brand())
    } else {
        (theme::surface_subtle(), theme::fg1())
    };
    ui.add(
        egui::Button::selectable(
            selected,
            RichText::new(label.to_owned())
                .font(theme::regular(font_size::CAPTION))
                .color(text),
        )
        .fill(fill)
        .corner_radius(theme::radius::XL),
    )
}

/// "History" header with a real chevron icon; clicking toggles the section.
fn history_toggle(ui: &mut egui::Ui, open: bool) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(
        "History".to_owned(),
        theme::semibold(font_size::BODY),
        theme::fg1(),
    );
    let side = IconSize::S16.px();
    let size = egui::vec2(
        side + theme::space::SM + galley.size().x + theme::space::MD,
        28.0,
    );
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), "History")
    });
    if response.hovered() {
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(theme::radius::SM),
            theme::surface_subtle(),
        );
    }
    let chevron = if open {
        Icon::ChevronDown
    } else {
        Icon::ChevronRight
    };
    chevron.image(IconSize::S16).tint(theme::fg3()).paint_at(
        ui,
        egui::Rect::from_min_size(
            egui::pos2(rect.left(), rect.center().y - side / 2.0),
            egui::vec2(side, side),
        ),
    );
    ui.painter().galley(
        egui::pos2(
            rect.left() + side + theme::space::SM,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        theme::fg1(),
    );
    response
}

/// Dropdown trigger: leading icon, label and trailing chevron. Hosts a menu
/// via `egui::Popup::menu(&response)`.
fn dropdown_button(ui: &mut egui::Ui, icon: Icon, label: &str) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(
        label.to_owned(),
        theme::regular(font_size::CAPTION),
        theme::fg1(),
    );
    let side = IconSize::S16.px();
    let pad = theme::space::MD;
    let gap = theme::space::SM;
    let size = egui::vec2(pad * 2.0 + side * 2.0 + gap * 2.0 + galley.size().x, 32.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    let fill = if response.hovered() {
        theme::surface_hover()
    } else {
        theme::surface_subtle()
    };
    ui.painter().rect(
        rect,
        egui::CornerRadius::same(theme::radius::MD),
        fill,
        egui::Stroke::new(1.0_f32, theme::stroke_subtle()),
        egui::StrokeKind::Inside,
    );
    let icon_rect = |x: f32| {
        egui::Rect::from_min_size(
            egui::pos2(x, rect.center().y - side / 2.0),
            egui::vec2(side, side),
        )
    };
    icon.image(IconSize::S16)
        .tint(theme::fg2())
        .paint_at(ui, icon_rect(rect.left() + pad));
    ui.painter().galley(
        egui::pos2(
            rect.left() + pad + side + gap,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        theme::fg1(),
    );
    Icon::ChevronDown
        .image(IconSize::S16)
        .tint(theme::fg3())
        .paint_at(ui, icon_rect(rect.right() - pad - side));
    response
}

#[cfg(test)]
mod tests;
