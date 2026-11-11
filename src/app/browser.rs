//! Narrow, explicitly invoked system-browser handoff.
//!
//! Feature modules never launch a browser. They may only request an opaque,
//! already-normalized destination after a direct user gesture; this module
//! revalidates the small HTTPS boundary immediately before invoking the host.

use std::fmt;
use std::io;
use std::process::Command;
use url::Url;

const MAX_EXTERNAL_HTTPS_URL_BYTES: usize = 8 * 1024;

/// A category-only failure for a system-browser handoff.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BrowserOpenError {
    InvalidHttpsDestination,
    Unavailable,
}

impl fmt::Display for BrowserOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidHttpsDestination => {
                formatter.write_str("the supplied Microsoft 365 link was not a safe HTTPS URL")
            }
            Self::Unavailable => formatter.write_str("the system browser could not be opened"),
        }
    }
}

/// Revalidates and opens one explicit HTTPS destination in the system browser.
///
/// The URL itself is deliberately never included in a failure. The integration
/// adapter normalizes provider links earlier, but this last boundary protects
/// against any future caller accidentally passing arbitrary display text.
pub(crate) fn open_https_url(value: &str) -> Result<(), BrowserOpenError> {
    open_https_url_with(value, open_system_browser)
}

/// Opens a URL that a trusted app-owned flow has already constructed (such as
/// the OAuth authorization URL). External provider text must use
/// `open_https_url` instead.
pub(crate) fn open_system_browser(url: &Url) -> Result<(), BrowserOpenError> {
    launch_system_browser(url).map_err(|_| BrowserOpenError::Unavailable)
}

fn open_https_url_with(
    value: &str,
    launch: impl FnOnce(&Url) -> Result<(), BrowserOpenError>,
) -> Result<(), BrowserOpenError> {
    let url = validated_https_url(value).ok_or(BrowserOpenError::InvalidHttpsDestination)?;
    launch(&url)
}

fn validated_https_url(value: &str) -> Option<Url> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.len() > MAX_EXTERNAL_HTTPS_URL_BYTES
        || value.chars().any(char::is_control)
    {
        return None;
    }
    // `url::Url` is intentionally forgiving for special schemes and can
    // normalize `https:/host` into an authority URL. An external handoff must
    // preserve the stricter wire shape we accept from Graph: `https://` plus
    // a non-slash authority prefix before parsing it.
    let scheme_end = trimmed.find(':')?;
    let authority = trimmed.get(scheme_end + 1..)?;
    if !trimmed[..scheme_end].eq_ignore_ascii_case("https")
        || !authority.starts_with("//")
        || authority.get(2..).is_none_or(|rest| rest.starts_with('/'))
    {
        return None;
    }
    let url = Url::parse(trimmed).ok()?;
    (url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

/// Environment variable the Graph integration may read a token from. Spawned
/// launchers must never inherit it.
const GRAPH_TOKEN_ENVIRONMENT_VARIABLE: &str = "TEAMSPACE_GRAPH_ACCESS_TOKEN";

fn launcher_command(url: &Url) -> Option<Command> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg(url.as_str());
        command
    };

    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("rundll32");
        command.args(["url.dll,FileProtocolHandler", url.as_str()]);
        command
    };

    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(url.as_str());
        command
    };

    #[cfg(not(any(target_os = "macos", target_os = "windows", unix)))]
    {
        let _ = url;
        return None;
    }

    #[cfg(any(target_os = "macos", target_os = "windows", unix))]
    {
        command.env_remove(GRAPH_TOKEN_ENVIRONMENT_VARIABLE);
        Some(command)
    }
}

fn launch_system_browser(url: &Url) -> io::Result<()> {
    let Some(mut command) = launcher_command(url) else {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no system-browser launcher is configured for this platform",
        ));
    };
    command.spawn().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::{BrowserOpenError, open_https_url_with, validated_https_url};

    #[cfg(any(target_os = "macos", target_os = "windows", unix))]
    #[test]
    fn launcher_removes_the_graph_token_from_the_child_environment() {
        let url = validated_https_url("https://teams.microsoft.com/l/meetup-join/example")
            .expect("plain HTTPS meeting URL must be accepted");
        let command = super::launcher_command(&url).expect("launcher must exist");
        let removed = command
            .get_envs()
            .find(|(key, _)| *key == "TEAMSPACE_GRAPH_ACCESS_TOKEN");
        assert_eq!(
            removed,
            Some((std::ffi::OsStr::new("TEAMSPACE_GRAPH_ACCESS_TOKEN"), None))
        );
    }

    #[test]
    fn accepts_a_plain_https_destination_without_credentials() {
        let url = validated_https_url("https://teams.microsoft.com/l/meetup-join/example")
            .expect("plain HTTPS meeting URL must be accepted");
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("teams.microsoft.com"));
    }

    #[test]
    fn rejects_non_https_credentials_control_bytes_and_hostless_urls() {
        for value in [
            "http://teams.microsoft.com/l/meetup-join/example",
            "https://user:pass@teams.microsoft.com/l/meetup-join/example",
            "https://teams.microsoft.com/l/meetup-join/example\nnext",
            "https:/missing-host",
            "https:////missing-host",
        ] {
            assert!(
                validated_https_url(value).is_none(),
                "{value:?} must be rejected"
            );
        }
    }

    #[test]
    fn opening_revalidates_before_the_system_launcher() {
        let mut launch_called = false;
        let error = open_https_url_with("http://example.test", |_| {
            launch_called = true;
            Ok(())
        })
        .expect_err("non-HTTPS URLs must not launch");
        assert_eq!(error, BrowserOpenError::InvalidHttpsDestination);
        assert!(!launch_called);
    }

    #[test]
    fn launcher_failure_is_kept_category_only() {
        let error =
            open_https_url_with("https://teams.microsoft.com/l/meetup-join/example", |_| {
                Err(BrowserOpenError::Unavailable)
            })
            .expect_err("failed browser launch must surface a category only");
        assert_eq!(error, BrowserOpenError::Unavailable);
    }
}
