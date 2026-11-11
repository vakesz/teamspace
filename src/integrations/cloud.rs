//! The Microsoft cloud a session talks to.
//!
//! Every Graph and Entra host the app contacts is derived from one
//! [`GraphCloud`] value, so no other module hard-codes a host. The cloud is
//! fixed for the lifetime of a session and travels with its tokens; a token
//! issued by one cloud is never sent to another cloud's hosts.
//!
//! Sources: <https://learn.microsoft.com/en-us/graph/deployments> and
//! <https://learn.microsoft.com/en-us/entra/identity-platform/authentication-national-cloud>.

use url::Url;

pub(crate) const CLOUD_ENVIRONMENT_VARIABLE: &str = "TEAMSPACE_CLOUD";

/// Scopes that are OpenID Connect or OAuth meta scopes rather than resource
/// permissions, so they are never resource-qualified.
const UNQUALIFIED_SCOPES: [&str; 4] = ["openid", "profile", "offline_access", "email"];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum GraphCloud {
    #[default]
    Global,
    UsGov,
    UsGovDod,
    China,
}

/// The `TEAMSPACE_CLOUD` value is not one of the supported names. The
/// rejected value is deliberately never stored or displayed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InvalidCloud;

impl std::fmt::Display for InvalidCloud {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TEAMSPACE_CLOUD must be one of global, usgov, usgovdod, china")
    }
}

impl std::error::Error for InvalidCloud {}

