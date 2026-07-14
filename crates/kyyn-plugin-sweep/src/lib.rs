//! `sweep` — the generic file sweeper: glob a directory tree into the run
//! ledger. Every matched file is one item whose id IS its root-relative path
//! (filenames carry information — timestamps, session names — so the name
//! rides the identity), versioned by content hash: an edited file is a NEW
//! version and returns as curation work, which a seen-once checkpoint could
//! never do. No auth, no network, no checkpoint — the engine's cross-run
//! index does the deduplication.

use std::path::{Path, PathBuf};

use kyyn_core::plugin::{
    AuthStatus, Context, Describe, FetchRequest, FetchResult, FetchStyle, Item, RunSpec,
    SourcePlugin,
};
use serde::Deserialize;
use sha2::Digest;

pub struct SweepPlugin;

/// Instance config from `sources.ron`. Unknown keys are validation
/// errors, never silently ignored.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    /// The tree to sweep. `~` expands to the home directory.
    root: String,
    /// Glob patterns relative to `root` (`**/*.vtt`). Path-aware: `*` stays
    /// within one directory level, `**` crosses levels. Default: everything.
    #[serde(default = "default_patterns")]
    patterns: Vec<String>,
    /// The item kind swept files carry (eval keys, links). Default `file`;
    /// an instance sweeping meeting transcripts might declare `recording`.
    #[serde(default = "default_kind")]
    kind: String,
    /// Per-file size cap in bytes — larger files are SKIPPED WITH A NOTE,
    /// never silently. Default 64 MB.
    #[serde(default = "default_max_bytes")]
    max_file_bytes: u64,
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
    // The engine hands config as RON of a `ron::Value` — the Value bridge
    // accepts it in map OR struct notation (same idiom as plugin-graph).
    let value: ron::Value = ron::from_str(text).map_err(|e| format!("sweep config: {e}"))?;
    value
        .into_rust()
        .map_err(|e| format!("sweep config shape (root, patterns, kind, max_file_bytes): {e}"))
}

fn expand_home(path: &str) -> Result<PathBuf, String> {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").map_err(|_| "$HOME not set".to_string())?;
        Ok(PathBuf::from(home).join(rest))
    } else if path == "~" {
        std::env::var("HOME")
            .map(PathBuf::from)
            .map_err(|_| "$HOME not set".to_string())
    } else {
        Ok(PathBuf::from(path))
    }
}

impl SourcePlugin for SweepPlugin {
    fn describe(&self) -> Describe {
        Describe {
            name: "sweep".into(),
            link_namespace: "file".into(),
            fetch_style: FetchStyle::Sweep,
            auth_realm: None,
            protocol: kyyn_core::plugin::PROTOCOL,
        }
    }

    fn validate_config(&self, config: &str) -> Result<(), String> {
        let c = parse_config(config)?;
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
        Ok(AuthStatus::NotRequired)
    }

    fn authenticate(&self, _ctx: &Context) -> Result<(), String> {
        Ok(())
    }

    fn fetch(&self, req: &FetchRequest) -> Result<FetchResult, String> {
        if !matches!(req.spec, RunSpec::Sweep) {
            return Err("sweep only sweeps".into());
        }
        let config = parse_config(&req.config)?;
        let root = expand_home(&config.root)?;
        if !root.is_dir() {
            return Err(format!("root {} is not a directory", root.display()));
        }
        let patterns: Vec<glob::Pattern> = config
            .patterns
            .iter()
            .map(|p| glob::Pattern::new(p).map_err(|e| format!("pattern '{p}': {e}")))
            .collect::<Result<_, _>>()?;

        let mut files: Vec<PathBuf> = Vec::new();
        walk(&root, &mut |path| files.push(path.to_path_buf()))?;
        files.sort();

        let mut items = Vec::new();
        let mut notes = Vec::new();
        let match_opts = glob::MatchOptions {
            // Path-aware: `*` stays within one path level, `**` crosses —
            // the crate default lets `*` cross `/`, so `*.txt` would match
            // `sub/notes.txt`. A dotfile matches only when the pattern says
            // so explicitly.
            require_literal_separator: true,
            require_literal_leading_dot: true,
            ..Default::default()
        };
        for path in files {
            let rel = path
                .strip_prefix(&root)
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
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            items.push(Item {
                id: rel.clone(),
                kind: config.kind.clone(),
                version: None,
                content_hash: hash,
                files: vec![rel.clone()],
                locator: None,
                // The ORIGINAL path and mtime ride along — filenames carry
                // information the agent should see.
                meta: format!("{} · modified {}", path.display(), modified),
            });
        }
        Ok(FetchResult {
            items,
            notes: notes.join("; "),
            next_checkpoint: None,
        })
    }
}

