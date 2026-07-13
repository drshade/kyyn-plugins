//! `salesforce` — a SOQL query as a source: each returned record becomes a
//! hashed, immutable evidence item (ADR 0001 — the bundle file is storage,
//! the record Id is identity, the content hash anchors integrity), so a
//! changed Opportunity returns as new curation work while unchanged rows
//! dedup as re-observations.
//!
//! Auth is the OAuth 2.0 DEVICE FLOW against a Connected App the org
//! registers (enable "OAuth Device Flow"; scopes `api refresh_token`). The
//! consumer key is the `client_id`; there is no secret — same public-client
//! model as the graph family, one sign-in per (host, client) realm.

use std::path::Path;

use kindred_core::plugin::{
    AuthChallenge, AuthPollResult, AuthStatus, Context, Describe, FetchRequest, FetchResult,
    FetchStyle, Item, RunSpec, SourcePlugin,
};
use serde::{Deserialize, Serialize};
use sha2::Digest;

pub struct SalesforcePlugin;

/// Instance config from `sources.ron`. Unknown keys are validation
/// errors, never silently ignored.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    /// The org's My Domain URL (`https://acme.my.salesforce.com`).
    instance_url: String,
    /// The Connected App's consumer key (device flow enabled; not a secret).
    client_id: String,
    /// The SOQL query whose records this source ingests.
    query: String,
    /// The item kind records carry (eval keys, links). Default `sf-record`.
    #[serde(default = "default_kind")]
    kind: String,
    /// REST API version. Default `v62.0`.
    #[serde(default = "default_api_version")]
    api_version: String,
    /// The field carrying each record's stable identity. Default `Id`.
    #[serde(default = "default_id_field")]
    id_field: String,
}

fn default_kind() -> String {
    "sf-record".into()
}
fn default_api_version() -> String {
    "v62.0".into()
}
fn default_id_field() -> String {
    "Id".into()
}

fn parse_config(text: &str) -> Result<Config, String> {
    let value: ron::Value = ron::from_str(text).map_err(|e| format!("salesforce config: {e}"))?;
    value.into_rust().map_err(|e| {
        format!(
            "salesforce config shape (instance_url, client_id, query; optional kind, \
             api_version, id_field): {e}"
        )
    })
}

fn host_of(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("salesforce")
        .to_string()
}

fn token_path(secrets_dir: &Path) -> std::path::PathBuf {
    secrets_dir.join("sf-token.json")
}

#[derive(Serialize, Deserialize)]
struct StoredToken {
    access_token: String,
    refresh_token: Option<String>,
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

impl SourcePlugin for SalesforcePlugin {
    fn describe(&self) -> Describe {
        Describe {
            name: "salesforce".into(),
            link_namespace: "sf".into(),
            fetch_style: FetchStyle::Snapshot,
            auth_realm: Some("salesforce".into()),
            protocol: kindred_core::plugin::PROTOCOL,
        }
    }

    fn validate_config(&self, config: &str) -> Result<(), String> {
        let c = parse_config(config)?;
        if !c.instance_url.starts_with("https://") {
            return Err("instance_url must be an https:// URL (the org's My Domain)".into());
        }
        if c.client_id.trim().is_empty() {
            return Err("client_id (the Connected App's consumer key) is required".into());
        }
        if !c.query.trim().to_ascii_lowercase().starts_with("select") {
            return Err("query must be a SOQL SELECT".into());
        }
        if c.kind.is_empty()
            || !c
                .kind
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        {
            return Err(format!("kind '{}' must be a bare token", c.kind));
        }
        Ok(())
    }

    fn config_auth_realm(&self, config: &str) -> Result<Option<String>, String> {
        let c = parse_config(config)?;
        Ok(Some(format!(
            "salesforce:{}:{}",
            host_of(&c.instance_url),
            c.client_id
        )))
    }

    fn auth_status(&self, ctx: &Context) -> Result<AuthStatus, String> {
        if token_path(&ctx.secrets_dir).exists() {
            Ok(AuthStatus::Authenticated(
                "token cached (verified on fetch)".into(),
            ))
        } else {
            Ok(AuthStatus::NotAuthenticated(
                "no token — sign the realm in (source_auth_begin, or `kindred source auth`)".into(),
            ))
        }
    }

