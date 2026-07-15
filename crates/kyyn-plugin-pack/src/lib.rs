//! `pack` — a KB template from a templates repo at a PINNED rev, imported
//! as evidence (ADR 0007 packs v2). The template's tree arrives as ordinary
//! per-file items — paths rebased to the KB's own layout — for a curation
//! agent to merge in through the normal evidence pipeline: adopt new kinds
//! by schema evolution, dismiss what the KB deliberately doesn't take.
//! Snapshot-style keyed on the rev itself: a declared pack never re-fetches
//! until its rev is bumped by proposal, and then only the CHANGED files
//! return as fresh curation work (the engine's cross-run index folds the
//! rest into re-observations).

use std::path::{Path, PathBuf};
use std::process::Command;

use kyyn_core::plugin::{
    AuthStatus, Context, Describe, FetchRequest, FetchResult, FetchStyle, Item, RunSpec,
    SourcePlugin,
};
use serde::Deserialize;
use sha2::Digest;

/// The first-party templates repo — anonymous-cloneable, so the default
/// works without ssh keys.
const DEFAULT_REPO: &str = "https://github.com/drshade/kyyn-templates";

pub struct PackPlugin;

/// Instance config from `sources.ron`. Unknown keys are validation errors,
/// never silently ignored.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    /// The template directory to import (e.g. `gbrain`).
    template: String,
    /// FULL 40-hex commit OID of the templates repo — the trust anchor and
    /// the snapshot's version identity. Updating a pack = bumping this by
    /// proposal and fetching.
    rev: String,
    /// Templates repository. Default: the first-party kyyn-templates.
    #[serde(default = "default_repo")]
    repo: String,
}

fn default_repo() -> String {
    DEFAULT_REPO.into()
}

fn parse_config(text: &str) -> Result<Config, String> {
    let value: ron::Value = ron::from_str(text).map_err(|e| format!("pack config: {e}"))?;
    value
        .into_rust()
        .map_err(|e| format!("pack config shape (template, rev; optional repo): {e}"))
}

/// The template's own manifest, read LENIENTLY — the plugin needs only the
/// substitutions field (the engine parses manifests strictly).
#[derive(Deserialize, Default)]
#[serde(default)]
struct TemplateManifest {
    substitutions: Vec<ron::Value>,
}

impl SourcePlugin for PackPlugin {
    fn describe(&self) -> Describe {
        Describe {
            name: "pack".into(),
            link_namespace: "pack".into(),
            fetch_style: FetchStyle::Snapshot,
            auth_realm: None,
            protocol: kyyn_core::plugin::PROTOCOL,
        }
    }

    fn validate_config(&self, config: &str) -> Result<(), String> {
        let c = parse_config(config)?;
        if c.template.is_empty()
            || !c
                .template
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        {
            return Err(format!("template '{}' must be a bare token", c.template));
        }
        if c.rev.len() != 40 || !c.rev.chars().all(|ch| ch.is_ascii_hexdigit()) {
            return Err(format!(
                "rev '{}' is not a full 40-hex commit OID — movable ref names are not pins",
                c.rev
            ));
        }
        if c.repo.is_empty() || c.repo.starts_with('-') || c.repo.chars().any(char::is_whitespace) {
            return Err("repo must be a single non-empty token".into());
        }
        Ok(())
    }

    fn auth_status(&self, _ctx: &Context) -> Result<AuthStatus, String> {
        Ok(AuthStatus::NotRequired) // system git carries any credentials
    }

    fn authenticate(&self, _ctx: &Context) -> Result<(), String> {
        Ok(())
    }