/// Depth-first walk, files only. Entry errors are ERRORS — a directory that
/// cannot be read must not read as "nothing new".
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
        // Symlinks are neither followed nor swept: a sweep root must not
        // reach outside itself.
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fetch(_root: &Path, config: &str) -> FetchResult {
        let out = tempfile::tempdir().expect("out");
        let req = FetchRequest {
            config: config.to_string(),
            secrets_dir: PathBuf::from("/nonexistent"),
            out_dir: out.path().to_path_buf(),
            spec: RunSpec::Sweep,
            checkpoint: None,
        };
        let result = SweepPlugin.fetch(&req).expect("fetch");
        // keep the tempdir alive long enough to inspect layout
        for item in &result.items {
            assert!(out.path().join(&item.files[0]).exists(), "copy exists");
        }
        result
    }

    #[test]
    fn globs_hash_and_preserve_paths() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::create_dir_all(dir.path().join("20260710T1352-manual")).unwrap();
        std::fs::write(
            dir.path().join("20260710T1352-manual/transcript.vtt"),
            "WEBVTT\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("20260710T1352-manual/audio.wav"), "RIFF").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "hi").unwrap();
        let cfg = format!(
            r#"(root: "{}", patterns: ["**/*.vtt", "*.txt"], kind: "recording")"#,
            dir.path().display()
        );
        let result = fetch(dir.path(), &cfg);
        let ids: Vec<&str> = result.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["20260710T1352-manual/transcript.vtt", "notes.txt"]
        );
        let item = &result.items[0];
        assert_eq!(item.kind, "recording");
        assert_eq!(item.content_hash.len(), 64);
        assert!(item.meta.contains("20260710T1352-manual/transcript.vtt"));
        assert!(item.meta.contains("modified"));
    }

    #[test]
    fn oversized_files_skip_with_a_note_and_config_gates() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("big.bin"), vec![0u8; 2048]).unwrap();
        let cfg = format!(
            r#"(root: "{}", max_file_bytes: 1024)"#,
            dir.path().display()
        );
        let result = fetch(dir.path(), &cfg);
        assert!(result.items.is_empty());
        assert!(result.notes.contains("skipped big.bin"), "{}", result.notes);

        assert!(
            SweepPlugin
                .validate_config(r#"(root: "/x", patterns: ["[bad"])"#)
                .is_err()
        );
        assert!(
            SweepPlugin
                .validate_config(r#"(root: "/x", kind: "no spaces")"#)
                .is_err()
        );
        assert!(SweepPlugin.validate_config(r#"(root: "/x")"#).is_ok());
        // Unknown keys fail by name, never silently ignored.
        let err = SweepPlugin
            .validate_config(r#"(root: "/x", search: "y")"#)
            .unwrap_err();
        assert!(err.contains("search"), "{err}");
    }

    #[test]
    fn star_stays_within_one_path_level() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("top.txt"), "t").unwrap();
        std::fs::write(dir.path().join("sub/nested.txt"), "n").unwrap();
        let cfg = format!(r#"(root: "{}", patterns: ["*.txt"])"#, dir.path().display());
        let ids: Vec<String> = fetch(dir.path(), &cfg)
            .items
            .into_iter()
            .map(|i| i.id)
            .collect();
        assert_eq!(ids, vec!["top.txt"], "`*` must not cross a directory level");
    }

    #[test]
    fn changed_content_changes_the_hash_version() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("a.vtt"), "one").unwrap();
        let cfg = format!(r#"(root: "{}", patterns: ["*.vtt"])"#, dir.path().display());
        let first = fetch(dir.path(), &cfg).items[0].content_hash.clone();
        std::fs::write(dir.path().join("a.vtt"), "two").unwrap();
        let second = fetch(dir.path(), &cfg).items[0].content_hash.clone();
        assert_ne!(first, second, "an edited file is a NEW version");
    }
}