    fn authenticate(&self, ctx: &Context) -> Result<(), String> {
        // Blocking begin+poll for completeness; interactive surfaces use
        // auth_begin/auth_poll (a tap's stdout is the wire, never a UI).
        let ch = self.auth_begin(ctx)?;
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(ch.expires_in_secs);
        loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            if std::time::Instant::now() > deadline {
                return Err("sign-in code expired".into());
            }
            match self.auth_poll(ctx, &ch.handle)? {
                AuthPollResult::Pending => {}
                AuthPollResult::Done(_) => return Ok(()),
                AuthPollResult::Failed(m) => return Err(m),
            }
        }
    }

    fn auth_begin(&self, ctx: &Context) -> Result<AuthChallenge, String> {
        let c = parse_config(&ctx.config)?;
        let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        rt.block_on(async {
            let resp: serde_json::Value = reqwest::Client::new()
                .post(format!("{}/services/oauth2/token", c.instance_url))
                .form(&[
                    ("response_type", "device_code"),
                    ("client_id", &c.client_id),
                    ("scope", "api refresh_token"),
                ])
                .send()
                .await
                .map_err(|e| e.to_string())?
                .json()
                .await
                .map_err(|e| e.to_string())?;
            let field = |k: &str| resp[k].as_str().map(str::to_string);
            Ok(AuthChallenge {
                verification_url: field("verification_uri")
                    .ok_or_else(|| format!("device flow refused: {resp}"))?,
                user_code: field("user_code").ok_or("no user_code")?,
                expires_in_secs: 600,
                handle: field("device_code").ok_or("no device_code")?,
            })
        })
    }

    fn auth_poll(&self, ctx: &Context, handle: &str) -> Result<AuthPollResult, String> {
        let c = parse_config(&ctx.config)?;
        let secrets = ctx.secrets_dir.clone();
        let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        rt.block_on(async {
            let resp: serde_json::Value = reqwest::Client::new()
                .post(format!("{}/services/oauth2/token", c.instance_url))
                .form(&[
                    ("grant_type", "device"),
                    ("client_id", &c.client_id),
                    ("code", handle),
                ])
                .send()
                .await
                .map_err(|e| e.to_string())?
                .json()
                .await
                .map_err(|e| e.to_string())?;
            if let Some(token) = resp["access_token"].as_str() {
                let stored = StoredToken {
                    access_token: token.to_string(),
                    refresh_token: resp["refresh_token"].as_str().map(str::to_string),
                };
                std::fs::create_dir_all(&secrets).map_err(|e| e.to_string())?;
                std::fs::write(
                    token_path(&secrets),
                    serde_json::to_string(&stored).map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())?;
                return Ok(AuthPollResult::Done("signed in".into()));
            }
            match resp["error"].as_str() {
                Some("authorization_pending") | Some("slow_down") => Ok(AuthPollResult::Pending),
                Some(e) => Ok(AuthPollResult::Failed(format!(
                    "{e}: {}",
                    resp["error_description"].as_str().unwrap_or("")
                ))),
                None => Ok(AuthPollResult::Failed(format!("unexpected reply: {resp}"))),
            }
        })
    }

    fn fetch(&self, req: &FetchRequest) -> Result<FetchResult, String> {
        if !matches!(req.spec, RunSpec::Snapshot) {
            return Err("salesforce is a snapshot source".into());
        }
        let c = parse_config(&req.config)?;
        let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        rt.block_on(run_query(&c, req))
    }
}

/// A valid access token, refreshing through the stored refresh token when
/// the current one has expired.
async fn access_token(
    c: &Config,
    secrets_dir: &Path,
    force_refresh: bool,
) -> Result<String, String> {
    let text = std::fs::read_to_string(token_path(secrets_dir))
        .map_err(|_| "no token — sign the realm in first".to_string())?;
    let stored: StoredToken = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    if !force_refresh {
        return Ok(stored.access_token);
    }
    let refresh = stored
        .refresh_token
        .ok_or("token expired and no refresh token — sign in again")?;
    let resp: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/services/oauth2/token", c.instance_url))
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", &c.client_id),
            ("refresh_token", &refresh),
        ])
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let token = resp["access_token"]
        .as_str()
        .ok_or_else(|| format!("token refresh failed — sign in again ({resp})"))?
        .to_string();
    let stored = StoredToken {
        access_token: token.clone(),
        refresh_token: Some(refresh),
    };
    std::fs::write(
        token_path(secrets_dir),
        serde_json::to_string(&stored).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    Ok(token)
}

