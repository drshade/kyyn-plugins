//! `sharepoint-file` — one tracked SharePoint/OneDrive file OR folder per
//! instance, snapshot-fetched. Resolve the sharing link via Graph `/shares`;
//! a file target downloads when its version identity (eTag) moved, a folder
//! target is walked recursively with sweep semantics — glob patterns over
//! folder-relative paths — downloading only new/changed files. Ad-hoc pulls
//! are the same plugin run once.

use std::collections::BTreeMap;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use kindred_core::plugin::{
    AuthStatus, Context, Describe, FetchRequest, FetchResult, FetchStyle, Item, RunSpec,
    SourcePlugin,
};
use serde::Deserialize;
use sha2::Digest;

use crate::client::GraphClient;
use crate::config::Config;
use crate::graph::DriveItem;
use crate::urls;

pub struct SharepointFilePlugin;

// Unknown keys are validation errors, never silently ignored.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpConfig {
    /// The sharing link of the tracked file or folder.
    url: String,
    /// Azure app (client) id — default: the shared multi-tenant "kindred"
    /// registration (see [`crate::config::DEFAULT_CLIENT_ID`]).
    #[serde(default = "default_client_id")]
    client_id: String,
    /// Entra tenant GUID, or `organizations` (the default).
    #[serde(default = "default_tenant")]
    tenant: String,
    /// Folder targets only: glob patterns over folder-relative paths
    /// (`**/*.xlsx`), sweep semantics — path-aware (`*` stays within one
    /// folder level, `**` crosses), and a dotfile matches only when a
    /// pattern says so explicitly. Ignored for a file target: pointing at
    /// the file IS the selection. Default: everything.
    #[serde(default = "default_patterns")]
    patterns: Vec<String>,
    /// The item kind fetched files carry (eval keys, links). Default `file`.
    #[serde(default = "default_kind")]
    kind: String,
    /// Per-file size cap in bytes — larger files are SKIPPED WITH A NOTE,
    /// never silently. Default 64 MB.
    #[serde(default = "default_max_bytes")]
    max_file_bytes: u64,
}

fn default_client_id() -> String {
    crate::config::DEFAULT_CLIENT_ID.into()
}
fn default_tenant() -> String {
    crate::config::DEFAULT_TENANT.into()
}
fn default_patterns() -> Vec<String> {
    vec!["**/*".into()]
}
fn default_kind() -> String {
    "file".into()
}
fn default_max_bytes() -> u64 {
    64 * 1024 * 1024
}

/// Encode a sharing URL into a Graph share token: unpadded base64url,
/// prefixed with `u!`.
pub fn encode_share_url(url: &str) -> String {
    format!("u!{}", URL_SAFE_NO_PAD.encode(url.as_bytes()))
}

impl SourcePlugin for SharepointFilePlugin {
    fn describe(&self) -> Describe {
        Describe {
            name: "sharepoint-file".into(),
            link_namespace: "sharepoint".into(),
            fetch_style: FetchStyle::Snapshot,
            auth_realm: Some("ms-graph".into()),
            protocol: kindred_core::plugin::PROTOCOL,
        }
    }

    fn validate_config(&self, config: &str) -> Result<(), String> {
        let sp = parse_config(config)?;
        compile_patterns(&sp.patterns)?;
        if sp.kind.is_empty()
            || !sp
                .kind
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        {
            return Err(format!("kind '{}' must be a bare token", sp.kind));
        }
        Ok(())
    }

    fn config_auth_realm(&self, config: &str) -> Result<Option<String>, String> {
        let sp: SpConfig = parse_config(config)?;
        Ok(Some(format!("ms-graph:{}:{}", sp.tenant, sp.client_id)))
    }

    fn auth_status(&self, ctx: &Context) -> Result<AuthStatus, String> {
        if ctx.secrets_dir.join("ms-token.json").exists() {
            Ok(AuthStatus::Authenticated(
                "token cached (verified on fetch)".into(),
            ))
        } else {
            Ok(AuthStatus::NotAuthenticated(
                "no token — run `kindred source auth <name>`".into(),
            ))
        }
    }

