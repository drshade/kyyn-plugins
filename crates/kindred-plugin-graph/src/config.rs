//! Instance config: Azure app coordinates + the owner's own addresses.
//! Comes from the source's entry in `sources.ron`, handed over as canonical
//! RON — never from a file in any repository.

use anyhow::{Context, Result};
use serde::Deserialize;

// deny_unknown_fields: a misspelled or wished-for key (`search: …`) must
// fail validation by name, never be silently ignored.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub client_id: String,
    pub tenant: String,
    #[serde(default)]
    pub owner_addresses: Vec<String>,
    /// OData `$filter` fragment ANDed onto the MAIL listing's window filter —
    /// scopes a standing source to the mail that matters:
    /// `from/emailAddress/address eq 'tom@example.com'`,
    /// `contains(subject, 'Sales Update')`. Mail only: calendar, attachments
    /// of matched mail, and transcripts are unaffected. Probe the source to
    /// test an expression before proposing it.
    #[serde(default)]
    pub mail_filter: Option<String>,
}

impl Config {
    /// Parse from the context's config text (RON of a Value — the bridge
    /// builds the struct regardless of map/struct notation).
    pub fn from_ron(text: &str) -> Result<Config> {
        let value: ron::Value = ron::from_str(text).context("parsing source config")?;
        value
            .into_rust()
            .context("source config shape (need client_id, tenant, owner_addresses)")
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
