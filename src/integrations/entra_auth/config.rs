//! Non-secret configuration for a tenant-owned Entra public client.
//!
//! Teamspace intentionally does not bundle a client ID, tenant, redirect URI,
//! or delegated permissions. Those choices belong to the tenant owner. This
//! module accepts only the narrowly shaped configuration required for a
//! loopback authorization-code flow; access and refresh tokens never enter
//! this configuration type.

use super::super::cloud::GraphCloud;
use std::fmt;

pub(crate) const CLIENT_ID_ENV: &str = "TEAMSPACE_ENTRA_CLIENT_ID";
pub(crate) const TENANT_ENV: &str = "TEAMSPACE_ENTRA_TENANT";
pub(crate) const LOOPBACK_PORT_ENV: &str = "TEAMSPACE_ENTRA_LOOPBACK_PORT";
pub(crate) const SCOPES_ENV: &str = "TEAMSPACE_ENTRA_SCOPES";

const REQUIRED_SCOPES: [&str; 2] = ["offline_access", "User.Read"];
/// Scopes that only shape an `id_token`, which Teamspace never requests or
/// reads. Older configurations may still list them; they are accepted and
/// then left out of every authorization and refresh request.
const ID_TOKEN_ONLY_SCOPES: [&str; 3] = ["openid", "profile", "email"];
const MAX_TENANT_BYTES: usize = 255;
const MAX_SCOPE_BYTES: usize = 128;
const MAX_SCOPES: usize = 32;
const MAX_SCOPE_LIST_BYTES: usize = 2_048;

/// Tenant-owned public-client settings used only to create an authorization
/// request. The exact redirect is always `127.0.0.1`, never an arbitrary URI
/// supplied through the environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EntraPublicClientConfig {
    client_id: String,
    tenant: String,
    loopback_port: u16,
    scopes: Vec<String>,
    cloud: GraphCloud,
}

impl EntraPublicClientConfig {
    /// Loads an all-or-nothing public-client configuration from the process
    /// environment. When every Entra setting is absent, remote sign-in is not
    /// configured; a partially specified configuration is rejected rather
    /// than guessed.
    pub(crate) fn from_environment() -> Result<Option<Self>, EntraConfigError> {
        let client_id = read_environment(CLIENT_ID_ENV)?;
        let tenant = read_environment(TENANT_ENV)?;
        let loopback_port = read_environment(LOOPBACK_PORT_ENV)?;
        let scopes = read_environment(SCOPES_ENV)?;

        if [
            client_id.as_ref(),
            tenant.as_ref(),
            loopback_port.as_ref(),
            scopes.as_ref(),
        ]
        .iter()
        .all(Option::is_none)
        {
            return Ok(None);
        }

        // The cloud is a separate, optional setting: it must not make the
        // four required values above all-or-nothing, and an invalid value is
        // an error rather than a silent fallback to the global cloud.
        let cloud = GraphCloud::from_environment().map_err(|_| EntraConfigError::InvalidCloud)?;

        Self::from_values(
            client_id.as_deref(),
            tenant.as_deref(),
            loopback_port.as_deref(),
            scopes.as_deref(),
        )
        .map(|config| Some(config.with_cloud(cloud)))
    }

    /// Selects the cloud whose login and Graph hosts this configuration uses.
    pub(super) fn with_cloud(mut self, cloud: GraphCloud) -> Self {
        self.cloud = cloud;
        self
    }

    pub(super) const fn cloud(&self) -> GraphCloud {
        self.cloud
    }