    fn authenticate(&self, ctx: &Context) -> Result<(), String> {
        let sp: SpConfig = parse_config(&ctx.config)?;
        let cfg = graph_config(&sp);
        let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        rt.block_on(async {
            let client = reqwest::Client::new();
            crate::auth::device_login(&client, &cfg, &ctx.secrets_dir.join("ms-token.json"))
                .await
                .map(|_| ())
                .map_err(|e| format!("{e:#}"))
        })
    }

    fn auth_begin(&self, ctx: &Context) -> Result<kindred_core::plugin::AuthChallenge, String> {
        let sp: SpConfig = parse_config(&ctx.config)?;
        let cfg = graph_config(&sp);
        let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        rt.block_on(async {
            let client = reqwest::Client::new();
            let b = crate::auth::device_begin(&client, &cfg)
                .await
                .map_err(|e| format!("{e:#}"))?;
            Ok(kindred_core::plugin::AuthChallenge {
                verification_url: b.verification_uri,
                user_code: b.user_code,
                expires_in_secs: b.expires_in_secs,
                handle: b.device_code,
            })
        })
    }

    fn auth_poll(
        &self,
        ctx: &Context,
        handle: &str,
    ) -> Result<kindred_core::plugin::AuthPollResult, String> {
        let sp: SpConfig = parse_config(&ctx.config)?;
        let cfg = graph_config(&sp);
        let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        rt.block_on(async {
            let client = reqwest::Client::new();
            match crate::auth::device_poll_once(
                &client,
                &cfg,
                handle,
                &ctx.secrets_dir.join("ms-token.json"),
            )
            .await
            {
                Ok(Some(())) => Ok(kindred_core::plugin::AuthPollResult::Done(
                    "signed in".into(),
                )),
                Ok(None) => Ok(kindred_core::plugin::AuthPollResult::Pending),
                Err(e) => Ok(kindred_core::plugin::AuthPollResult::Failed(format!(
                    "{e:#}"
                ))),
            }
        })
    }

    fn fetch(&self, req: &FetchRequest) -> Result<FetchResult, String> {
        if !matches!(req.spec, RunSpec::Snapshot) {
            return Err("sharepoint-file is a snapshot source".into());
        }
        let sp: SpConfig = parse_config(&req.config)?;
        let cfg = graph_config(&sp);
        let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        rt.block_on(snapshot(&sp, &cfg, req))
            .map_err(|e| format!("{e:#}"))
    }
}

fn parse_config(text: &str) -> Result<SpConfig, String> {
    let value: ron::Value = ron::from_str(text).map_err(|e| format!("config: {e}"))?;
    value.into_rust().map_err(|e| {
        format!(
            "config (need url; optional client_id, tenant, patterns, kind, max_file_bytes): {e}"
        )
    })
}

fn compile_patterns(patterns: &[String]) -> Result<Vec<glob::Pattern>, String> {
    patterns
        .iter()
        .map(|p| glob::Pattern::new(p).map_err(|e| format!("pattern '{p}': {e}")))
        .collect()
}

/// Path-aware glob semantics: `*` stays within one path level, `**` crosses
/// levels (the crate's MatchOptions DEFAULT lets `*` cross `/` — `*.xlsx`
/// would match `archive/old.xlsx`); a dotfile matches only when the pattern
/// says so explicitly.
fn rel_matches(patterns: &[glob::Pattern], rel: &str) -> bool {
    let opts = glob::MatchOptions {
        require_literal_separator: true,
        require_literal_leading_dot: true,
        ..Default::default()
    };
    patterns.iter().any(|p| p.matches_with(rel, opts))
}

fn graph_config(sp: &SpConfig) -> Config {
    Config {
        client_id: sp.client_id.clone(),
        tenant: sp.tenant.clone(),
        owner_addresses: Vec::new(),
        mail_filter: None,
    }
}

/// The folder checkpoint: Graph item id → last published version, a RON map.
/// A file target's checkpoint is a bare version string and never parses as
/// one — that mismatch (or any other unparseable text) means one full
/// refetch, which the engine's cross-run index absorbs as re-observations.
fn parse_version_map(text: &str) -> Option<BTreeMap<String, String>> {
    ron::from_str(text).ok()
}