    fn fetch(&self, req: &FetchRequest) -> Result<FetchResult, String> {
        if !matches!(req.spec, RunSpec::Snapshot) {
            return Err("pack is a snapshot source".into());
        }
        let config = parse_config(&req.config)?;
        self.validate_config(&req.config)?;
        let short: String = config.rev.chars().take(12).collect();
        // The rev IS the snapshot identity: an unbumped pack never re-clones.
        if req.checkpoint.as_deref() == Some(config.rev.as_str()) {
            return Ok(FetchResult {
                items: vec![],
                notes: format!("pack '{}' unchanged at {short}", config.template),
                next_checkpoint: Some(config.rev),
            });
        }
        kyyn_core::progress::report(&format!("fetching pack '{}' at {short}…", config.template));
        let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
        let clone_dir = tmp.path().join("templates");
        std::fs::create_dir_all(&clone_dir).map_err(|e| e.to_string())?;
        let git = |args: &[&str]| -> Result<(), String> {
            let out = Command::new("git")
                .arg("-C")
                .arg(&clone_dir)
                .args(args)
                .output()
                .map_err(|e| format!("running git {}: {e}", args.join(" ")))?;
            if !out.status.success() {
                return Err(format!(
                    "git {} failed: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            Ok(())
        };
        git(&["init", "--quiet"])?;
        git(&["remote", "add", "origin", &config.repo])?;
        git(&["fetch", "--quiet", "--depth", "1", "origin", &config.rev])?;
        git(&["checkout", "--quiet", "--detach", "FETCH_HEAD"])?;

        let template_dir = clone_dir.join(&config.template);
        let manifest_path = template_dir.join("kyyn-template.ron");
        if !manifest_path.is_file() {
            return Err(format!(
                "repo at {short} offers no template '{}' (no {}/kyyn-template.ron)",
                config.template, config.template
            ));
        }
        // Substitution-bearing templates carry dummy identities meant to be
        // swapped at init — imported verbatim they'd smuggle
        // `kyyn-template.invalid`-grade values into curation. Refuse.
        let manifest: TemplateManifest = std::fs::read_to_string(&manifest_path)
            .map_err(|e| e.to_string())
            .and_then(|t| ron::from_str(&t).map_err(|e| format!("kyyn-template.ron: {e}")))?;
        if !manifest.substitutions.is_empty() {
            return Err(format!(
                "template '{}' declares identity substitutions — it is an init-time \
                 starter, not an importable pack",
                config.template
            ));
        }

        let mut files: Vec<PathBuf> = Vec::new();
        walk(&template_dir, &mut |p| files.push(p.to_path_buf()))?;
        files.sort();
        let mut items = Vec::new();
        for path in files {
            // REBASED: the item's identity is the path the file would have
            // in the IMPORTING KB (`schema/src/model.rs`), so provenance
            // links read as what they describe.
            let rel = path
                .strip_prefix(&template_dir)
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .to_string();
            if rel == "kyyn-template.ron" {
                continue; // describes the template, not the KB
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
                kind: "file".into(),
                version: None,
                content_hash: hash,
                files: vec![rel.clone()],
                locator: None,
                file_hashes: Default::default(),
                meta: format!("{} · {rel} · {short}", config.template),
            });
        }
        kyyn_core::progress::report(&format!(
            "pack '{}': {} file(s) at {short}",
            config.template,
            items.len()
        ));
        Ok(FetchResult {
            items,
            notes: format!("pack '{}' at {short}", config.template),
            next_checkpoint: Some(config.rev),
        })
    }
}

/// Depth-first walk, files only, `.git` excluded. Entry errors are ERRORS —
/// a directory that cannot be read must not read as "nothing new".
fn walk(dir: &Path, f: &mut impl FnMut(&Path)) -> Result<(), String> {
    for entry in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = entry.path();
        if entry.file_name() == ".git" || entry.file_name() == "target" {
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

    /// A local templates repo with one importable pack and one
    /// substitution-bearing starter.
    fn fixture_repo() -> (tempfile::TempDir, String) {
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
        std::fs::create_dir_all(dir.path().join("brain/schema/src")).unwrap();
        std::fs::write(
            dir.path().join("brain/kyyn-template.ron"),
            r#"(template: 1, name: "brain", summary: "s")"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("brain/schema/src/model.rs"), "// kinds\n").unwrap();
        std::fs::write(dir.path().join("brain/sources.ron"), "(sources: [])\n").unwrap();
        std::fs::create_dir_all(dir.path().join("starter")).unwrap();
        std::fs::write(
            dir.path().join("starter/kyyn-template.ron"),
            r#"(template: 1, name: "starter", summary: "s",
                substitutions: [(key: "k", doc: "d", dummy: "D", files: ["a"])])"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("starter/a"), "D\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "--quiet", "-m", "templates"]);
        let out = Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("rev-parse");
        let rev = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (dir, rev)
    }

    fn fetch(
        repo: &Path,
        template: &str,
        rev: &str,
        checkpoint: Option<String>,
    ) -> Result<FetchResult, String> {
        let out = tempfile::tempdir().expect("out");
        PackPlugin.fetch(&FetchRequest {
            config: format!(
                r#"(template: "{template}", rev: "{rev}", repo: "{}")"#,
                repo.display()
            ),
            secrets_dir: PathBuf::from("/nonexistent"),
            out_dir: out.path().to_path_buf(),
            spec: RunSpec::Snapshot,
            checkpoint,
        })
    }

    /// The import loop: rebased per-file items with hashes, the manifest
    /// excluded, rev-keyed dedup, and starters refused.
    #[test]
    fn pack_imports_rebased_and_dedups_on_rev() {
        let (repo, rev) = fixture_repo();

        let first = fetch(repo.path(), "brain", &rev, None).expect("fetch");
        let ids: Vec<&str> = first.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            ["schema/src/model.rs", "sources.ron"],
            "rebased, manifest excluded"
        );
        assert!(first.items.iter().all(|i| i.content_hash.len() == 64));
        assert_eq!(first.next_checkpoint.as_deref(), Some(rev.as_str()));

        // Same rev in the checkpoint: no clone, no items.
        let second = fetch(repo.path(), "brain", &rev, Some(rev.clone())).expect("fetch");
        assert!(second.items.is_empty());
        assert!(second.notes.contains("unchanged"), "{}", second.notes);

        // A template that isn't there fails by name.
        let err = fetch(repo.path(), "ghost", &rev, None).expect_err("missing template");
        assert!(err.contains("no template 'ghost'"), "{err}");

        // A substitution-bearing starter refuses import.
        let err = fetch(repo.path(), "starter", &rev, None).expect_err("starter refused");
        assert!(err.contains("init-time starter"), "{err}");
    }

    #[test]
    fn config_gates_are_loud() {
        let ok = format!(r#"(template: "gbrain", rev: "{}")"#, "a".repeat(40));
        assert!(PackPlugin.validate_config(&ok).is_ok());
        assert!(
            PackPlugin
                .validate_config(r#"(template: "gbrain", rev: "main")"#)
                .is_err(),
            "ref names are not pins"
        );
        assert!(
            PackPlugin
                .validate_config(&format!(
                    r#"(template: "../up", rev: "{}")"#,
                    "a".repeat(40)
                ))
                .is_err(),
            "template must be a bare token"
        );
        assert!(
            PackPlugin
                .validate_config(&format!(
                    r#"(template: "g", rev: "{}", surprise: true)"#,
                    "a".repeat(40)
                ))
                .is_err(),
            "unknown keys fail by name"
        );
    }
}
