//! `plugin-graph` — Microsoft Graph as an `ev` source: mail, calendar, Teams
//! chats and meeting transcripts, one windowed block per run. Ported from the
//! original everything-fetch; destined for the everything-plugins-core tap.

pub mod auth;
pub mod chat;
pub mod client;
pub mod config;
pub mod graph;
pub mod inbox;
pub mod sharepoint;
pub mod sync;
pub mod transcript;
pub mod urls;
pub mod window;

use kyyn_core::plugin::{
    AuthStatus, Context, Describe, FetchRequest, FetchResult, FetchStyle, RunSpec, SourcePlugin,
};

use crate::config::Config;

pub use sharepoint::SharepointFilePlugin;

fn token_path_in(secrets_dir: &std::path::Path) -> std::path::PathBuf {
    secrets_dir.join("ms-token.json")
}

fn token_path(ctx: &Context) -> std::path::PathBuf {
    token_path_in(&ctx.secrets_dir)
}

/// The realm derives from CONFIGURATION (tenant + client), never a
/// hard-coded string: unrelated tenants must not share a token.
fn realm_of(config: &str) -> Result<Option<String>, String> {
    let cfg = Config::from_ron(config).map_err(|e| format!("{e:#}"))?;
    Ok(Some(format!("ms-graph:{}:{}", cfg.tenant, cfg.client_id)))
}

/// The shared ms-graph auth surface — every graph-family plugin signs into
/// the same config-derived realm, so one sign-in covers mail, calendar,
/// meetings and chats on a tenant.
mod ms_auth {
    use super::*;

    pub fn status(ctx: &Context) -> Result<AuthStatus, String> {
        if token_path(ctx).exists() {
            Ok(AuthStatus::Authenticated(
                "token cached (verified on fetch)".into(),
            ))
        } else {
            Ok(AuthStatus::NotAuthenticated(
                "no token — sign the realm in (source_auth_begin, or `kyyn source auth`)".into(),
            ))
        }
    }

    pub fn interactive(ctx: &Context) -> Result<(), String> {
        let cfg = Config::from_ron(&ctx.config).map_err(|e| format!("{e:#}"))?;
        let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        rt.block_on(async {
            let client = reqwest::Client::new();
            auth::device_login(&client, &cfg, &token_path(ctx))
                .await
                .map(|_| ())
                .map_err(|e| format!("{e:#}"))
        })
    }

    pub fn begin(ctx: &Context) -> Result<kyyn_core::plugin::AuthChallenge, String> {
        let cfg = Config::from_ron(&ctx.config).map_err(|e| format!("{e:#}"))?;
        let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        rt.block_on(async {
            let client = reqwest::Client::new();
            let b = auth::device_begin(&client, &cfg)
                .await
                .map_err(|e| format!("{e:#}"))?;
            Ok(kyyn_core::plugin::AuthChallenge {
                verification_url: b.verification_uri,
                user_code: b.user_code,
                expires_in_secs: b.expires_in_secs,
                handle: b.device_code,
            })
        })
    }

    pub fn poll(ctx: &Context, handle: &str) -> Result<kyyn_core::plugin::AuthPollResult, String> {
        let cfg = Config::from_ron(&ctx.config).map_err(|e| format!("{e:#}"))?;
        let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        rt.block_on(async {
            let client = reqwest::Client::new();
            match auth::device_poll_once(&client, &cfg, handle, &token_path(ctx)).await {
                Ok(Some(())) => Ok(kyyn_core::plugin::AuthPollResult::Done("signed in".into())),
                Ok(None) => Ok(kyyn_core::plugin::AuthPollResult::Pending),
                Err(e) => Ok(kyyn_core::plugin::AuthPollResult::Failed(format!("{e:#}"))),
            }
        })
    }
}

/// Reject the mail-only knob on non-mail plugins — accepting and ignoring
/// it would be the silent-swallow bug wearing a new hat.
fn validate_config_no_mail_filter(config: &str, plugin: &str) -> Result<(), String> {
    let cfg = Config::from_ron(config).map_err(|e| format!("{e:#}"))?;
    if cfg.mail_filter.is_some() {
        return Err(format!(
            "mail_filter applies to the mail source ('graph-mail'), not {plugin}"
        ));
    }
    Ok(())
}

