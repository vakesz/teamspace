//! Platform-owned locations for local, profile-scoped application data.
//!
//! Features own their snapshots and serialization. This module owns only the
//! stable location and opaque profile directory shared by those snapshots.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

pub(crate) fn profile_file(profile: &str, file_name: &str) -> Option<PathBuf> {
    profile_file_in(&local_state_directory()?, profile, file_name)
}

/// Pure path construction with an injectable base directory. `file_name` must
/// be exactly one normal path component: `.`, `..`, separators, absolute or
/// prefixed names and the empty string are rejected.
fn profile_file_in(base: &Path, profile: &str, file_name: &str) -> Option<PathBuf> {
    let mut components = Path::new(file_name).components();
    let file_name = match (components.next(), components.next()) {
        (Some(Component::Normal(name)), None) => name,
        _ => return None,
    };
    Some(
        base.join("teamspace")
            .join("profiles")
            .join(profile_directory_name(profile))
            .join(file_name),
    )
}

#[cfg(test)]
fn local_state_directory() -> Option<PathBuf> {
    // Tests never resolve (or write to) a real user profile directory.
    Some(std::env::temp_dir().join(format!("teamspace-test-state-{}", std::process::id())))
}

#[cfg(not(test))]
fn local_state_directory() -> Option<PathBuf> {
    state_directory_from(&|name| std::env::var_os(name))
}

/// Looks up an environment variable; injected so resolution is testable.
type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

/// An absolute, non-empty path from `name`; relative values are ignored.
fn absolute_var(env: Env<'_>, name: &str) -> Option<PathBuf> {
    env(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// Directory for local profile state. `XDG_STATE_HOME` wins on every OS;
/// otherwise `%LOCALAPPDATA%` on Windows, `~/Library/Application Support` on
/// macOS and `~/.local/state` elsewhere.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn state_directory_from(env: Env<'_>) -> Option<PathBuf> {
    if let Some(directory) = absolute_var(env, "XDG_STATE_HOME") {
        return Some(directory);
    }
    #[cfg(windows)]
    {
        absolute_var(env, "LOCALAPPDATA")
    }
    #[cfg(target_os = "macos")]
    {
        Some(
            absolute_var(env, "HOME")?
                .join("Library")
                .join("Application Support"),
        )
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Some(absolute_var(env, "HOME")?.join(".local").join("state"))
    }
}

/// Directory for configuration such as the session file. `XDG_CONFIG_HOME`
/// wins on every OS; otherwise `%LOCALAPPDATA%` on Windows and `~/.config`
/// elsewhere (macOS included, to keep existing files in place).
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn config_directory_from(env: Env<'_>) -> Option<PathBuf> {
    if let Some(directory) = absolute_var(env, "XDG_CONFIG_HOME") {
        return Some(directory);
    }
    #[cfg(windows)]
    {
        absolute_var(env, "LOCALAPPDATA")
    }
    #[cfg(not(windows))]
    {
        Some(absolute_var(env, "HOME")?.join(".config"))
    }
}

fn profile_directory_name(profile: &str) -> String {
    let profile = profile.trim().to_lowercase();
    if profile.is_empty() {
        return "local-preview".into();
    }

    // This deterministic identifier separates local-preview accounts without
    // placing an email-like label directly in the filesystem. It is an
    // identifier, not a security primitive.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in profile.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("profile-{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::{
        OsString, PathBuf, config_directory_from, profile_directory_name, profile_file,
        profile_file_in, state_directory_from,
    };

    fn fake_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let map: std::collections::HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), OsString::from(*v)))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn xdg_variables_win_and_relative_or_empty_values_are_ignored() {
        let root = std::env::temp_dir();
        let root = root.to_str().expect("utf-8 temp dir");
        let env = fake_env(&[("XDG_STATE_HOME", root), ("XDG_CONFIG_HOME", root)]);
        assert_eq!(state_directory_from(&env), Some(PathBuf::from(root)));
        assert_eq!(config_directory_from(&env), Some(PathBuf::from(root)));

        let env = fake_env(&[("XDG_STATE_HOME", "relative"), ("XDG_CONFIG_HOME", "")]);
        assert_ne!(state_directory_from(&env), Some(PathBuf::from("relative")));
        assert_ne!(config_directory_from(&env), Some(PathBuf::from("")));
    }

    #[test]
    fn missing_environment_resolves_nothing() {
        let env = fake_env(&[]);
        assert_eq!(state_directory_from(&env), None);
        assert_eq!(config_directory_from(&env), None);
    }

    #[cfg(unix)]
    #[test]
    fn unix_paths_are_derived_from_home() {
        let env = fake_env(&[("HOME", "/home/u")]);
        #[cfg(target_os = "macos")]
        assert_eq!(
            state_directory_from(&env),
            Some(PathBuf::from("/home/u/Library/Application Support"))
        );
        #[cfg(not(target_os = "macos"))]
        assert_eq!(
            state_directory_from(&env),
            Some(PathBuf::from("/home/u/.local/state"))
        );
        assert_eq!(
            config_directory_from(&env),
            Some(PathBuf::from("/home/u/.config"))
        );
        assert_eq!(state_directory_from(&fake_env(&[("HOME", "")])), None);
        assert_eq!(config_directory_from(&fake_env(&[("HOME", "rel")])), None);
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_use_localappdata() {
        let env = fake_env(&[
            ("LOCALAPPDATA", r"C:\Users\u\AppData\Local"),
            ("HOME", r"C:\h"),
        ]);
        let expected = Some(PathBuf::from(r"C:\Users\u\AppData\Local"));
        assert_eq!(state_directory_from(&env), expected);
        assert_eq!(config_directory_from(&env), expected);
    }
    use std::path::Path;

    #[test]
    fn profile_directory_is_stable_and_case_insensitive_for_local_accounts() {
        assert_eq!(
            profile_directory_name(" Alex@Example.com "),
            profile_directory_name("alex@example.com")
        );
        assert_ne!(
            profile_directory_name("alex@example.com"),
            profile_directory_name("avery@example.com")
        );
        assert_eq!(profile_directory_name(""), "local-preview");
    }

    #[test]
    fn profile_files_accept_only_a_single_file_name() {
        assert!(profile_file("local-preview", "chat-v1.json").is_some());
        assert!(profile_file("local-preview", "nested/chat-v1.json").is_none());
        assert!(profile_file("local-preview", "").is_none());
    }

    #[test]
    fn profile_files_require_exactly_one_normal_component() {
        let base = Path::new("base");
        for rejected in [
            ".",
            "..",
            "./",
            "../x",
            "a/..",
            "a/b",
            "/abs",
            "",
            "./chat.json",
        ] {
            assert!(
                profile_file_in(base, "p", rejected).is_none(),
                "{rejected:?} must be rejected"
            );
        }
        let accepted = profile_file_in(base, "p", "chat-v1.json").expect("plain name");
        assert!(accepted.starts_with(base.join("teamspace").join("profiles")));
        assert!(accepted.ends_with("chat-v1.json"));
    }

    #[test]
    fn tests_never_resolve_a_real_profile_directory() {
        let path = profile_file("alex@example.com", "chat-v1.json").expect("test path");
        assert!(path.starts_with(std::env::temp_dir()));
    }
}
