//! `kb` — federation: another kindred knowledge base's git repository as a
//! source. Each record file (and accept receipt) in the remote KB becomes a
//! hashed, immutable evidence item; the remote `registry.ron` rides along as
//! an item of its own so the curating agent can read the remote VOCABULARY
//! before translating records into the local schema. Nothing crosses into
//! local main except through the normal proposal/accept gate.
//!
//! Mechanics: a shallow clone (`--depth 1`, system git — the owner's ssh
//! keys and credential helpers apply) into a temp dir per fetch, read, emit,
//! discard. Plugins own no writable state (ADR 0001); the engine's
//! cross-run index makes unchanged records mere re-observations.

use std::path::Path;

use kindred_core::plugin::{
    AuthStatus, Context, Describe, FetchRequest, FetchResult, FetchStyle, Item, RunSpec,
    SourcePlugin,
};
use kindred_core::registry::{Affordance, Registry};
use serde::Deserialize;
use sha2::Digest;

pub struct KbPlugin;

/// Instance config from `sources.ron`. Unknown keys are validation
/// errors, never silently ignored.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    /// Git URL or local path of the remote KB's ontology repository.
    url: String,
    /// Branch or tag to read. Default `main`.
    #[serde(default = "default_ref")]
    git_ref: String,
    /// Paths swept as evidence, relative to the remote root.
    #[serde(default = "default_include")]
    include: Vec<String>,
}

fn default_ref() -> String {
    "main".into()
}
fn default_include() -> Vec<String> {
    vec!["facts/**".into(), "receipts/**".into()]
}

fn parse_config(text: &str) -> Result<Config, String> {
    let value: ron::Value = ron::from_str(text).map_err(|e| format!("kb config: {e}"))?;
    value
        .into_rust()
        .map_err(|e| format!("kb config shape (url, git_ref, include): {e}"))
}

fn safe_token(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('-') && !s.chars().any(char::is_whitespace)
}

impl SourcePlugin for KbPlugin {
    fn describe(&self) -> Describe {
        Describe {
            name: "kb".into(),
            link_namespace: "kb".into(),
            fetch_style: FetchStyle::Sweep,
            auth_realm: None,
            protocol: kindred_core::plugin::PROTOCOL,
        }
    }

    fn validate_config(&self, config: &str) -> Result<(), String> {
        let c = parse_config(config)?;
        if !safe_token(&c.url) {
            return Err("url must be a single non-empty token".into());
        }
        if !safe_token(&c.git_ref) {
            return Err("git_ref must be a bare branch/tag name".into());
        }
        for p in &c.include {
            glob::Pattern::new(p).map_err(|e| format!("include '{p}': {e}"))?;
        }
        Ok(())
    }

    fn auth_status(&self, _ctx: &Context) -> Result<AuthStatus, String> {
        Ok(AuthStatus::NotRequired) // system git carries the credentials
    }

    fn authenticate(&self, _ctx: &Context) -> Result<(), String> {
        Ok(())
    }

