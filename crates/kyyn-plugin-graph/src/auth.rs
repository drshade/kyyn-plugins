//! Microsoft identity platform: device-code sign-in and refresh-token exchange.
//! The token cache path is supplied by the caller (kept outside the repo, e.g.
//! the plugin's engine-assigned secrets dir) — never log its contents.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::Config;

pub const SCOPES: &str = "Mail.Read Calendars.Read User.Read Chat.Read \
     OnlineMeetings.Read OnlineMeetingTranscript.Read.All \
     OnlineMeetingArtifact.Read.All \
     Files.Read.All Sites.Read.All offline_access";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenCache {
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: u64,
    expires_in: i64,
    message: String,
}

/// The web-facing two-phase flow: begin returns the code to show the human.
pub struct DeviceBegin {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in_secs: u64,
}

pub async fn device_begin(client: &reqwest::Client, cfg: &Config) -> Result<DeviceBegin> {
    let resp = client
        .post(cfg.device_code_url())
        .form(&[("client_id", cfg.client_id.as_str()), ("scope", SCOPES)])
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!(
            "device-code request failed (HTTP {}) — check client_id/tenant in the source's config (sources.ron)",
            resp.status().as_u16()
        );
    }
    let dc: DeviceCodeResponse = resp.json().await?;
    Ok(DeviceBegin {
        device_code: dc.device_code,
        user_code: dc.user_code,
        verification_uri: dc.verification_uri,
        expires_in_secs: dc.expires_in.max(0) as u64,
    })
}

/// One token poll for a pending device sign-in. `Ok(None)` = still pending;
/// `Ok(Some(()))` = token written (realm-locked, atomic).
pub async fn device_poll_once(
    client: &reqwest::Client,
    cfg: &Config,
    device_code: &str,
    token_path: &Path,
) -> Result<Option<()>> {
    let resp = client
        .post(cfg.token_url())
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("client_id", cfg.client_id.as_str()),
            ("device_code", device_code),
        ])
        .send()
        .await?;
    if resp.status().is_success() {
        let tr: TokenResponse = resp.json().await?;
        let refresh_token = tr
            .refresh_token
            .context("no refresh_token returned — is offline_access among the app's scopes?")?;
        let _realm = realm_lock(token_path)?;
        write_token_cache(
            token_path,
            &TokenCache {
                access_token: tr.access_token,
                refresh_token,
            },
        )?;
        return Ok(Some(()));
    }
    let body = resp.text().await?;
    match serde_json::from_str::<TokenError>(&body) {
        Ok(e) if e.error == "authorization_pending" || e.error == "slow_down" => Ok(None),
        Ok(e) => bail!("device login failed: {}", e.error),
        Err(_) => bail!("device login failed (unrecognised response)"),
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct TokenError {
    error: String,
}

pub fn read_token_cache(path: &Path) -> Result<Option<TokenCache>> {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).context("reading token cache"),
        Ok(text) => match serde_json::from_str(&text) {
            Ok(cache) => Ok(Some(cache)),
            Err(e) => {
                eprintln!(
                    "Warning: token cache unreadable ({e}); sign in again with `kyyn source auth <name>`"
                );
                Ok(None)
            }
        },
    }
}

pub fn write_token_cache(path: &Path, cache: &TokenCache) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let json = serde_json::to_string(cache)?;
    // Private from the first byte (0600 at creation, no chmod window), then
    // an atomic rename: a crash mid-write can never corrupt the cache or
    // leak a world-readable instant, and a concurrent reader sees old-or-new,
    // never a torn file.
    let tmp = path.with_extension("json.tmp");
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp).context("creating token cache temp file")?;
        f.write_all(json.as_bytes())
            .context("writing token cache")?;
        f.sync_all().ok();
    }
    std::fs::rename(&tmp, path).context("publishing token cache")?;
    Ok(())
}