    pub(super) fn from_values(
        client_id: Option<&str>,
        tenant: Option<&str>,
        loopback_port: Option<&str>,
        scopes: Option<&str>,
    ) -> Result<Self, EntraConfigError> {
        let client_id = client_id
            .ok_or(EntraConfigError::MissingEnvironment(CLIENT_ID_ENV))?
            .trim();
        if !is_client_id(client_id) {
            return Err(EntraConfigError::InvalidClientId);
        }

        let tenant = tenant
            .ok_or(EntraConfigError::MissingEnvironment(TENANT_ENV))?
            .trim();
        if !is_tenant(tenant) {
            return Err(EntraConfigError::InvalidTenant);
        }

        let loopback_port = loopback_port
            .ok_or(EntraConfigError::MissingEnvironment(LOOPBACK_PORT_ENV))?
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .ok_or(EntraConfigError::InvalidLoopbackPort)?;

        let scopes = parse_scopes(scopes.ok_or(EntraConfigError::MissingEnvironment(SCOPES_ENV))?)?;

        Ok(Self {
            client_id: client_id.into(),
            tenant: tenant.into(),
            loopback_port,
            scopes,
            cloud: GraphCloud::Global,
        })
    }

    pub(super) fn client_id(&self) -> &str {
        &self.client_id
    }

    pub(crate) const fn loopback_port(&self) -> u16 {
        self.loopback_port
    }

    /// The redirect is built from the validated port, so config cannot route
    /// an authorization code to another host or callback path.
    pub(super) fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}/oauth/callback", self.loopback_port)
    }

    pub(super) fn authorization_endpoint(&self) -> String {
        format!(
            "https://{}/{}/oauth2/v2.0/authorize",
            self.cloud.login_host(),
            self.tenant
        )
    }

    /// The v2 token endpoint shares the validated tenant segment with the
    /// authorization endpoint. No environment value can redirect a code or
    /// verifier to an arbitrary host.
    pub(super) fn token_endpoint(&self) -> String {
        format!(
            "https://{}/{}/oauth2/v2.0/token",
            self.cloud.login_host(),
            self.tenant
        )
    }

    /// The space-delimited scope parameter. Required-scope validation uses
    /// the bare names; national clouds additionally need each resource scope
    /// qualified with that cloud's Graph host.
    pub(super) fn scope_string(&self) -> String {
        self.scopes
            .iter()
            .map(|scope| self.cloud.qualify_scope(scope))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn read_environment(name: &'static str) -> Result<Option<String>, EntraConfigError> {
    let Some(value) = std::env::var_os(name) else {
        return Ok(None);
    };
    value
        .into_string()
        .map(Some)
        .map_err(|_| EntraConfigError::NonUnicodeEnvironment(name))
}

fn is_client_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && [8, 13, 18, 23]
            .into_iter()
            .all(|index| bytes[index] == b'-')
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 8 | 13 | 18 | 23) || byte.is_ascii_hexdigit())
}

fn is_tenant(value: &str) -> bool {
    if value.is_empty() || value.len() > MAX_TENANT_BYTES {
        return false;
    }
    if matches!(value, "common" | "organizations" | "consumers") {
        return true;
    }
    value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .last()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
}

fn parse_scopes(raw: &str) -> Result<Vec<String>, EntraConfigError> {
    if raw.len() > MAX_SCOPE_LIST_BYTES || raw.chars().any(char::is_control) {
        return Err(EntraConfigError::InvalidScopes);
    }
    let scopes = raw
        .split_whitespace()
        .map(str::trim)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if scopes.is_empty()
        || scopes.len() > MAX_SCOPES
        || scopes.iter().any(|scope| !is_scope(scope))
        || has_duplicate_scopes(&scopes)
    {
        return Err(EntraConfigError::InvalidScopes);
    }
    for required in REQUIRED_SCOPES {
        if !scopes.iter().any(|scope| scope == required) {
            return Err(EntraConfigError::MissingRequiredScope(required));
        }
    }
    Ok(scopes
        .into_iter()
        .filter(|scope| !ID_TOKEN_ONLY_SCOPES.contains(&scope.as_str()))
        .collect())
}