async fn run_query(c: &Config, req: &FetchRequest) -> Result<FetchResult, String> {
    let client = reqwest::Client::new();
    let mut token = access_token(c, &req.secrets_dir, false).await?;
    let first_url = format!(
        "{}/services/data/{}/query?q={}",
        c.instance_url,
        c.api_version,
        percent_encode(&c.query)
    );

    let mut records: Vec<serde_json::Value> = Vec::new();
    let mut next: Option<String> = Some(first_url.clone());
    let mut refreshed = false;
    let mut pages = 0u32;
    while let Some(url) = next.take() {
        let resp = client
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if resp.status().as_u16() == 401 && !refreshed {
            // One refresh, then replay this page.
            token = access_token(c, &req.secrets_dir, true).await?;
            refreshed = true;
            next = Some(url);
            continue;
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("SOQL query failed (HTTP {status}): {body}"));
        }
        let page: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        if let Some(batch) = page["records"].as_array() {
            records.extend(batch.iter().cloned());
        }
        next = page["nextRecordsUrl"]
            .as_str()
            .map(|p| format!("{}{}", c.instance_url, p));
        pages += 1;
        if next.is_some() {
            kindred_core::progress::report(&format!("{} records ({pages} pages)…", records.len()));
        }
        if pages >= 500 {
            return Err("paging exceeded 500 pages — narrow the query".into());
        }
    }
    kindred_core::progress::report(&format!("{} records returned", records.len()));

    std::fs::create_dir_all(&req.out_dir).map_err(|e| e.to_string())?;
    std::fs::write(
        req.out_dir.join("records.json"),
        serde_json::to_string_pretty(&records).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;

    // ONE ITEM PER RECORD (ADR 0001): the bundle is storage, the record Id
    // is identity, the content hash anchors evidence integrity.
    let mut items = Vec::new();
    for (n, record) in records.iter().enumerate() {
        let id = record[&c.id_field]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| format!("row-{n}"));
        let canon = serde_json::to_string(record).map_err(|e| e.to_string())?;
        let name = record["Name"].as_str().unwrap_or_default();
        let sobject = record["attributes"]["type"].as_str().unwrap_or("record");
        items.push(Item {
            id: id.clone(),
            kind: c.kind.clone(),
            version: None,
            content_hash: format!("{:x}", sha2::Sha256::digest(canon.as_bytes())),
            files: vec!["records.json".into()],
            locator: Some(id),
            meta: format!("{sobject} · {name}"),
        });
    }
    Ok(FetchResult {
        notes: format!("{} record(s) from SOQL", items.len()),
        items,
        next_checkpoint: None, // content hashes carry the versioning
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_gates_and_realm_derivation() {
        let ok = r#"(instance_url: "https://acme.my.salesforce.com", client_id: "3MVG9…", query: "SELECT Id, Name FROM Account")"#;
        assert!(SalesforcePlugin.validate_config(ok).is_ok());
        assert_eq!(
            SalesforcePlugin.config_auth_realm(ok).unwrap().unwrap(),
            "salesforce:acme.my.salesforce.com:3MVG9…"
        );
        for bad in [
            r#"(instance_url: "http://insecure", client_id: "c", query: "SELECT Id FROM A")"#,
            r#"(instance_url: "https://x", client_id: "c", query: "DELETE FROM A")"#,
            r#"(instance_url: "https://x", client_id: "", query: "SELECT Id FROM A")"#,
            r#"(instance_url: "https://x", client_id: "c", query: "SELECT Id FROM A", soql: "y")"#,
        ] {
            assert!(SalesforcePlugin.validate_config(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn soql_url_is_percent_encoded() {
        assert_eq!(
            percent_encode("SELECT Id FROM Account WHERE Name = 'Acme & Co'"),
            "SELECT%20Id%20FROM%20Account%20WHERE%20Name%20%3D%20%27Acme%20%26%20Co%27"
        );
    }
}