fn serialize_version_map(map: &BTreeMap<String, String>) -> anyhow::Result<String> {
    Ok(ron::to_string(map)?)
}

async fn snapshot(sp: &SpConfig, cfg: &Config, req: &FetchRequest) -> anyhow::Result<FetchResult> {
    let token_path = req.secrets_dir.join("ms-token.json");
    let (http, token) = crate::auth::authed_client(cfg, &token_path).await?;
    let graph = GraphClient::new(http.clone(), token);

    let meta_url = urls::share_drive_item_url(&encode_share_url(&sp.url));
    let body = match graph.get_raw(&meta_url, "application/json").await? {
        crate::client::Fetched::Ok(b) => b,
        crate::client::Fetched::Absent => anyhow::bail!(
            "share link not found (Graph 404) — the document may have been moved or deleted"
        ),
        crate::client::Fetched::Denied => anyhow::bail!(
            "share link access DENIED (Graph 403) — confirm you can open it and that \
             Files.Read.All / Sites.Read.All are granted; this run is not complete"
        ),
    };
    let item: DriveItem = serde_json::from_slice(&body)?;
    if item.folder.is_some() {
        snapshot_folder(sp, req, &graph, &http, item).await
    } else {
        snapshot_file(sp, req, &http, item).await
    }
}

/// A file target — the original single-file behavior: the sharing URL is the
/// provider id and the bare version string is the checkpoint, so existing
/// instances keep their identity and dedup state untouched.
async fn snapshot_file(
    sp: &SpConfig,
    req: &FetchRequest,
    http: &reqwest::Client,
    item: DriveItem,
) -> anyhow::Result<FetchResult> {
    use anyhow::Context as _;
    let provider_version = item.etag.clone().or(item.last_modified.clone());

    // Snapshot dedup against the ENGINE-OWNED checkpoint (the last
    // successfully published run's) — a crash before publication can no
    // longer suppress a future fetch. A provider without a version is
    // ALWAYS refetched and deduped by content hash (the literal
    // "unversioned" used to mark all future changes as unchanged).
    if let (Some(v), Some(prev)) = (provider_version.as_deref(), req.checkpoint.as_deref())
        && v == prev
    {
        return Ok(FetchResult {
            items: vec![],
            notes: format!("'{}' unchanged (version {v})", item.name),
            next_checkpoint: Some(prev.to_string()),
        });
    }
    if let Some(size) = item.size
        && size > sp.max_file_bytes
    {
        return Ok(FetchResult {
            items: vec![],
            notes: format!(
                "skipped '{}' ({size} bytes > {} cap)",
                item.name, sp.max_file_bytes
            ),
            next_checkpoint: req.checkpoint.clone(),
        });
    }

    let download_url = item
        .download_url
        .as_deref()
        .with_context(|| format!("no download URL on '{}'", item.name))?;
    let bytes = download_capped(http, download_url, sp.max_file_bytes, &item.name).await?;
    let content_hash = format!("{:x}", sha2::Sha256::digest(&bytes));
    // Content-hash dedup for unversioned providers.
    if provider_version.is_none() && req.checkpoint.as_deref() == Some(content_hash.as_str()) {
        return Ok(FetchResult {
            items: vec![],
            notes: format!("'{}' unchanged (content hash match)", item.name),
            next_checkpoint: Some(content_hash),
        });
    }
    let name = std::path::Path::new(&item.name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&item.name)
        .to_string();
    std::fs::create_dir_all(&req.out_dir)?;
    std::fs::write(req.out_dir.join(&name), &bytes)?;

    let next = provider_version
        .clone()
        .unwrap_or_else(|| content_hash.clone());
    Ok(FetchResult {
        items: vec![Item {
            id: sp.url.clone(),
            kind: sp.kind.clone(),
            version: provider_version.clone(),
            content_hash,
            files: vec![name.clone()],
            locator: None,
            meta: format!(
                "{name} · {} bytes · modified {}",
                bytes.len(),
                item.last_modified.as_deref().unwrap_or("?")
            ),
        }],
        notes: format!("'{name}' new version snapshotted"),
        next_checkpoint: Some(next),
    })
}