    fn fetch(&self, req: &FetchRequest) -> Result<FetchResult, String> {
        if !matches!(req.spec, RunSpec::Sweep) {
            return Err("kb only sweeps".into());
        }
        let config = parse_config(&req.config)?;
        let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
        let clone_dir = tmp.path().join("remote");
        let out = std::process::Command::new("git")
            .args(["clone", "--depth", "1", "--branch", &config.git_ref, "--"])
            .arg(&config.url)
            .arg(&clone_dir)
            .output()
            .map_err(|e| format!("running git: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git clone failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let head = std::process::Command::new("git")
            .args(["-C"])
            .arg(&clone_dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();

        // The remote vocabulary: registry.ron as an item of its own, and the
        // parsed registry for routing + title extraction.
        let mut items = Vec::new();
        let mut notes = vec![format!("remote HEAD {head}")];
        let registry_text = std::fs::read_to_string(clone_dir.join("registry.ron")).ok();
        let registry: Option<Registry> =
            registry_text.as_deref().and_then(|t| ron::from_str(t).ok());
        if let Some(text) = &registry_text {
            items.push(emit(
                req,
                &clone_dir,
                "registry.ron",
                "registry",
                "registry",
                format!("the remote KB's vocabulary · HEAD {head}"),
                text.as_bytes(),
            )?);
        } else {
            notes.push("remote has no readable registry.ron — records swept as plain files".into());
        }
        let routes: Vec<(String, regex_lite::Regex, Option<String>)> = registry
            .as_ref()
            .map(|r| {
                r.kinds
                    .iter()
                    .filter(|k| k.storage.contains('{'))
                    .map(|k| {
                        let title = r
                            .affordance_field(k, Affordance::Title)
                            .map(|f| f.name.clone());
                        (k.name.clone(), storage_regex(&k.storage), title)
                    })
                    .collect()
            })
            .unwrap_or_default();

        let patterns: Vec<glob::Pattern> = config
            .include
            .iter()
            .map(|p| glob::Pattern::new(p).map_err(|e| e.to_string()))
            .collect::<Result<_, _>>()?;
        let mut files = Vec::new();
        walk(&clone_dir, &mut |p| files.push(p.to_path_buf()))?;
        files.sort();
        for path in files {
            let rel = path
                .strip_prefix(&clone_dir)
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .to_string();
            if rel.starts_with(".git/") || !patterns.iter().any(|p| p.matches(&rel)) {
                continue;
            }
            let bytes = std::fs::read(&path).map_err(|e| format!("{rel}: {e}"))?;
            if rel.starts_with("receipts/") {
                let uid = rel
                    .strip_prefix("receipts/")
                    .and_then(|r| r.strip_suffix(".ron"))
                    .unwrap_or(&rel)
                    .to_string();
                let meta = receipt_meta(&bytes).unwrap_or_default();
                items.push(emit(req, &clone_dir, &rel, "receipt", &uid, meta, &bytes)?);
                continue;
            }
            // A record: route through the remote registry for kind + id +
            // title; unrouted files sweep as kind `file` (id = path).
            match routes.iter().find_map(|(kind, re, title)| {
                re.captures(&rel).map(|c| {
                    let id = (1..c.len())
                        .filter_map(|i| c.get(i).map(|m| m.as_str()))
                        .collect::<Vec<_>>()
                        .join("/");
                    (kind.clone(), id, title.clone())
                })
            }) {
                Some((kind, id, title_field)) => {
                    let meta = title_field
                        .and_then(|f| record_title(&bytes, &f))
                        .unwrap_or_default();
                    items.push(emit(req, &clone_dir, &rel, &kind, &id, meta, &bytes)?);
                }
                None => {
                    items.push(emit(
                        req,
                        &clone_dir,
                        &rel,
                        "file",
                        &rel,
                        String::new(),
                        &bytes,
                    )?);
                }
            }
        }
        Ok(FetchResult {
            items,
            notes: notes.join(" · "),
            next_checkpoint: None,
        })
    }
}

/// Copy one remote file into the run dir and shape its item.
fn emit(
    req: &FetchRequest,
    _clone: &Path,
    rel: &str,
    kind: &str,
    id: &str,
    meta: String,
    bytes: &[u8],
) -> Result<Item, String> {
    let dest = req.out_dir.join(rel);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&dest, bytes).map_err(|e| e.to_string())?;
    Ok(Item {
        id: id.to_string(),
        kind: kind.to_string(),
        version: None,
        content_hash: format!("{:x}", sha2::Sha256::digest(bytes)),
        files: vec![rel.to_string()],
        locator: None,
        meta,
    })
}

/// `facts/people/{id}.ron` → an anchored regex with one capture per
/// placeholder — the same identity convention the engine's own router uses.
fn storage_regex(storage: &str) -> regex_lite::Regex {
    let mut pattern = String::from("^");
    let mut rest = storage;
    while let Some(open) = rest.find('{') {
        pattern.push_str(&regex_lite::escape(&rest[..open]));
        let close = rest[open..]
            .find('}')
            .map(|i| open + i)
            .unwrap_or(rest.len());
        pattern.push_str("([^/]+)");
        rest = rest.get(close + 1..).unwrap_or("");
    }
    pattern.push_str(&regex_lite::escape(rest));
    pattern.push('$');
    regex_lite::Regex::new(&pattern).expect("storage pattern regex")
}

/// The record's Title-affordance value, best-effort via the RON value tree.
fn record_title(bytes: &[u8], field: &str) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    let value: ron::Value = ron::from_str(text).ok()?;
    let ron::Value::Map(map) = value else {
        return None;
    };
    map.iter().find_map(|(k, v)| match (k, v) {
        (ron::Value::String(k), ron::Value::String(v)) if k == field => Some(v.clone()),
        _ => None,
    })
}

/// `proposal_slug` + `accepted_at` off a receipt, best-effort.
fn receipt_meta(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    let value: ron::Value = ron::from_str(text).ok()?;
    let ron::Value::Map(map) = value else {
        return None;
    };
    let get = |name: &str| {
        map.iter().find_map(|(k, v)| match (k, v) {
            (ron::Value::String(k), ron::Value::String(v)) if k == name => Some(v.clone()),
            _ => None,
        })
    };
    Some(format!(
        "accepted '{}' at {}",
        get("proposal_slug").unwrap_or_default(),
        get("accepted_at").unwrap_or_default()
    ))
}

/// Depth-first walk, files only, symlinks neither followed nor swept.
fn walk(dir: &Path, f: &mut impl FnMut(&Path)) -> Result<(), String> {
    for entry in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = entry.path();
        let ty = entry.file_type().map_err(|e| e.to_string())?;
        if ty.is_dir() {
            walk(&path, f)?;
        } else if ty.is_file() {
            f(&path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A miniature remote KB: git repo with registry.ron, two people, one
    /// receipt — the plugin must route kinds/ids/titles through the REMOTE
    /// vocabulary and carry receipts as evidence.
    fn remote_kb() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("facts/people")).unwrap();
        std::fs::create_dir_all(root.join("receipts")).unwrap();
        std::fs::write(
            root.join("registry.ron"),
            r#"(
    schema_hash: "x",
    kinds: [(
        name: "person",
        doc: "",
        storage: "facts/people/{id}.ron",
        fields: [
            (name: "id", doc: "", ty: Str),
            (name: "name", doc: "", ty: Str, role: Some("title")),
        ],
    )],
    roles: [(name: "title", doc: "", binds: Title)],
)"#,
        )
        .unwrap();
        std::fs::write(
            root.join("facts/people/jane-doe.ron"),
            r#"(id: "jane-doe", name: "Jane Doe")"#,
        )
        .unwrap();
        std::fs::write(
            root.join("facts/people/bob-ray.ron"),
            r#"(id: "bob-ray", name: "Bob Ray")"#,
        )
        .unwrap();
        std::fs::write(
            root.join("receipts/abc123.ron"),
            r#"(proposal_slug: "import-people", accepted_at: "2026-07-12T10:00:00Z")"#,
        )
        .unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "kb"]);
        dir
    }

    #[test]
    fn federates_records_registry_and_receipts() {
        let remote = remote_kb();
        let out = tempfile::tempdir().expect("out");
        let req = FetchRequest {
            config: format!(r#"(url: "{}")"#, remote.path().display()),
            secrets_dir: PathBuf::from("/nonexistent"),
            out_dir: out.path().to_path_buf(),
            spec: RunSpec::Sweep,
            checkpoint: None,
        };
        let result = KbPlugin.fetch(&req).expect("fetch");
        let mut keys: Vec<String> = result
            .items
            .iter()
            .map(|i| format!("{}:{}", i.kind, i.id))
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "person:bob-ray",
                "person:jane-doe",
                "receipt:abc123",
                "registry:registry",
            ]
        );
        let jane = result.items.iter().find(|i| i.id == "jane-doe").unwrap();
        assert_eq!(
            jane.meta, "Jane Doe",
            "title routed through the REMOTE registry"
        );
        assert_eq!(jane.content_hash.len(), 64);
        assert!(out.path().join("facts/people/jane-doe.ron").exists());
        let receipt = result.items.iter().find(|i| i.kind == "receipt").unwrap();
        assert!(receipt.meta.contains("import-people"), "{}", receipt.meta);
        assert!(result.notes.contains("remote HEAD"));
    }

    #[test]
    fn config_gates_and_multi_placeholder_storage_routes() {
        assert!(KbPlugin.validate_config(r#"(url: "x y")"#).is_err());
        assert!(
            KbPlugin
                .validate_config(r#"(url: "/kb", git_ref: "-rf")"#)
                .is_err()
        );
        assert!(
            KbPlugin
                .validate_config(r#"(url: "/kb", include: ["[bad"])"#)
                .is_err()
        );
        assert!(
            KbPlugin
                .validate_config(r#"(url: "git@github.com:x/kb.git")"#)
                .is_ok()
        );
        let re = storage_regex("facts/checkins/{person}/{date}.ron");
        let c = re
            .captures("facts/checkins/jane-doe/2026-07-06.ron")
            .unwrap();
        assert_eq!(&c[1], "jane-doe");
        assert_eq!(&c[2], "2026-07-06");
    }
}
