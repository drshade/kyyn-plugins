//! `git-repo` — any git repository (local path or remote URL) as a source:
//! the tree at a ref, swept into hashed evidence with sweep's glob
//! semantics. Snapshot-style with the HEAD commit as the version: an
//! unchanged repo costs one `git ls-remote` and no clone; a moved HEAD
//! shallow-clones (system git — the owner's ssh keys and credential
//! helpers apply), and the engine's cross-run index turns unchanged files
//! into re-observations while edited ones return as new curation work.

use std::path::{Path, PathBuf};
use std::process::Command;

use kindred_core::plugin::{
    AuthStatus, Context, Describe, FetchRequest, FetchResult, FetchStyle, Item, RunSpec,
    SourcePlugin,
};
use serde::Deserialize;
use sha2::Digest;

pub struct GitRepoPlugin;

/// Instance config from `sources.ron`. Unknown keys are validation
/// errors, never silently ignored.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    /// Git URL or local path of the repository.
    url: String,
    /// Branch or tag to read. Default: the remote's default branch (HEAD).
    #[serde(default = "default_ref")]
    git_ref: String,
    /// Glob patterns over repo-relative paths, sweep semantics: `*` stays
    /// within one directory level, `**` crosses; dotfiles match only when
    /// a pattern says so explicitly. Default: everything.
    #[serde(default = "default_patterns")]
    patterns: Vec<String>,
    /// The item kind swept files carry (eval keys, links). Default `file`.
    #[serde(default = "default_kind")]
    kind: String,
    /// Per-file size cap in bytes — larger files are SKIPPED WITH A NOTE,
    /// never silently. Default 64 MB.
    #[serde(default = "default_max_bytes")]
    max_file_bytes: u64,
}