/// A folder target — walk it recursively, match folder-relative paths
/// against the glob patterns (sweep semantics), download only new/changed
/// files. Provider identity is the stable Graph item id, which survives
/// renames and moves (ADR 0001: filenames are never identity); the readable
/// path rides along in `files` and `meta`.
async fn snapshot_folder(
    sp: &SpConfig,
    req: &FetchRequest,
    graph: &GraphClient,
    http: &reqwest::Client,
    root: DriveItem,
) -> anyhow::Result<FetchResult> {
    use anyhow::Context as _;
    let drive_id = root
        .parent_reference
        .as_ref()
        .and_then(|p| p.drive_id.clone())
        .context("folder share carries no driveId — cannot list children")?;
    let root_id = root
        .id
        .clone()
        .context("folder share carries no item id — cannot list children")?;
    let patterns = compile_patterns(&sp.patterns).map_err(|e| anyhow::anyhow!(e))?;

    let mut notes: Vec<String> = Vec::new();
    let prev: BTreeMap<String, String> = match req.checkpoint.as_deref() {
        None => BTreeMap::new(),
        Some(text) => parse_version_map(text).unwrap_or_else(|| {
            notes.push(
                "checkpoint is not a version map (target changed file → folder?) — full refetch"
                    .into(),
            );
            BTreeMap::new()
        }),
    };

    // Depth-first walk — Graph lists one level per (paged) call. Only
    // folders recurse; every file surfaces with its folder-relative path.
    let mut stack: Vec<(String, String)> = vec![(root_id, String::new())];
    let mut found: Vec<(DriveItem, String)> = Vec::new();
    while let Some((id, prefix)) = stack.pop() {
        let children: Vec<DriveItem> = graph
            .fetch_all_pages(&urls::drive_item_children_url(&drive_id, &id))
            .await?;
        for child in children {
            let rel = if prefix.is_empty() {
                child.name.clone()
            } else {
                format!("{prefix}/{}", child.name)
            };
            if child.folder.is_some() {
                let cid = child
                    .id
                    .clone()
                    .with_context(|| format!("folder '{rel}' has no Graph id"))?;
                stack.push((cid, rel));
            } else {
                found.push((child, rel));
            }
        }
    }
    found.sort_by(|a, b| a.1.cmp(&b.1));

    let mut items = Vec::new();
    let mut next: BTreeMap<String, String> = BTreeMap::new();
    let mut unchanged = 0usize;
    for (child, rel) in found {
        if !rel_matches(&patterns, &rel) {
            continue;
        }
        let id = child
            .id
            .clone()
            .with_context(|| format!("file '{rel}' has no Graph id"))?;
        if let Some(size) = child.size
            && size > sp.max_file_bytes
        {
            notes.push(format!(
                "skipped {rel} ({size} bytes > {} cap)",
                sp.max_file_bytes
            ));
            continue;
        }
        // Version-unchanged: no download, no item — the entry carries over.
        if let (Some(etag), Some(prev_v)) = (child.etag.as_deref(), prev.get(&id))
            && etag == prev_v
        {
            next.insert(id, etag.to_string());
            unchanged += 1;
            continue;
        }
        let download_url = child
            .download_url
            .as_deref()
            .with_context(|| format!("no download URL on '{rel}'"))?;
        let bytes = download_capped(http, download_url, sp.max_file_bytes, &rel).await?;
        let content_hash = format!("{:x}", sha2::Sha256::digest(&bytes));
        // Content-hash dedup for unversioned files.
        if child.etag.is_none() && prev.get(&id) == Some(&content_hash) {
            next.insert(id, content_hash);
            unchanged += 1;
            continue;
        }
        let dest = req.out_dir.join(&rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, &bytes)?;
        next.insert(
            id.clone(),
            child.etag.clone().unwrap_or_else(|| content_hash.clone()),
        );
        items.push(Item {
            id,
            kind: sp.kind.clone(),
            version: child.etag.clone(),
            content_hash,
            files: vec![rel.clone()],
            locator: None,
            meta: format!(
                "{rel} · {} bytes · modified {}",
                bytes.len(),
                child.last_modified.as_deref().unwrap_or("?")
            ),
        });
    }
    // Deleted files (and files renamed out of the patterns) drop out of the
    // next checkpoint; if they return, they refetch.
    notes.insert(
        0,
        format!("{} new/changed, {unchanged} unchanged", items.len()),
    );
    Ok(FetchResult {
        items,
        notes: notes.join("; "),
        next_checkpoint: Some(serialize_version_map(&next)?),
    })
}