impl GraphCloud {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 4] = [Self::Global, Self::UsGov, Self::UsGovDod, Self::China];

    /// Unset selects the global cloud. Only the exact lowercase names are
    /// accepted: a different case, surrounding whitespace, or non-Unicode
    /// value is an error rather than a silent fallback to the wrong cloud.
    pub(crate) fn from_environment() -> Result<Self, InvalidCloud> {
        match std::env::var_os(CLOUD_ENVIRONMENT_VARIABLE) {
            None => Ok(Self::Global),
            Some(value) => value.to_str().and_then(Self::parse).ok_or(InvalidCloud),
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "global" => Some(Self::Global),
            "usgov" => Some(Self::UsGov),
            "usgovdod" => Some(Self::UsGovDod),
            "china" => Some(Self::China),
            _ => None,
        }
    }

    pub(crate) const fn graph_host(self) -> &'static str {
        match self {
            Self::Global => "graph.microsoft.com",
            Self::UsGov => "graph.microsoft.us",
            Self::UsGovDod => "dod-graph.microsoft.us",
            Self::China => "microsoftgraph.chinacloudapi.cn",
        }
    }

    pub(crate) const fn login_host(self) -> &'static str {
        match self {
            Self::Global => "login.microsoftonline.com",
            Self::UsGov | Self::UsGovDod => "login.microsoftonline.us",
            Self::China => "login.partner.microsoftonline.cn",
        }
    }

    /// DNS suffixes (without a leading dot) of the hosts that serve
    /// preauthorized file downloads for this cloud.
    pub(crate) const fn content_host_suffixes(self) -> &'static [&'static str] {
        match self {
            Self::Global => &[
                "sharepoint.com",
                "1drv.com",
                "livefilestore.com",
                "svc.ms",
                "microsoftpersonalcontent.com",
            ],
            Self::UsGov => &["sharepoint.us"],
            Self::UsGovDod => &["sharepoint-mil.us"],
            Self::China => &["sharepoint.cn"],
        }
    }

    /// Short label for the connect menu.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Global => "Microsoft 365",
            Self::UsGov => "Microsoft 365 (US Government)",
            Self::UsGovDod => "Microsoft 365 (US Government DoD)",
            Self::China => "Microsoft 365 (China)",
        }
    }

    /// Builds `https://{graph host}/v1.0/{segments...}` with each segment
    /// percent-encoded as a single path segment. A `.` or `..` segment is
    /// rejected so a provider ID can never rewrite the route.
    pub(crate) fn graph_route(
        self,
        segments: &[&str],
        query: Option<&str>,
    ) -> Result<Url, InvalidGraphRoute> {
        if segments
            .iter()
            .any(|segment| matches!(*segment, "." | ".."))
        {
            return Err(InvalidGraphRoute);
        }
        let mut url = Url::parse(&format!("https://{}/v1.0/", self.graph_host()))
            .map_err(|_| InvalidGraphRoute)?;
        {
            let mut path = url.path_segments_mut().map_err(|_| InvalidGraphRoute)?;
            path.pop_if_empty();
            path.extend(segments);
        }
        url.set_query(query);
        Ok(url)
    }

    /// Accepts an opaque provider continuation only when it is HTTPS on this
    /// cloud's Graph host, carries no port, credentials or fragment, and
    /// continues the exact expected path.
    pub(crate) fn validated_next_link(self, raw: &str, expected_path: &str) -> Option<Url> {
        let url = Url::parse(raw).ok()?;
        let trusted = url.scheme() == "https"
            && url.host_str() == Some(self.graph_host())
            && url.port().is_none()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && url.path() == expected_path;
        trusted.then_some(url)
    }

    /// Graph documents `chatMessage: update` only for the global cloud.
    pub(crate) const fn supports_chat_message_update(self) -> bool {
        matches!(self, Self::Global)
    }

    /// Graph documents `getPresencesByUserId` as unavailable in the China
    /// (21Vianet) cloud.
    pub(crate) const fn supports_presence(self) -> bool {
        !matches!(self, Self::China)
    }

    /// Global uses bare resource scopes. National clouds need the resource
    /// qualified form `https://{graph host}/{Scope}`.
    pub(crate) fn qualify_scope(self, scope: &str) -> String {
        if self == Self::Global || UNQUALIFIED_SCOPES.contains(&scope) {
            scope.to_owned()
        } else {
            format!("https://{}/{scope}", self.graph_host())
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InvalidGraphRoute;

#[cfg(test)]
mod tests {
    use super::{GraphCloud, InvalidCloud};

    #[test]
    fn only_the_global_cloud_supports_chat_message_update() {
        for cloud in GraphCloud::ALL {
            assert_eq!(
                cloud.supports_chat_message_update(),
                cloud == GraphCloud::Global
            );
        }
    }

    #[test]
    fn parse_accepts_only_the_exact_names() {
        assert_eq!(GraphCloud::parse("global"), Some(GraphCloud::Global));
        assert_eq!(GraphCloud::parse("usgov"), Some(GraphCloud::UsGov));
        assert_eq!(GraphCloud::parse("usgovdod"), Some(GraphCloud::UsGovDod));
        assert_eq!(GraphCloud::parse("china"), Some(GraphCloud::China));
        for rejected in [
            "", "Global", "CHINA", " global", "global ", "gcc", "usgov\n",
        ] {
            assert_eq!(GraphCloud::parse(rejected), None, "{rejected:?}");
        }
    }

    #[test]
    fn invalid_cloud_error_never_echoes_a_value() {
        let rendered = format!("{InvalidCloud} {InvalidCloud:?}");
        assert!(!rendered.contains("secret"));
    }

    #[test]
    fn hosts_are_distinct_per_cloud() {
        let expected = [
            ("graph.microsoft.com", "login.microsoftonline.com"),
            ("graph.microsoft.us", "login.microsoftonline.us"),
            ("dod-graph.microsoft.us", "login.microsoftonline.us"),
            (
                "microsoftgraph.chinacloudapi.cn",
                "login.partner.microsoftonline.cn",
            ),
        ];
        for (cloud, (graph, login)) in GraphCloud::ALL.into_iter().zip(expected) {
            assert_eq!(cloud.graph_host(), graph);
            assert_eq!(cloud.login_host(), login);
        }
    }

    #[test]
    fn graph_route_uses_the_cloud_host_and_encodes_segments() {
        for cloud in GraphCloud::ALL {
            let url = cloud
                .graph_route(&["chats", "19:a/b", "messages"], Some("$top=50"))
                .expect("route");
            assert_eq!(url.scheme(), "https");
            assert_eq!(url.host_str(), Some(cloud.graph_host()));
            assert_eq!(url.path(), "/v1.0/chats/19:a%2Fb/messages");
            assert_eq!(url.query(), Some("$top=50"));
        }
    }

    #[test]
    fn graph_route_rejects_dot_segments() {
        for segment in [".", ".."] {
            assert!(
                GraphCloud::Global
                    .graph_route(&["chats", segment], None)
                    .is_err()
            );
        }
    }

    #[test]
    fn next_links_must_match_the_cloud_host_and_exact_path() {
        for cloud in GraphCloud::ALL {
            let path = "/v1.0/me/chats";
            let good = format!("https://{}{path}?$skiptoken=x", cloud.graph_host());
            assert!(cloud.validated_next_link(&good, path).is_some());
            for other in GraphCloud::ALL.into_iter().filter(|other| *other != cloud) {
                let cross = format!("https://{}{path}?$skiptoken=x", other.graph_host());
                assert!(cloud.validated_next_link(&cross, path).is_none());
            }
            let host = cloud.graph_host();
            for bad in [
                format!("http://{host}{path}"),
                format!("https://{host}:8443{path}"),
                format!("https://user:pw@{host}{path}"),
                format!("https://{host}{path}#frag"),
                format!("https://{host}/v1.0/me/other"),
                format!("https://{host}.evil.test{path}"),
                "not a url".to_owned(),
            ] {
                assert!(cloud.validated_next_link(&bad, path).is_none(), "{bad}");
            }
        }
    }

    #[test]
    fn scopes_are_qualified_outside_the_global_cloud() {
        for cloud in GraphCloud::ALL {
            for meta in ["openid", "profile", "offline_access", "email"] {
                assert_eq!(cloud.qualify_scope(meta), meta);
            }
            let qualified = cloud.qualify_scope("Chat.Read");
            if cloud == GraphCloud::Global {
                assert_eq!(qualified, "Chat.Read");
            } else {
                assert_eq!(
                    qualified,
                    format!("https://{}/Chat.Read", cloud.graph_host())
                );
            }
        }
    }

    #[test]
    fn national_clouds_use_only_their_own_content_hosts() {
        assert!(
            GraphCloud::Global
                .content_host_suffixes()
                .contains(&"sharepoint.com")
        );
        for cloud in [GraphCloud::UsGov, GraphCloud::UsGovDod, GraphCloud::China] {
            let suffixes = cloud.content_host_suffixes();
            assert_eq!(suffixes.len(), 1);
            assert!(!suffixes.contains(&"svc.ms") && !suffixes.contains(&"dps.mil"));
            assert!(!suffixes.contains(&"sharepoint.com"));
        }
    }

    #[test]
    fn no_other_integration_source_hard_codes_a_cloud_host() {
        fn visit(dir: &std::path::Path, found: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).expect("readable source tree") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    visit(&path, found);
                    continue;
                }
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !name.ends_with(".rs") || name == "cloud.rs" || name.contains("tests") {
                    continue;
                }
                let source = std::fs::read_to_string(&path).expect("utf-8 source");
                // Everything from the unit-test module on is test code.
                let production = source.split("#[cfg(test)]\nmod tests").next().unwrap_or("");
                if production.contains("graph.microsoft.com")
                    || production.contains("login.microsoftonline")
                {
                    found.push(path.display().to_string());
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/integrations");
        let mut found = Vec::new();
        visit(&root, &mut found);
        // Crate-level files outside `src/integrations` are held to the same rule.
        for name in ["lib.rs", "cli.rs", "fuzz.rs", "main.rs"] {
            let path = root.join("..").join(name);
            if let Ok(source) = std::fs::read_to_string(&path) {
                let production = source.split("#[cfg(test)]\nmod tests").next().unwrap_or("");
                if production.contains("graph.microsoft.com")
                    || production.contains("login.microsoftonline")
                {
                    found.push(path.display().to_string());
                }
            }
        }
        assert!(found.is_empty(), "hard-coded cloud hosts in {found:?}");
    }
}