fn is_scope(scope: &str) -> bool {
    !scope.is_empty()
        && scope.len() <= MAX_SCOPE_BYTES
        && scope
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn has_duplicate_scopes(scopes: &[String]) -> bool {
    scopes.iter().enumerate().any(|(index, scope)| {
        scopes[index + 1..]
            .iter()
            .any(|candidate| candidate == scope)
    })
}

/// Configuration errors intentionally do not include user-supplied values;
/// a deployment may contain tenant-specific information even though it is not
/// a secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EntraConfigError {
    MissingEnvironment(&'static str),
    NonUnicodeEnvironment(&'static str),
    InvalidClientId,
    InvalidTenant,
    InvalidLoopbackPort,
    InvalidScopes,
    MissingRequiredScope(&'static str),
    InvalidCloud,
}

impl fmt::Display for EntraConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingEnvironment(name) => {
                write!(formatter, "missing required Entra setting {name}")
            }
            Self::NonUnicodeEnvironment(name) => {
                write!(formatter, "Entra setting {name} is not valid Unicode")
            }
            Self::InvalidClientId => formatter.write_str("Entra client ID is invalid"),
            Self::InvalidTenant => formatter.write_str("Entra tenant is invalid"),
            Self::InvalidLoopbackPort => formatter.write_str("Entra loopback port is invalid"),
            Self::InvalidScopes => formatter.write_str("Entra delegated scopes are invalid"),
            Self::MissingRequiredScope(scope) => {
                write!(formatter, "Entra delegated scopes must include {scope}")
            }
            Self::InvalidCloud => formatter.write_str("TEAMSPACE_CLOUD is not a supported cloud"),
        }
    }
}

impl std::error::Error for EntraConfigError {}

#[cfg(test)]
mod tests {
    use super::{
        CLIENT_ID_ENV, EntraConfigError, EntraPublicClientConfig, LOOPBACK_PORT_ENV, SCOPES_ENV,
        TENANT_ENV,
    };
    use crate::integrations::cloud::GraphCloud;

    const CLIENT_ID: &str = "6f2c8b6a-218c-4d3f-b4f4-e8d77d6c9a72";
    const SCOPES: &str = "offline_access User.Read Chat.Read";

    fn config(
        client_id: Option<&str>,
        tenant: Option<&str>,
        port: Option<&str>,
        scopes: Option<&str>,
    ) -> Result<EntraPublicClientConfig, EntraConfigError> {
        EntraPublicClientConfig::from_values(client_id, tenant, port, scopes)
    }

    #[test]
    fn valid_config_keeps_an_exact_loopback_redirect_and_scope_order() {
        let config = config(
            Some(CLIENT_ID),
            Some("organizations"),
            Some("8400"),
            Some(SCOPES),
        )
        .expect("valid public-client settings must parse");

        assert_eq!(config.client_id(), CLIENT_ID);
        assert_eq!(config.tenant, "organizations");
        assert_eq!(config.loopback_port(), 8400);
        assert_eq!(
            config.redirect_uri(),
            "http://127.0.0.1:8400/oauth/callback"
        );
        assert_eq!(
            config.authorization_endpoint(),
            "https://login.microsoftonline.com/organizations/oauth2/v2.0/authorize"
        );
        assert_eq!(
            config.token_endpoint(),
            "https://login.microsoftonline.com/organizations/oauth2/v2.0/token"
        );
        assert_eq!(config.scope_string(), SCOPES);
        assert_eq!(config.scopes.len(), 3);
    }

    #[test]
    fn endpoints_and_scopes_follow_the_configured_cloud() {
        let expected = [
            (GraphCloud::Global, "login.microsoftonline.com"),
            (GraphCloud::UsGov, "login.microsoftonline.us"),
            (GraphCloud::UsGovDod, "login.microsoftonline.us"),
            (GraphCloud::China, "login.partner.microsoftonline.cn"),
        ];
        for (cloud, login_host) in expected {
            let config = config(
                Some(CLIENT_ID),
                Some("organizations"),
                Some("8400"),
                Some(SCOPES),
            )
            .expect("valid settings")
            .with_cloud(cloud);
            assert_eq!(config.cloud(), cloud);
            assert_eq!(
                config.authorization_endpoint(),
                format!("https://{login_host}/organizations/oauth2/v2.0/authorize")
            );
            assert_eq!(
                config.token_endpoint(),
                format!("https://{login_host}/organizations/oauth2/v2.0/token")
            );
            let scope = config.scope_string();
            if cloud == GraphCloud::Global {
                assert_eq!(scope, SCOPES);
            } else {
                let host = cloud.graph_host();
                assert_eq!(
                    scope,
                    format!("offline_access https://{host}/User.Read https://{host}/Chat.Read")
                );
            }
            // Required-scope validation still applies to the bare names.
            assert_eq!(config.scopes.len(), 3);
        }
    }