/// Plain GET, no bearer — the pre-authenticated URL carries its own
/// credential. Streamed with a hard size cap: never buffer an unbounded
/// download (provider metadata can understate the size).
async fn download_capped(
    http: &reqwest::Client,
    url: &str,
    cap: u64,
    label: &str,
) -> anyhow::Result<Vec<u8>> {
    let mut resp = http.get(url).send().await?;
    anyhow::ensure!(
        resp.status().is_success(),
        "download failed (HTTP {}) for '{label}'",
        resp.status()
    );
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if (bytes.len() + chunk.len()) as u64 > cap {
            anyhow::bail!("'{label}' exceeds the {cap}-byte cap mid-download — refusing to buffer");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_token_is_prefixed_urlsafe_and_unpadded() {
        let token = encode_share_url("https://example.sharepoint.com/:x:/r/x.xlsx?a=1");
        assert!(token.starts_with("u!"));
        assert!(!token.contains('+') && !token.contains('/') && !token.contains('='));
    }

    #[test]
    fn config_defaults_mirror_sweep_and_validation_gates() {
        let minimal = r#"(url: "https://x", client_id: "c", tenant: "t")"#;
        let sp = parse_config(minimal).unwrap();
        assert_eq!(sp.patterns, vec!["**/*"]);
        assert_eq!(sp.kind, "file");
        assert_eq!(sp.max_file_bytes, 64 * 1024 * 1024);
        assert!(SharepointFilePlugin.validate_config(minimal).is_ok());
        assert!(
            SharepointFilePlugin
                .validate_config(r#"(url: "u", client_id: "c", tenant: "t", patterns: ["[bad"])"#)
                .is_err()
        );
        assert!(
            SharepointFilePlugin
                .validate_config(r#"(url: "u", client_id: "c", tenant: "t", kind: "no spaces")"#)
                .is_err()
        );
        // Unknown keys fail by name, never silently ignored.
        let err = SharepointFilePlugin
            .validate_config(r#"(url: "u", client_id: "c", tenant: "t", search: "BEE")"#)
            .unwrap_err();
        assert!(err.contains("search"), "{err}");
    }

    #[test]
    fn patterns_are_path_aware() {
        let single = compile_patterns(&["*.xlsx".into()]).unwrap();
        assert!(rel_matches(&single, "Tracker.xlsx"));
        assert!(
            !rel_matches(&single, "Closed Trackers/Old.xlsx"),
            "`*` must not cross a folder level"
        );
        let deep = compile_patterns(&["**/*.xlsx".into()]).unwrap();
        assert!(rel_matches(&deep, "Closed Trackers/Old.xlsx"));
        assert!(rel_matches(&deep, "Tracker.xlsx"));
        let scoped = compile_patterns(&["Product Trackers/*.xlsx".into()]).unwrap();
        assert!(rel_matches(&scoped, "Product Trackers/A.xlsx"));
        assert!(!rel_matches(&scoped, "Product Trackers/sub/A.xlsx"));
        assert!(
            !rel_matches(&deep, ".hidden/x.xlsx"),
            "explicit dotfiles only"
        );
    }

    #[test]
    fn folder_checkpoint_round_trips_and_a_file_checkpoint_is_not_a_map() {
        let mut m = BTreeMap::new();
        m.insert("01ABCDEF".to_string(), "\"{ETAG},2\"".to_string());
        m.insert("01GHIJKL".to_string(), "deadbeef".to_string());
        let text = serialize_version_map(&m).unwrap();
        assert_eq!(parse_version_map(&text), Some(m));
        // A file target's checkpoint is a bare version string — never a map,
        // so a target that changed shape triggers one clean full refetch.
        assert_eq!(parse_version_map(r#""{3299524D-2E89},2""#), None);
        assert_eq!(parse_version_map(""), None);
    }
}