/// One windowed graph-family plugin definition: describe + auth delegation.
macro_rules! delegate_auth {
    () => {
        fn auth_status(&self, ctx: &Context) -> Result<AuthStatus, String> {
            ms_auth::status(ctx)
        }
        fn authenticate(&self, ctx: &Context) -> Result<(), String> {
            ms_auth::interactive(ctx)
        }
        fn auth_begin(&self, ctx: &Context) -> Result<kyyn_core::plugin::AuthChallenge, String> {
            ms_auth::begin(ctx)
        }
        fn auth_poll(
            &self,
            ctx: &Context,
            handle: &str,
        ) -> Result<kyyn_core::plugin::AuthPollResult, String> {
            ms_auth::poll(ctx, handle)
        }
    };
}

/// Mail only (with attachments) — narrow it with `mail_filter`.
pub struct GraphMailPlugin;

impl SourcePlugin for GraphMailPlugin {
    fn describe(&self) -> Describe {
        Describe {
            name: "graph-mail".into(),
            link_namespace: "graph".into(),
            fetch_style: FetchStyle::Windowed,
            auth_realm: Some("ms-graph".into()),
            protocol: kyyn_core::plugin::PROTOCOL,
        }
    }

    fn validate_config(&self, config: &str) -> Result<(), String> {
        Config::from_ron(config)
            .map(|_| ())
            .map_err(|e| format!("{e:#}"))
    }

    fn config_auth_realm(&self, config: &str) -> Result<Option<String>, String> {
        realm_of(config)
    }

    delegate_auth!();

    fn fetch(&self, req: &FetchRequest) -> Result<FetchResult, String> {
        let (cfg, wf, wt) = windowed_setup(&req.config, &req.spec)?;
        let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        let items = rt
            .block_on(sync::sync_mail(
                &cfg,
                &token_path_in(&req.secrets_dir),
                &req.out_dir,
                wf,
                wt,
            ))
            .map_err(|e| format!("{e:#}"))?;
        Ok(FetchResult {
            notes: format!("{} emails", items.len()),
            items,
            next_checkpoint: None, // windows are engine-computed from the ledger
        })
    }
}

/// Calendar events only — transcripts are the meetings source's job.
pub struct GraphCalendarPlugin;

impl SourcePlugin for GraphCalendarPlugin {
    fn describe(&self) -> Describe {
        Describe {
            name: "graph-calendar".into(),
            link_namespace: "graph".into(),
            fetch_style: FetchStyle::Windowed,
            auth_realm: Some("ms-graph".into()),
            protocol: kyyn_core::plugin::PROTOCOL,
        }
    }

    fn validate_config(&self, config: &str) -> Result<(), String> {
        validate_config_no_mail_filter(config, "graph-calendar")
    }

    fn config_auth_realm(&self, config: &str) -> Result<Option<String>, String> {
        realm_of(config)
    }

    delegate_auth!();

    fn fetch(&self, req: &FetchRequest) -> Result<FetchResult, String> {
        let (cfg, wf, wt) = windowed_setup(&req.config, &req.spec)?;
        let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        let items = rt
            .block_on(sync::sync_calendar(
                &cfg,
                &token_path_in(&req.secrets_dir),
                &req.out_dir,
                wf,
                wt,
            ))
            .map_err(|e| format!("{e:#}"))?;
        Ok(FetchResult {
            notes: format!("{} events", items.len()),
            items,
            next_checkpoint: None,
        })
    }
}

/// Meetings that carry transcripts — the calendar is listed internally to
/// find them, but only transcript-bearing meetings become items.
pub struct GraphMeetingsPlugin;

impl SourcePlugin for GraphMeetingsPlugin {
    fn describe(&self) -> Describe {
        Describe {
            name: "graph-meetings".into(),
            link_namespace: "graph".into(),
            fetch_style: FetchStyle::Windowed,
            auth_realm: Some("ms-graph".into()),
            protocol: kyyn_core::plugin::PROTOCOL,
        }
    }