/// Serialize refresh-rotate-write across concurrent fetches sharing a realm
/// (Run All fires graph + chats + sharepoint together): the LAST refresh
/// rotation wins the file, and an unlocked interleave can persist a refresh
/// token the provider has already revoked.
pub fn realm_lock(token_path: &Path) -> Result<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let lock_path = token_path.with_extension("lock");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        anyhow::bail!(
            "acquiring auth realm lock: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(f)
}

/// Interactive device-code sign-in: print Microsoft's instructions, then poll
/// until the user completes it.
pub async fn device_login(
    client: &reqwest::Client,
    cfg: &Config,
    token_path: &Path,
) -> Result<TokenCache> {
    let resp = client
        .post(cfg.device_code_url())
        .form(&[("client_id", cfg.client_id.as_str()), ("scope", SCOPES)])
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!(
            "device-code request failed (HTTP {}) — check client_id/tenant in the source's config (sources.ron)",
            resp.status().as_u16()
        );
    }
    let dc: DeviceCodeResponse = resp.json().await?;
    println!("{}", dc.message);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(dc.expires_in.max(0) as u64);
    loop {
        tokio::time::sleep(Duration::from_secs(dc.interval)).await;
        if tokio::time::Instant::now() >= deadline {
            bail!("device code expired — run `kyyn source auth <name>` to sign in again");
        }
        let resp = client
            .post(cfg.token_url())
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", cfg.client_id.as_str()),
                ("device_code", dc.device_code.as_str()),
            ])
            .send()
            .await?;
        if resp.status().is_success() {
            let tr: TokenResponse = resp.json().await?;
            let refresh_token = tr
                .refresh_token
                .context("no refresh_token returned — is offline_access among the app's scopes?")?;
            let cache = TokenCache {
                access_token: tr.access_token,
                refresh_token,
            };
            write_token_cache(token_path, &cache)?;
            return Ok(cache);
        }
        // Non-200: decode the OAuth error to decide whether to keep polling.
        let body = resp.text().await?;
        match serde_json::from_str::<TokenError>(&body) {
            Ok(e) if e.error == "authorization_pending" => continue,
            Ok(e) if e.error == "slow_down" => {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            Ok(e) => bail!("device login failed: {}", e.error),
            Err(_) => bail!("device login failed (unrecognised response)"),
        }
    }
}

/// Exchange the cached refresh token for a fresh access token, persisting the
/// rotated refresh token.
pub async fn refresh_access(
    client: &reqwest::Client,
    cfg: &Config,
    cache: TokenCache,
    token_path: &Path,
) -> Result<TokenCache> {
    let resp = client
        .post(cfg.token_url())
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", cfg.client_id.as_str()),
            ("refresh_token", cache.refresh_token.as_str()),
            ("scope", SCOPES),
        ])
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!(
            "token refresh failed (HTTP {}) — run `kyyn source auth <name>` to sign in again",
            resp.status().as_u16()
        );
    }
    let tr: TokenResponse = resp.json().await?;
    let cache = TokenCache {
        access_token: tr.access_token,
        refresh_token: tr.refresh_token.unwrap_or(cache.refresh_token),
    };
    write_token_cache(token_path, &cache)?;
    Ok(cache)
}

/// Load config, refresh the cached token (or fail with a clear message), and
/// return a ready reqwest client + fresh access token. The shared entry point
/// for every fetch subcommand.
pub async fn authed_client(cfg: &Config, token_path: &Path) -> Result<(reqwest::Client, String)> {
    // A per-request timeout so a truly stalled Graph response can't hang a
    // fetch forever — set well above the known-slow first /me/chats call
    // (which can take a couple of minutes) so it never aborts real work.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .context("building HTTP client")?;
    // Hold the realm lock across read-refresh-write: concurrent fetches
    // sharing this token (Run All) must serialize refresh-token rotation.
    let _realm = realm_lock(token_path)?;
    let cache = read_token_cache(token_path)?
        .context("no token cache found — run `kyyn source auth <name>` first")?;
    let cache = refresh_access(&client, cfg, cache, token_path).await?;
    Ok((client, cache.access_token))
}