    #[test]
    fn id_token_scopes_are_accepted_but_never_requested() {
        let config = config(
            Some(CLIENT_ID),
            Some("organizations"),
            Some("8400"),
            Some("openid profile email offline_access User.Read Chat.Read"),
        )
        .expect("legacy scope lists must keep working");

        assert_eq!(config.scope_string(), "offline_access User.Read Chat.Read");
        assert_eq!(
            config_missing_scope("openid profile User.Read"),
            EntraConfigError::MissingRequiredScope("offline_access")
        );
    }

    fn config_missing_scope(scopes: &str) -> EntraConfigError {
        config(
            Some(CLIENT_ID),
            Some("organizations"),
            Some("8400"),
            Some(scopes),
        )
        .expect_err("a required scope is missing")
    }

    #[test]
    fn partial_configuration_is_rejected_instead_of_guessing_missing_values() {
        assert_eq!(
            config(Some(CLIENT_ID), None, Some("8400"), Some(SCOPES)),
            Err(EntraConfigError::MissingEnvironment(TENANT_ENV))
        );
        assert_eq!(
            config(None, Some("organizations"), Some("8400"), Some(SCOPES)),
            Err(EntraConfigError::MissingEnvironment(CLIENT_ID_ENV))
        );
        assert_eq!(
            config(Some(CLIENT_ID), Some("organizations"), None, Some(SCOPES)),
            Err(EntraConfigError::MissingEnvironment(LOOPBACK_PORT_ENV))
        );
        assert_eq!(
            config(Some(CLIENT_ID), Some("organizations"), Some("8400"), None),
            Err(EntraConfigError::MissingEnvironment(SCOPES_ENV))
        );
    }

    #[test]
    fn client_tenant_port_and_scope_validation_rejects_unsafe_values() {
        assert_eq!(
            config(
                Some("not-a-client-id"),
                Some("organizations"),
                Some("8400"),
                Some(SCOPES)
            ),
            Err(EntraConfigError::InvalidClientId)
        );
        assert_eq!(
            config(
                Some(CLIENT_ID),
                Some("organizations/evil"),
                Some("8400"),
                Some(SCOPES)
            ),
            Err(EntraConfigError::InvalidTenant)
        );
        assert_eq!(
            config(
                Some(CLIENT_ID),
                Some("organizations"),
                Some("0"),
                Some(SCOPES)
            ),
            Err(EntraConfigError::InvalidLoopbackPort)
        );
        assert_eq!(
            config(
                Some(CLIENT_ID),
                Some("organizations"),
                Some("8400"),
                Some("offline_access User.Read Chat.Read&evil"),
            ),
            Err(EntraConfigError::InvalidScopes)
        );
        assert_eq!(
            config(
                Some(CLIENT_ID),
                Some("organizations"),
                Some("8400"),
                Some("offline_access User.Read\nChat.Read"),
            ),
            Err(EntraConfigError::InvalidScopes)
        );
    }

    #[test]
    fn scope_set_requires_oidc_refresh_and_identity_permissions_without_duplicates() {
        assert_eq!(
            config(
                Some(CLIENT_ID),
                Some("organizations"),
                Some("8400"),
                Some("openid profile User.Read Chat.Read"),
            ),
            Err(EntraConfigError::MissingRequiredScope("offline_access"))
        );
        assert_eq!(
            config(
                Some(CLIENT_ID),
                Some("organizations"),
                Some("8400"),
                Some("offline_access User.Read User.Read"),
            ),
            Err(EntraConfigError::InvalidScopes)
        );
    }
}