    fn validate_config(&self, config: &str) -> Result<(), String> {
        validate_config_no_mail_filter(config, "graph-meetings")
    }

    fn config_auth_realm(&self, config: &str) -> Result<Option<String>, String> {
        realm_of(config)
    }

    delegate_auth!();

    fn fetch(&self, req: &FetchRequest) -> Result<FetchResult, String> {
        let (cfg, wf, wt) = windowed_setup(&req.config, &req.spec)?;
        let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        let items = rt
            .block_on(sync::sync_meetings(
                &cfg,
                &token_path_in(&req.secrets_dir),
                &req.out_dir,
                wf,
                wt,
            ))
            .map_err(|e| format!("{e:#}"))?;
        Ok(FetchResult {
            notes: format!("{} meetings with transcripts", items.len()),
            items,
            next_checkpoint: None,
        })
    }
}

/// Teams chats as their own plugin: the slow crawl runs on its own cadence
/// with its own ledger/windows, sharing the ms-graph auth realm (one sign-in).
pub struct GraphChatsPlugin;

impl SourcePlugin for GraphChatsPlugin {
    fn describe(&self) -> Describe {
        Describe {
            name: "graph-chats".into(),
            link_namespace: "graph".into(),
            fetch_style: FetchStyle::Windowed,
            auth_realm: Some("ms-graph".into()),
            protocol: kyyn_core::plugin::PROTOCOL,
        }
    }

    fn validate_config(&self, config: &str) -> Result<(), String> {
        let cfg = Config::from_ron(config).map_err(|e| format!("{e:#}"))?;
        // Shared config shape, but the knob is mail-only — accepting it here
        // and ignoring it would be the silent-swallow bug wearing a new hat.
        if cfg.mail_filter.is_some() {
            return Err("mail_filter applies to the mail source ('graph'), not graph-chats".into());
        }
        Ok(())
    }

    fn config_auth_realm(&self, config: &str) -> Result<Option<String>, String> {
        realm_of(config)
    }

    delegate_auth!();

    fn fetch(&self, req: &FetchRequest) -> Result<FetchResult, String> {
        let (cfg, wf, wt) = windowed_setup(&req.config, &req.spec)?;
        let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        let items = rt
            .block_on(sync::sync_chats(
                &cfg,
                &token_path_in(&req.secrets_dir),
                &req.out_dir,
                wf,
                wt,
            ))
            .map_err(|e| format!("{e:#}"))?;
        Ok(FetchResult {
            notes: format!("{} chat messages in window", items.len()),
            items,
            next_checkpoint: None,
        })
    }
}

fn windowed_setup(
    config: &str,
    spec: &RunSpec,
) -> Result<
    (
        Config,
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
    ),
    String,
> {
    let RunSpec::Window { from, to } = spec else {
        return Err("this is a windowed source".into());
    };
    let cfg = Config::from_ron(config).map_err(|e| format!("{e:#}"))?;
    let wf = chrono::DateTime::parse_from_rfc3339(from)
        .map_err(|e| format!("window from: {e}"))?
        .with_timezone(&chrono::Utc);
    let wt = chrono::DateTime::parse_from_rfc3339(to)
        .map_err(|e| format!("window to: {e}"))?
        .with_timezone(&chrono::Utc);
    Ok((cfg, wf, wt))
}

#[cfg(test)]
mod plugin_tests {
    use super::*;

    /// The chats plugin shares the config shape but not the knob — a
    /// mail_filter there must fail validation, not sit inert.
    #[test]
    fn chats_reject_mail_filter() {
        let cfg = r#"(client_id: "c", tenant: "t", mail_filter: Some("x eq 'y'"))"#;
        assert!(GraphMailPlugin.validate_config(cfg).is_ok());
        let err = GraphChatsPlugin.validate_config(cfg).unwrap_err();
        assert!(err.contains("mail_filter"), "{err}");
    }
}