fn default_ref() -> String {
    "HEAD".into()
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

fn parse_config(text: &str) -> Result<Config, String> {
    let value: ron::Value = ron::from_str(text).map_err(|e| format!("git-repo config: {e}"))?;
    value.into_rust().map_err(|e| {
        format!(
            "git-repo config shape (url; optional git_ref, patterns, kind, max_file_bytes): {e}"
        )
    })
}

fn safe_token(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('-') && !s.chars().any(char::is_whitespace)
}

/// The commit OID `url`'s `git_ref` currently points at — one network (or
/// disk) round-trip, no clone. This is the snapshot's version identity.
fn resolve_head(url: &str, git_ref: &str) -> Result<String, String> {
    let out = Command::new("git")
        .args(["ls-remote", "--", url, git_ref])
        .output()
        .map_err(|e| format!("running git ls-remote: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git ls-remote failed for {url}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().next())
        .map(str::to_string)
        .filter(|oid| !oid.is_empty())
        .ok_or_else(|| format!("ref '{git_ref}' not found in {url}"))
}

impl SourcePlugin for GitRepoPlugin {
    fn describe(&self) -> Describe {
        Describe {
            name: "git-repo".into(),
            link_namespace: "repo".into(),
            fetch_style: FetchStyle::Snapshot,
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
            return Err("git_ref must be a bare branch/tag name (or HEAD)".into());
        }
        for p in &c.patterns {
            glob::Pattern::new(p).map_err(|e| format!("pattern '{p}': {e}"))?;
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

    fn auth_status(&self, _ctx: &Context) -> Result<AuthStatus, String> {
        Ok(AuthStatus::NotRequired) // system git carries the credentials
    }

    fn authenticate(&self, _ctx: &Context) -> Result<(), String> {
        Ok(())
    }

    fn fetch(&self, req: &FetchRequest) -> Result<FetchResult, String> {
        if !matches!(req.spec, RunSpec::Snapshot) {
            return Err("git-repo is a snapshot source".into());
        }
        let config = parse_config(&req.config)?;
        let head = resolve_head(&config.url, &config.git_ref)?;
        let short: String = head.chars().take(12).collect();
        // Snapshot dedup against the engine-owned checkpoint: an unchanged
        // repo never clones.
        if req.checkpoint.as_deref() == Some(head.as_str()) {
            return Ok(FetchResult {
                items: vec![],
                notes: format!("unchanged at {short}"),
                next_checkpoint: Some(head),
            });
        }
        kindred_core::progress::report(&format!("cloning {} at {short}…", config.url));
        let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
        let clone_dir = tmp.path().join("repo");
        let mut clone = Command::new("git");
        clone.args(["clone", "--quiet", "--depth", "1"]);
        if config.git_ref != "HEAD" {
            clone.args(["--branch", &config.git_ref]);
        }
        let out = clone
            .arg("--")
            .arg(&config.url)
            .arg(&clone_dir)
            .output()
            .map_err(|e| format!("running git clone: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git clone failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }

        let patterns: Vec<glob::Pattern> = config
            .patterns
            .iter()
            .map(|p| glob::Pattern::new(p).map_err(|e| format!("pattern '{p}': {e}")))
            .collect::<Result<_, _>>()?;
        let match_opts = glob::MatchOptions {
            // Path-aware: `*` within one level, `**` crosses; explicit
            // dotfiles only (sweep semantics).
            require_literal_separator: true,
            require_literal_leading_dot: true,
            ..Default::default()
        };
        let mut files: Vec<PathBuf> = Vec::new();
        walk(&clone_dir, &mut |p| files.push(p.to_path_buf()))?;
        files.sort();

        let mut items = Vec::new();
        let mut notes = Vec::new();
        for path in files {
            let rel = path
                .strip_prefix(&clone_dir)
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .to_string();
            if !patterns.iter().any(|p| p.matches_with(&rel, match_opts)) {
                continue;
            }
            let meta = std::fs::metadata(&path).map_err(|e| format!("{rel}: {e}"))?;
            if meta.len() > config.max_file_bytes {
                notes.push(format!(
                    "skipped {rel} ({} bytes > {} cap)",
                    meta.len(),
                    config.max_file_bytes
                ));
                continue;
            }
            let bytes = std::fs::read(&path).map_err(|e| format!("{rel}: {e}"))?;
            let hash = format!("{:x}", sha2::Sha256::digest(&bytes));
            let dest = req.out_dir.join(&rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(&dest, &bytes).map_err(|e| e.to_string())?;
            items.push(Item {
                id: rel.clone(),
                kind: config.kind.clone(),
                version: None,
                content_hash: hash,
                files: vec![rel.clone()],
                locator: None,
                meta: format!("{rel} · {short}"),
            });
        }
        kindred_core::progress::report(&format!("{} file(s) matched at {short}", items.len()));
        notes.insert(0, format!("tree at {short}"));
        Ok(FetchResult {
            items,
            notes: notes.join("; "),
            next_checkpoint: Some(head),
        })
    }
}

/// Depth-first walk, files only, `.git` excluded. Entry errors are ERRORS —
/// a directory that cannot be read must not read as "nothing new".
fn walk(dir: &Path, f: &mut impl FnMut(&Path)) -> Result<(), String> {
    for entry in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = entry.path();
        if entry.file_name() == ".git" {
            continue;
        }
        let ty = entry.file_type().map_err(|e| e.to_string())?;
        if ty.is_dir() {
            walk(&path, f)?;
        } else if ty.is_file() {
            f(&path);
        }
        // Symlinks are neither followed nor swept.
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("dir");
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        run(&["init", "--quiet", "-b", "main"]);
        run(&["config", "user.name", "Test"]);
        run(&["config", "user.email", "t@example.com"]);
        std::fs::create_dir_all(dir.path().join("docs")).unwrap();
        std::fs::write(dir.path().join("docs/plan.md"), "# plan\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "hello\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "--quiet", "-m", "initial"]);
        dir
    }

    fn fetch(url: &str, checkpoint: Option<String>) -> FetchResult {
        let out = tempfile::tempdir().expect("out");
        GitRepoPlugin
            .fetch(&FetchRequest {
                config: format!(r#"(url: "{url}", patterns: ["**/*.md"])"#),
                secrets_dir: PathBuf::from("/nonexistent"),
                out_dir: out.path().to_path_buf(),
                spec: RunSpec::Snapshot,
                checkpoint,
            })
            .expect("fetch")
    }

    /// The whole snapshot loop: clone + glob + hash on first fetch; a
    /// second fetch at the same HEAD is one ls-remote and NO work; a new
    /// commit is a new snapshot.
    #[test]
    fn snapshot_dedups_on_head_and_sweeps_globs() {
        let repo = fixture_repo();
        let url = repo.path().display().to_string();

        let first = fetch(&url, None);
        assert_eq!(first.items.len(), 1, "{:?}", first.notes);
        assert_eq!(first.items[0].id, "docs/plan.md");
        assert_eq!(first.items[0].content_hash.len(), 64);
        let head = first.next_checkpoint.clone().expect("checkpoint");

        let second = fetch(&url, Some(head.clone()));
        assert!(second.items.is_empty());
        assert!(second.notes.contains("unchanged"), "{}", second.notes);
        assert_eq!(second.next_checkpoint, Some(head.clone()));

        std::fs::write(repo.path().join("docs/more.md"), "more\n").unwrap();
        let run = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(args)
                .output()
                .expect("git");
        };
        run(&["add", "-A"]);
        run(&["commit", "--quiet", "-m", "more"]);
        let third = fetch(&url, Some(head));
        assert_eq!(third.items.len(), 2, "{:?}", third.notes);
    }

    #[test]
    fn config_gates_are_loud() {
        assert!(GitRepoPlugin.validate_config(r#"(url: "/x")"#).is_ok());
        assert!(
            GitRepoPlugin
                .validate_config(r#"(url: "/x", patterns: ["[bad"])"#)
                .is_err()
        );
        assert!(
            GitRepoPlugin
                .validate_config(r#"(url: "/x", git_ref: "--upload-pack=evil")"#)
                .is_err()
        );
        assert!(
            GitRepoPlugin
                .validate_config(r#"(url: "/x", search: "y")"#)
                .is_err(),
            "unknown keys fail by name"
        );
    }
}
