//! Session capabilities that depend on optional delegated scopes and on the
//! signed-in Graph cloud. They are computed by the adapter and only ever
//! reach feature code as these presentation-safe categories.

use crate::integrations::cloud::GraphCloud;

/// The optional scope that gates editing and deleting own chat messages.
const CHAT_READ_WRITE_SCOPE: &str = "Chat.ReadWrite";

/// The optional scope that gates People-panel presence.
const PRESENCE_READ_ALL_SCOPE: &str = "Presence.Read.All";

/// The optional scope that gates attaching a file to a chat message. The
/// documented higher scope `Files.ReadWrite.All` is accepted too.
const FILES_READ_WRITE_SCOPE: &str = "Files.ReadWrite";
const FILES_READ_WRITE_ALL_SCOPE: &str = "Files.ReadWrite.All";

/// Whether one optional action is expected to work in this session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Capability {
    Available,
    /// The scope was not granted, or Graph answered `403` to the action.
    NotGranted,
    UnsupportedInCloud,
    /// A token whose scopes are not known (a development token). The action
    /// is offered and degrades to `NotGranted` on a `403`.
    Unknown,
}

impl Capability {
    /// Whether the UI may offer the action.
    pub(crate) const fn is_offered(self) -> bool {
        matches!(self, Self::Available | Self::Unknown)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RemoteCapabilities {
    pub(crate) chat_message_edit: Capability,
    pub(crate) chat_message_delete: Capability,
    pub(crate) member_presence: Capability,
    /// Opt-in read receipts (`markChatReadForUser`). Needs `Chat.ReadWrite`;
    /// available in every cloud.
    pub(crate) chat_mark_read: Capability,
    /// Attaching a file: an upload to the user's OneDrive (`Files.ReadWrite`,
    /// or the documented higher `Files.ReadWrite.All`), referenced from the
    /// message. Available in every cloud.
    pub(crate) file_attach: Capability,
}

impl Default for RemoteCapabilities {
    fn default() -> Self {
        Self {
            chat_message_edit: Capability::Unknown,
            chat_message_delete: Capability::Unknown,
            member_presence: Capability::Unknown,
            chat_mark_read: Capability::Unknown,
            file_attach: Capability::Unknown,
        }
    }
}

impl RemoteCapabilities {
    /// Computes capabilities from the cloud and the scope Entra reported.
    pub(in crate::integrations) fn compute(cloud: GraphCloud, granted_scope: Option<&str>) -> Self {
        let scoped = |name: &str| match granted_scope {
            None => Capability::Unknown,
            Some(granted) if grants(cloud, granted, name) => Capability::Available,
            Some(_) => Capability::NotGranted,
        };
        let scope = scoped(CHAT_READ_WRITE_SCOPE);
        let file_attach = match granted_scope {
            None => Capability::Unknown,
            Some(granted)
                if grants(cloud, granted, FILES_READ_WRITE_SCOPE)
                    || grants(cloud, granted, FILES_READ_WRITE_ALL_SCOPE) =>
            {
                Capability::Available
            }
            Some(_) => Capability::NotGranted,
        };
        Self {
            file_attach,
            chat_message_edit: if cloud.supports_chat_message_update() {
                scope
            } else {
                Capability::UnsupportedInCloud
            },
            chat_message_delete: scope,
            chat_mark_read: scope,
            member_presence: if cloud.supports_presence() {
                scoped(PRESENCE_READ_ALL_SCOPE)
            } else {
                Capability::UnsupportedInCloud
            },
        }
    }
}

/// Case-insensitive match of a scope name, bare or qualified with exactly this
/// cloud's own resource, `https://{graph host}/`. A scope qualified with any
/// other resource (another cloud's host, a look-alike host, another path) does
/// not grant: it is for a different API.
fn grants(cloud: GraphCloud, granted: &str, wanted: &str) -> bool {
    let own_prefix = format!("https://{}/", cloud.graph_host());
    granted.split_whitespace().any(|scope| {
        let name = if scope.contains('/') {
            match scope.strip_prefix(own_prefix.as_str()) {
                Some(name) => name,
                None => return false,
            }
        } else {
            scope
        };
        !name.contains('/') && name.eq_ignore_ascii_case(wanted)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_matrix_per_cloud() {
        let granted = "openid User.Read chat.readwrite";
        let qualified = "User.Read https://graph.microsoft.us/Chat.ReadWrite";
        let missing = "User.Read Chat.Read ChatMessage.Send";
        let global = RemoteCapabilities::compute(GraphCloud::Global, Some(granted));
        assert_eq!(global.chat_message_edit, Capability::Available);
        assert_eq!(global.chat_message_delete, Capability::Available);
        let gov = RemoteCapabilities::compute(GraphCloud::UsGov, Some(qualified));
        assert_eq!(gov.chat_message_edit, Capability::UnsupportedInCloud);
        assert_eq!(gov.chat_message_delete, Capability::Available);
        let denied = RemoteCapabilities::compute(GraphCloud::Global, Some(missing));
        assert_eq!(denied.chat_message_edit, Capability::NotGranted);
        assert_eq!(denied.chat_message_delete, Capability::NotGranted);
        let dev = RemoteCapabilities::compute(GraphCloud::Global, None);
        assert_eq!(dev, RemoteCapabilities::default());
        let china = RemoteCapabilities::compute(GraphCloud::China, None);
        assert_eq!(china.chat_message_edit, Capability::UnsupportedInCloud);
        assert_eq!(china.chat_message_delete, Capability::Unknown);
    }

    #[test]
    fn mark_read_follows_chat_read_write_in_every_cloud() {
        for cloud in [
            GraphCloud::Global,
            GraphCloud::UsGov,
            GraphCloud::UsGovDod,
            GraphCloud::China,
        ] {
            let granted = RemoteCapabilities::compute(cloud, Some("User.Read chat.readwrite"));
            assert_eq!(granted.chat_mark_read, Capability::Available, "{cloud:?}");
            let missing = RemoteCapabilities::compute(cloud, Some("User.Read Chat.Read"));
            assert_eq!(missing.chat_mark_read, Capability::NotGranted, "{cloud:?}");
            let dev = RemoteCapabilities::compute(cloud, None);
            assert_eq!(dev.chat_mark_read, Capability::Unknown, "{cloud:?}");
        }
    }

    #[test]
    fn file_attach_needs_files_read_write_or_the_documented_higher_scope() {
        fn cap(cloud: GraphCloud, scope: Option<&str>) -> Capability {
            RemoteCapabilities::compute(cloud, scope).file_attach
        }
        for cloud in GraphCloud::ALL {
            assert_eq!(
                cap(cloud, Some("User.Read files.readwrite")),
                Capability::Available,
                "{cloud:?}"
            );
            let higher = format!("https://{}/Files.ReadWrite.All", cloud.graph_host());
            assert_eq!(
                cap(cloud, Some(&higher)),
                Capability::Available,
                "{cloud:?}"
            );
            assert_eq!(cap(cloud, None), Capability::Unknown, "{cloud:?}");
            for missing in [
                "User.Read Chat.ReadWrite",
                "Files.Read",
                "Files.Read.All",
                "Files.ReadWrite.AppFolder",
                "XFiles.ReadWrite",
                "Files.ReadWrite.Selected",
            ] {
                assert_eq!(
                    cap(cloud, Some(missing)),
                    Capability::NotGranted,
                    "{missing}"
                );
            }
        }
    }

    #[test]
    fn a_qualified_scope_grants_only_for_this_clouds_own_graph_host() {
        for cloud in GraphCloud::ALL {
            let own = |name: &str| format!("https://{}/{name}", cloud.graph_host());
            let caps = |scope: &str| RemoteCapabilities::compute(cloud, Some(scope));
            assert_eq!(
                caps(&own("Chat.ReadWrite")).chat_message_delete,
                Capability::Available
            );
            assert_eq!(
                caps(&own("Presence.Read.All")).member_presence,
                if cloud.supports_presence() {
                    Capability::Available
                } else {
                    Capability::UnsupportedInCloud
                }
            );
            // Another cloud's resource, a look-alike host, another scheme, a
            // longer path, a prefix trick and a different case of the host.
            for other in GraphCloud::ALL.into_iter().filter(|other| *other != cloud) {
                let foreign = format!("https://{}/Chat.ReadWrite", other.graph_host());
                assert_eq!(
                    caps(&foreign).chat_message_delete,
                    Capability::NotGranted,
                    "{cloud:?} accepted {foreign}"
                );
            }
            for bad in [
                format!("https://{}.evil.test/Chat.ReadWrite", cloud.graph_host()),
                format!("https://evil.test/{}/Chat.ReadWrite", cloud.graph_host()),
                format!("http://{}/Chat.ReadWrite", cloud.graph_host()),
                format!("https://{}/v1.0/Chat.ReadWrite", cloud.graph_host()),
                format!("https://{}:8443/Chat.ReadWrite", cloud.graph_host()),
                format!("https://user@{}/Chat.ReadWrite", cloud.graph_host()),
                format!("https://{}/", cloud.graph_host()),
                "evil/Chat.ReadWrite".to_owned(),
                "/Chat.ReadWrite".to_owned(),
            ] {
                assert_eq!(
                    caps(&bad).chat_message_delete,
                    Capability::NotGranted,
                    "{cloud:?} accepted {bad}"
                );
            }
        }
    }

    #[test]
    fn lookalike_scopes_do_not_grant() {
        for scope in ["Chat.ReadWrite.All", "XChat.ReadWrite", "Chat.Read"] {
            assert_eq!(
                RemoteCapabilities::compute(GraphCloud::Global, Some(scope)).chat_message_delete,
                Capability::NotGranted,
                "{scope}"
            );
        }
    }

    #[test]
    fn member_presence_scope_and_cloud_matrix() {
        let granted = "User.Read presence.read.all";
        let qualified = "https://graph.microsoft.us/Presence.Read.All";
        let cap = |cloud, scope| RemoteCapabilities::compute(cloud, scope).member_presence;
        assert_eq!(
            cap(GraphCloud::Global, Some(granted)),
            Capability::Available
        );
        assert_eq!(
            cap(GraphCloud::UsGov, Some(qualified)),
            Capability::Available
        );
        assert_eq!(
            cap(GraphCloud::Global, Some("User.Read Chat.ReadWrite")),
            Capability::NotGranted
        );
        assert_eq!(cap(GraphCloud::Global, None), Capability::Unknown);
        assert_eq!(
            cap(GraphCloud::China, Some(granted)),
            Capability::UnsupportedInCloud
        );
    }

    #[test]
    fn presence_lookalike_scopes_do_not_grant() {
        for scope in ["Presence.Read", "Presence.Read.All.x", "XPresence.Read.All"] {
            assert_eq!(
                RemoteCapabilities::compute(GraphCloud::Global, Some(scope)).member_presence,
                Capability::NotGranted,
                "{scope}"
            );
        }
    }

    #[test]
    fn only_available_and_unknown_are_offered() {
        assert!(Capability::Available.is_offered());
        assert!(Capability::Unknown.is_offered());
        assert!(!Capability::NotGranted.is_offered());
        assert!(!Capability::UnsupportedInCloud.is_offered());
    }
}
