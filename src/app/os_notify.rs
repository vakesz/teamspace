//! Optional system notifications, behind the `os-notifications` feature and
//! the user's separate `os_notifications_enabled` choice (default off).
//!
//! An OS notification names the app that raised it, so it must never be
//! raised under an identity that is not ours. The gate below allows exactly:
//!
//! - Linux: the application name `Teamspace`;
//! - macOS: only when the process runs from a bundle whose
//!   `CFBundleIdentifier` is [`BUNDLE_ID`] (a bundled `.app`; the release
//!   phase produces it). Run from `cargo run` the id is not ours, so the
//!   notification would be attributed to another app and is refused;
//! - Windows: only when [`AUMID`] is registered under
//!   `HKCU\Software\Classes\AppUserModelId`, because an unregistered id
//!   shows up under another app's name.
//!
//! Anywhere else, and in a build without the feature, no notifier exists and
//! the in-app badge and toast are all the user gets. The text is the same
//! count-only text as the in-app toast; a message body is never passed in.
//! Delivery runs on its own thread behind a small bounded queue, so the UI
//! thread never waits on the OS and a stalled notification centre cannot
//! build up work.

// The gate and queue are compiled (and tested) in every build, but only the
// `os-notifications` feature starts a notifier; without it they are unused.
#![cfg_attr(not(feature = "os-notifications"), allow(dead_code))]

use std::path::Path;
use std::sync::mpsc::{self, SyncSender, TrySendError};

/// The application name shown by Linux notification daemons.
pub(crate) const APP_NAME: &str = "Teamspace";
/// The bundle identifier the release phase gives the macOS `.app`.
pub(crate) const BUNDLE_ID: &str = "io.github.vakesz.teamspace";
/// The Windows AppUserModelID the release phase registers.
pub(crate) const AUMID: &str = "io.github.vakesz.teamspace";

/// At most this many notifications wait for the OS; more are dropped.
const QUEUE_LIMIT: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Os {
    Linux,
    Mac,
    Windows,
    Other,
}

