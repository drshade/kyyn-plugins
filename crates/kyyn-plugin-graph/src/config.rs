//! Instance config: Azure app coordinates + the owner's own addresses.
//! Comes from the source's entry in `sources.ron`, handed over as canonical
//! RON — never from a file in any repository.

use anyhow::{Context, Result};
use serde::Deserialize;

/// The shared "kyyn" app registration — MULTI-TENANT and a public client
/// (device-code flow; a client id is not a secret), so any org's users sign
/// in with it after their tenant consents. Overridable per instance for
/// orgs that register their own app.
pub const DEFAULT_CLIENT_ID: &str = "53ddb21b-849f-45a3-8168-8a0e555f386f";
/// Entra's multi-tenant work/school endpoint — any organizational
/// directory, resolved to the user's own tenant at sign-in.
pub const DEFAULT_TENANT: &str = "organizations";

fn default_client_id() -> String {
    DEFAULT_CLIENT_ID.into()
}
fn default_tenant() -> String {
    DEFAULT_TENANT.into()
}

// deny_unknown_fields: a misspelled or wished-for key (`search: …`) must
// fail validation by name, never be silently ignored.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Azure app (client) id. Default: the shared multi-tenant "kyyn"
    /// registration — set both this and `tenant` to use your org's own.
    #[serde(default = "default_client_id")]
    pub client_id: String,
    /// Entra tenant: a tenant GUID, or `organizations` (any work/school
    /// directory — the default, paired with the shared registration).
    #[serde(default = "default_tenant")]
    pub tenant: String,
    #[serde(default)]
    pub owner_addresses: Vec<String>,
    /// OData `$filter` fragment ANDed onto the MAIL listing's window filter —
    /// scopes a standing source to the mail that matters:
    /// `from/emailAddress/address eq 'tom@example.com'`,
    /// `contains(subject, 'Sales Update')`. Mail only: calendar, attachments
    /// of matched mail, and transcripts are unaffected. Probe the source to
    /// test an expression before proposing it.
    ///
    /// Accepts a bare `"…"` or an explicit `Some("…")`: the web install form
    /// (and every `Str`-typed config field) renders values bare, so an
    /// optional string field must take both spellings.
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub mail_filter: Option<String>,
}

/// `Option<String>` from a bare string, an explicit `Some(...)`/`None`, or
/// absent — so form-assembled configs (bare) and hand-written RON
/// (`Some(...)`) both parse.
fn opt_string_lenient<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Lenient {
        Opt(Option<String>),
        Bare(String),
    }
    Ok(match Lenient::deserialize(d)? {
        Lenient::Opt(o) => o,
        Lenient::Bare(s) => Some(s),
    })
}

impl Config {
    /// Parse from the context's config text (RON of a Value — the bridge
    /// builds the struct regardless of map/struct notation).
    pub fn from_ron(text: &str) -> Result<Config> {
        let value: ron::Value = ron::from_str(text).context("parsing source config")?;
        // No config at all (`()` — the engine's spelling of an absent
        // config): every field has a default, so that IS a valid config.
        if matches!(value, ron::Value::Unit) {
            return Ok(Config {
                client_id: default_client_id(),
                tenant: default_tenant(),
                owner_addresses: Vec::new(),
                mail_filter: None,
            });
        }
        value
            .into_rust()
            .context("source config shape (all fields optional: client_id, tenant, owner_addresses, mail_filter)")
    }

    pub fn device_code_url(&self) -> String {
        format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/devicecode",
            self.tenant
        )
    }

    pub fn token_url(&self) -> String {
        format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
            self.tenant
        )
    }

    /// Case-insensitive membership in the owner's known addresses.
    pub fn is_own_address(&self, addr: Option<&str>) -> bool {
        match addr {
            None => false,
            Some(a) => self
                .owner_addresses
                .iter()
                .any(|o| o.eq_ignore_ascii_case(a)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A wished-for or misspelled config key must fail BY NAME through the
    /// Value bridge — an agent probing `search: "…"` learns the shape
    /// immediately instead of silently syncing the whole mailbox.
    #[test]
    fn unknown_config_keys_fail_by_name() {
        let err = Config::from_ron(r#"(client_id: "c", tenant: "t", search: "BEE")"#)
            .unwrap_err()
            .root_cause()
            .to_string();
        assert!(err.contains("search"), "{err}");
    }

    #[test]
    fn mail_filter_parses_and_stays_optional() {
        let cfg = Config::from_ron(
            r#"(client_id: "c", tenant: "t", mail_filter: Some("contains(subject, 'Sales Update')"))"#,
        )
        .unwrap();
        assert_eq!(
            cfg.mail_filter.as_deref(),
            Some("contains(subject, 'Sales Update')")
        );
        assert!(
            Config::from_ron(r#"(client_id: "c", tenant: "t")"#)
                .unwrap()
                .mail_filter
                .is_none()
        );
    }
}

#[cfg(test)]
mod default_tests {
    use super::*;

    /// A minimal `()` config gets the shared multi-tenant registration —
    /// zero Azure knowledge needed; explicit fields still override.
    #[test]
    fn defaults_are_the_shared_multitenant_app() {
        let cfg = Config::from_ron("()").unwrap();
        assert_eq!(cfg.client_id, DEFAULT_CLIENT_ID);
        assert_eq!(cfg.tenant, "organizations");
        assert!(cfg.device_code_url().contains("/organizations/"));
        let absent = Config::from_ron("()").unwrap();
        assert_eq!(
            absent.tenant, "organizations",
            "absent config = all defaults"
        );
        let own = Config::from_ron(r#"(client_id: "mine", tenant: "t1")"#).unwrap();
        assert_eq!(own.client_id, "mine");
        assert_eq!(own.tenant, "t1");
    }
}
