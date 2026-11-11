//! Command-line entry point shared by the `teamspace` binary.

#[cfg(any(test, feature = "screenshot"))]
use crate::shared::model::Section;
#[cfg(feature = "screenshot")]
use std::path::PathBuf;

#[cfg(feature = "screenshot")]
const DEFAULT_SCREENSHOT_PATH: &str = "artifacts/latest.png";

/// Runs the desktop app (or the `--screenshot` renderer) and returns its exit result.
pub fn main_entry() -> eframe::Result {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|argument| argument == "--screenshot") {
        run_screenshot(&args);
    }
    crate::app::run()
}

#[cfg(feature = "screenshot")]
fn run_screenshot(args: &[String]) -> ! {
    let section = match parse_screenshot_section(args) {
        Ok(section) => section,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };
    match crate::app::screenshot::render_screenshot(
        section,
        &PathBuf::from(DEFAULT_SCREENSHOT_PATH),
    ) {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            eprintln!("screenshot failed: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(feature = "screenshot"))]
fn run_screenshot(_args: &[String]) -> ! {
    eprintln!(
        "--screenshot requires: cargo run --features screenshot -- --screenshot [activity|chat|teams|calendar|calls|files]"
    );
    std::process::exit(2);
}

/// Parses `--screenshot [activity|chat|teams|calendar|calls|files]`; the section defaults to chat.
#[cfg(any(test, feature = "screenshot"))]
fn parse_screenshot_section(args: &[String]) -> Result<Section, String> {
    let position = args
        .iter()
        .position(|argument| argument == "--screenshot")
        .ok_or_else(|| "missing --screenshot".to_owned())?;
    match args.get(position + 1).map(String::as_str) {
        None | Some("chat") => Ok(Section::Chat),
        Some("teams") => Ok(Section::Teams),
        Some("calendar") => Ok(Section::Calendar),
        Some("activity") => Ok(Section::Activity),
        Some("calls") => Ok(Section::Calls),
        Some("files") => Ok(Section::Files),
        Some(other) => Err(format!(
            "unknown screenshot section `{other}`; use activity, chat, teams, calendar, calls, or files"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_screenshot_section;
    use crate::shared::model::Section;

    fn parse(args: &[&str]) -> Result<Section, String> {
        parse_screenshot_section(&args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn screenshot_defaults_to_the_chat_section() {
        assert_eq!(parse(&["--screenshot"]), Ok(Section::Chat));
    }

    #[test]
    fn screenshot_selects_any_section() {
        assert_eq!(parse(&["--screenshot", "chat"]), Ok(Section::Chat));
        assert_eq!(parse(&["--screenshot", "teams"]), Ok(Section::Teams));
        assert_eq!(parse(&["--screenshot", "calendar"]), Ok(Section::Calendar));
        assert_eq!(parse(&["--screenshot", "activity"]), Ok(Section::Activity));
        assert_eq!(parse(&["--screenshot", "calls"]), Ok(Section::Calls));
        assert_eq!(parse(&["--screenshot", "files"]), Ok(Section::Files));
    }

    #[test]
    fn screenshot_rejects_unknown_sections() {
        assert!(parse(&["--screenshot", "nonsense"]).is_err());
    }
}