/// The identity a notification may be raised under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Identity {
    Appname(&'static str),
    Bundle(String),
    Aumid(&'static str),
}

/// What the gate needs to know about the running process. Injectable so the
/// decision is testable on any machine.
pub(super) trait Platform {
    fn os(&self) -> Os;
    /// The `CFBundleIdentifier` of the bundle this process runs from.
    fn main_bundle_id(&self) -> Option<String>;
    fn aumid_registered(&self, aumid: &str) -> bool;
}

/// `None` means "do not raise OS notifications".
pub(super) fn truthful_identity(platform: &impl Platform) -> Option<Identity> {
    match platform.os() {
        Os::Linux => Some(Identity::Appname(APP_NAME)),
        Os::Mac => platform
            .main_bundle_id()
            .filter(|id| id == BUNDLE_ID)
            .map(Identity::Bundle),
        Os::Windows => platform
            .aumid_registered(AUMID)
            .then_some(Identity::Aumid(AUMID)),
        Os::Other => None,
    }
}

/// The real process. Only the cheap, local checks the gate needs.
pub(super) struct SystemPlatform;

impl Platform for SystemPlatform {
    fn os(&self) -> Os {
        if cfg!(target_os = "linux") {
            Os::Linux
        } else if cfg!(target_os = "macos") {
            Os::Mac
        } else if cfg!(target_os = "windows") {
            Os::Windows
        } else {
            Os::Other
        }
    }

    fn main_bundle_id(&self) -> Option<String> {
        let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
        bundle_id_for_executable(&exe)
    }

    fn aumid_registered(&self, aumid: &str) -> bool {
        // Fixed arguments, no shell: the documented registration location.
        std::process::Command::new("reg")
            .args([
                "query",
                &format!(r"HKCU\Software\Classes\AppUserModelId\{aumid}"),
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}

/// `Foo.app/Contents/MacOS/foo` is bundled; its `Contents/Info.plist` names
/// the bundle. Anything else (a bare binary) has no bundle id.
pub(super) fn bundle_id_for_executable(exe: &Path) -> Option<String> {
    let macos_dir = exe.parent()?;
    let contents = macos_dir.parent()?;
    if macos_dir.file_name()? != "MacOS" || contents.file_name()? != "Contents" {
        return None;
    }
    let plist = std::fs::read_to_string(contents.join("Info.plist")).ok()?;
    bundle_id_from_plist(&plist)
}

/// The `CFBundleIdentifier` string of an XML property list.
pub(super) fn bundle_id_from_plist(plist: &str) -> Option<String> {
    const KEY: &str = "<key>CFBundleIdentifier</key>";
    let after = &plist[plist.find(KEY)? + KEY.len()..];
    let after = after.trim_start();
    let value = after.strip_prefix("<string>")?;
    let id = value[..value.find("</string>")?].trim();
    (!id.is_empty() && id.len() <= 255).then(|| id.to_owned())
}

/// What happened to one request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NotifyOutcome {
    Queued,
    /// The queue was full; the notification is dropped, never retried.
    Dropped,
    /// No truthful notifier in this build or on this machine.
    Unavailable,
}

pub(crate) struct OsNotifier {
    sender: Option<SyncSender<String>>,
}

impl OsNotifier {
    /// No OS notifications (the default, and every build without the feature).
    pub(crate) const fn disabled() -> Self {
        Self { sender: None }
    }

    /// Starts the system notifier when this build has one and the process has
    /// a truthful identity; otherwise returns a disabled notifier.
    pub(crate) fn start() -> Self {
        #[cfg(feature = "os-notifications")]
        {
            Self::start_with(&SystemPlatform, system_backend)
        }
        #[cfg(not(feature = "os-notifications"))]
        {
            Self::disabled()
        }
    }

    /// The gate plus a backend factory. The factory runs on the notifier
    /// thread and returns the function that shows one text.
    pub(super) fn start_with<F, B>(platform: &impl Platform, make_backend: F) -> Self
    where
        F: FnOnce(Identity) -> B + Send + 'static,
        B: FnMut(&str) + 'static,
    {
        let Some(identity) = truthful_identity(platform) else {
            return Self::disabled();
        };
        let (sender, receiver) = mpsc::sync_channel::<String>(QUEUE_LIMIT);
        let spawned = std::thread::Builder::new()
            .name("teamspace-notifier".into())
            .spawn(move || {
                let mut show = make_backend(identity);
                // Ends when the app drops the sender.
                while let Ok(text) = receiver.recv() {
                    show(&text);
                }
            });
        match spawned {
            Ok(_) => Self {
                sender: Some(sender),
            },
            Err(_) => Self::disabled(),
        }
    }

    /// Whether a truthful notifier is running, so the choice can be offered.
    pub(crate) const fn is_available(&self) -> bool {
        self.sender.is_some()
    }

    /// Queues one count-only text. Never blocks the caller.
    pub(crate) fn notify(&self, text: &str) -> NotifyOutcome {
        let Some(sender) = &self.sender else {
            return NotifyOutcome::Unavailable;
        };
        match sender.try_send(text.to_owned()) {
            Ok(()) => NotifyOutcome::Queued,
            Err(TrySendError::Full(_)) => NotifyOutcome::Dropped,
            Err(TrySendError::Disconnected(_)) => NotifyOutcome::Unavailable,
        }
    }
}

/// Shows one notification through the platform's backend. A failure is
/// ignored: a notification is best effort and never worth interrupting the app.
#[cfg(feature = "os-notifications")]
fn system_backend(identity: Identity) -> impl FnMut(&str) {
    #[cfg(target_os = "macos")]
    if let Identity::Bundle(id) = &identity {
        // Attribute notifications to our own bundle, not a default app.
        let _ = notify_rust::set_application(id);
    }
    move |text: &str| {
        let mut notification = notify_rust::Notification::new();
        notification.summary(APP_NAME).body(text);
        match &identity {
            Identity::Appname(name) => {
                notification.appname(name);
            }
            #[cfg(target_os = "windows")]
            Identity::Aumid(aumid) => {
                notification.app_id(aumid);
            }
            _ => {}
        }
        let _ = notification.show();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    struct Fake {
        os: Os,
        bundle: Option<&'static str>,
        registered: bool,
    }

    impl Platform for Fake {
        fn os(&self) -> Os {
            self.os
        }

        fn main_bundle_id(&self) -> Option<String> {
            self.bundle.map(str::to_owned)
        }

        fn aumid_registered(&self, aumid: &str) -> bool {
            self.registered && aumid == AUMID
        }
    }

    fn fake(os: Os, bundle: Option<&'static str>, registered: bool) -> Fake {
        Fake {
            os,
            bundle,
            registered,
        }
    }

    #[test]
    fn the_identity_gate_is_truthful_per_platform() {
        assert_eq!(
            truthful_identity(&fake(Os::Linux, None, false)),
            Some(Identity::Appname("Teamspace"))
        );
        assert_eq!(
            truthful_identity(&fake(Os::Mac, Some(BUNDLE_ID), false)),
            Some(Identity::Bundle(BUNDLE_ID.into()))
        );
        for bundle in [
            None,
            Some("com.apple.Terminal"),
            Some("io.github.vakesz.teamspace.evil"),
            Some(""),
        ] {
            assert_eq!(
                truthful_identity(&fake(Os::Mac, bundle, true)),
                None,
                "{bundle:?}"
            );
        }
        assert_eq!(
            truthful_identity(&fake(Os::Windows, None, true)),
            Some(Identity::Aumid(AUMID))
        );
        assert_eq!(
            truthful_identity(&fake(Os::Windows, Some(BUNDLE_ID), false)),
            None
        );
        assert_eq!(
            truthful_identity(&fake(Os::Other, Some(BUNDLE_ID), true)),
            None
        );
    }

    #[test]
    fn plist_parsing_reads_only_the_bundle_identifier() {
        let plist = r#"<?xml version="1.0"?><plist><dict>
            <key>CFBundleName</key><string>Teamspace</string>
            <key>CFBundleIdentifier</key>
            <string> io.github.vakesz.teamspace </string>
        </dict></plist>"#;
        assert_eq!(bundle_id_from_plist(plist).as_deref(), Some(BUNDLE_ID));
        for bad in [
            "",
            "<key>CFBundleName</key><string>x</string>",
            "<key>CFBundleIdentifier</key><string></string>",
            "<key>CFBundleIdentifier</key><integer>1</integer>",
            "<key>CFBundleIdentifier</key><string>unterminated",
        ] {
            assert_eq!(bundle_id_from_plist(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn only_an_executable_inside_an_app_bundle_has_a_bundle_id() {
        let directory = crate::testing::TemporaryDirectory::new("bundle");
        let app = directory.path().join("Teamspace.app").join("Contents");
        std::fs::create_dir_all(app.join("MacOS")).expect("dirs");
        std::fs::write(
            app.join("Info.plist"),
            format!("<key>CFBundleIdentifier</key><string>{BUNDLE_ID}</string>"),
        )
        .expect("plist");
        assert_eq!(
            bundle_id_for_executable(&app.join("MacOS").join("teamspace")).as_deref(),
            Some(BUNDLE_ID)
        );
        // A bare binary, or a binary in some other folder layout, has none.
        let bare = directory
            .path()
            .join("target")
            .join("debug")
            .join("teamspace");
        assert_eq!(bundle_id_for_executable(&bare), None);
        assert_eq!(
            bundle_id_for_executable(&app.join("Resources").join("teamspace")),
            None
        );
    }

    type Shown = Arc<Mutex<Vec<String>>>;
    type Backend = Box<dyn FnMut(&str)>;

    fn recorder() -> (Shown, impl FnOnce(Identity) -> Backend) {
        let shown = Shown::default();
        let sink = Arc::clone(&shown);
        (shown, move |_| {
            Box::new(move |text: &str| sink.lock().expect("sink").push(text.to_owned()))
        })
    }

    fn wait_for(shown: &Arc<Mutex<Vec<String>>>, count: usize) {
        for _ in 0..2000 {
            if shown.lock().expect("shown").len() >= count {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("the notifier thread did not deliver");
    }

    #[test]
    fn a_refused_identity_never_starts_a_notifier() {
        let (shown, backend) = recorder();
        let notifier = OsNotifier::start_with(&fake(Os::Mac, None, false), backend);
        assert!(!notifier.is_available());
        assert_eq!(
            notifier.notify("New Microsoft 365 message"),
            NotifyOutcome::Unavailable
        );
        std::thread::sleep(Duration::from_millis(20));
        assert!(shown.lock().expect("shown").is_empty());
    }

    #[test]
    fn the_default_notifier_is_unavailable() {
        let notifier = OsNotifier::disabled();
        assert!(!notifier.is_available());
        assert_eq!(notifier.notify("x"), NotifyOutcome::Unavailable);
    }

    #[test]
    fn a_truthful_notifier_delivers_exactly_the_given_text_off_thread() {
        let (shown, backend) = recorder();
        let notifier = OsNotifier::start_with(&fake(Os::Linux, None, false), backend);
        assert!(notifier.is_available());
        assert_eq!(
            notifier.notify("3 new Microsoft 365 messages"),
            NotifyOutcome::Queued
        );
        wait_for(&shown, 1);
        assert_eq!(
            *shown.lock().expect("shown"),
            ["3 new Microsoft 365 messages"]
        );
    }

    #[test]
    fn the_queue_is_bounded_and_a_stalled_backend_never_blocks_the_caller() {
        let (release, gate) = mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let notifier = OsNotifier::start_with(&fake(Os::Linux, None, false), move |_| {
            move |_: &str| {
                let _ = gate.lock().expect("gate").recv();
            }
        });
        let mut dropped = 0;
        for _ in 0..(QUEUE_LIMIT * 3) {
            if notifier.notify("x") == NotifyOutcome::Dropped {
                dropped += 1;
            }
        }
        assert!(dropped >= QUEUE_LIMIT, "overflow is dropped, not queued");
        drop(release);
    }
}
