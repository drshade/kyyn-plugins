//! The `sync` subcommands: fetch one data type for a date range into its file
//! in the per-range block under `.inbox/`, or to a `--out` target. Each command
//! writes only its own output; none wipes the block.

use std::path::Path;

use anyhow::Result;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use serde::Serialize;

/// Max concurrent per-chat message fetches.
const CHAT_CONCURRENCY: usize = 8;
/// Max concurrent per-event transcript fetches.
const TRANSCRIPT_CONCURRENCY: usize = 8;
/// Per-attachment size cap — bigger files are listed but not stored.
const MAX_ATTACHMENT_BYTES: u64 = 32 * 1024 * 1024;

use crate::chat::{chat_messages_in_window, page_reached_window_start};
use crate::client::GraphClient;
use crate::config::Config;
use crate::graph::{
    GraphAttachment, GraphChat, GraphChatMessage, GraphEvent, GraphMessage, OnlineMeeting,
    Transcript,
};
use crate::inbox::{
    InboxAttachment, InboxChat, InboxChatMessage, InboxEmail, InboxEvent, to_inbox_chat,
    to_inbox_chat_message, to_inbox_email, to_inbox_event,
};
use crate::transcript::{pick_transcript_for, transcript_file_name};
use crate::urls;
use crate::window::{dedupe_by, iso_utc};

fn write_records<T: Serialize>(path: &Path, records: &[T]) -> Result<usize> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(records)?)?;
    Ok(records.len())
}

// ── Reusable fetch cores ──

/// Mail in the window, normalized and newest-first, each flagged with
/// whether the provider says it carries attachments (swept separately).
async fn emails_for(
    graph: &GraphClient,
    cfg: &Config,
    wf: chrono::DateTime<Utc>,
    wt: chrono::DateTime<Utc>,
) -> Result<Vec<(InboxEmail, bool)>> {
    let msgs: Vec<GraphMessage> = graph
        .fetch_all_pages(&urls::messages_url(wf, wt, cfg.mail_filter.as_deref()))
        .await?;
    let mut emails: Vec<(InboxEmail, bool)> = dedupe_by(msgs, |m| m.id.clone())
        .into_iter()
        .map(|m| {
            let has_att = m.has_attachments.unwrap_or(false);
            (to_inbox_email(cfg, m), has_att)
        })
        .collect();
    emails.sort_by(|a, b| b.0.received_date_time.cmp(&a.0.received_date_time));
    Ok(emails)
}

/// Fetch one email's attachments into `dir`, returning the evidence records.
/// Inline images/signature noise are skipped; oversized or non-file
/// attachments are LISTED with the reason but not stored — a listed-but-not-
/// stored attachment is still a fact about the email.
async fn attachments_for(
    graph: &GraphClient,
    message_id: &str,
    dir: &Path,
) -> Result<Vec<InboxAttachment>> {
    use sha2::Digest;
    let metas: Vec<GraphAttachment> = graph
        .fetch_all_pages(&urls::message_attachments_url(message_id))
        .await?;
    let mut out = Vec::new();
    for meta in metas {
        if meta.is_inline.unwrap_or(false) {
            continue;
        }
        let name = meta.name.clone().unwrap_or_else(|| meta.id.clone());
        let mut rec = InboxAttachment {
            name: name.clone(),
            content_type: meta.content_type.clone(),
            size: meta.size,
            file: None,
            sha256: None,
            skipped: None,
        };
        if meta.odata_type.as_deref() != Some("#microsoft.graph.fileAttachment") {
            rec.skipped = Some(format!(
                "not a file attachment ({})",
                meta.odata_type.as_deref().unwrap_or("unknown type")
            ));
            out.push(rec);
            continue;
        }
        if meta.size.unwrap_or(0) > MAX_ATTACHMENT_BYTES {
            rec.skipped = Some(format!(
                "over the {}MB cap",
                MAX_ATTACHMENT_BYTES / (1024 * 1024)
            ));
            out.push(rec);
            continue;
        }
        match graph
            .get_raw(
                &urls::attachment_value_url(message_id, &meta.id),
                "application/octet-stream",
            )
            .await?
        {
            crate::client::Fetched::Ok(bytes) => {
                // Filenames come from mail senders: keep the basename only,
                // and namespace by the attachment id hash to avoid collisions.
                let safe: String = name
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or("attachment")
                    .chars()
                    .map(|c| if c.is_control() { '_' } else { c })
                    .collect();
                let h8 = format!("{:x}", sha2::Sha256::digest(meta.id.as_bytes()));
                let rel = format!("{}-{safe}", &h8[..8]);
                std::fs::create_dir_all(dir)?;
                std::fs::write(dir.join(&rel), &bytes)?;
                rec.sha256 = Some(format!("{:x}", sha2::Sha256::digest(&bytes)));
                rec.file = Some(rel);
                rec.size = Some(bytes.len() as u64);
            }
            crate::client::Fetched::Absent => rec.skipped = Some("gone at fetch time".into()),
            crate::client::Fetched::Denied => rec.skipped = Some("access denied".into()),
        }
        out.push(rec);
    }
    Ok(out)
}

/// Raw calendar events in the window, de-duplicated and sorted by start.
async fn events_for(
    graph: &GraphClient,
    wf: chrono::DateTime<Utc>,
    wt: chrono::DateTime<Utc>,
) -> Result<Vec<GraphEvent>> {
    let evts: Vec<GraphEvent> = graph
        .fetch_all_pages(&urls::calendar_view_url(wf, wt))
        .await?;
    let mut evts = dedupe_by(evts, |e| e.id.clone());
    evts.sort_by(|a, b| a.start.date_time.cmp(&b.start.date_time));
    Ok(evts)
}

/// Teams chats active in the window with their in-window messages, sorted by id.
async fn chats_for(
    graph: &GraphClient,
    wf: chrono::DateTime<Utc>,
    wt: chrono::DateTime<Utc>,
) -> Result<Vec<InboxChat>> {
    let from_t = iso_utc(wf);
    let to_t = iso_utc(wt);
    // The first /me/chats fetch is legitimately slow on Graph (can take a
    // couple of minutes); per-chat message fetches after it are quick. Not a
    // bug — wait it out. (The HTTP client timeout is set well above it.)
    let chats: Vec<GraphChat> = graph.fetch_all_pages_soft(&urls::chats_url(wf)).await?;
    kindred_core::progress::report(&format!(
        "{} chats active in window; fetching messages…",
        chats.len()
    ));

    let (from_t, to_t) = (&from_t, &to_t);
    let mut result: Vec<InboxChat> = futures::stream::iter(chats)
        .map(|chat| async move {
            let messages = fetch_chat_messages(graph, from_t, to_t, &chat).await?;
            anyhow::Ok((!messages.is_empty()).then(|| to_inbox_chat(chat, messages)))
        })
        .buffer_unordered(CHAT_CONCURRENCY)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    result.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(result)
}

/// Mail (with attachments) into the run directory — one item per message
/// (ADR 0001): the bundle file is storage, the provider id is identity, the
/// locator finds the record in the bundle, the content hash anchors
/// evidence integrity.
pub async fn sync_mail(
    cfg: &Config,
    token_path: &Path,
    out_dir: &Path,
    wf: DateTime<Utc>,
    wt: DateTime<Utc>,
) -> Result<Vec<kindred_core::plugin::Item>> {
    let (http, token) = crate::auth::authed_client(cfg, token_path).await?;
    // Text bodies: Outlook renders HTML→text server-side, so records stay
    // small and readable and we ship no HTML parsing.
    let graph = GraphClient::new(http, token).with_text_bodies();
    let mut items = Vec::new();

    let mut flagged = emails_for(&graph, cfg, wf, wt).await?;
    kindred_core::progress::report(&format!("{} messages in window", flagged.len()));
    let with_att = flagged.iter().filter(|(_, a)| *a).count();
    let mut nth = 0usize;
    for (e, has_att) in flagged.iter_mut() {
        if *has_att {
            nth += 1;
            kindred_core::progress::report(&format!(
                "attachments {nth} of {with_att}: {}",
                e.subject.as_deref().unwrap_or("(no subject)")
            ));
            e.attachments = attachments_for(&graph, &e.id, &out_dir.join("attachments")).await?;
        }
    }
    let emails: Vec<InboxEmail> = flagged.into_iter().map(|(e, _)| e).collect();
    write_records(&out_dir.join("emails.json"), &emails)?;
    for e in &emails {
        let mut item = record_item("email", &e.id, "emails.json", &e.subject, e)?;
        for a in &e.attachments {
            if let Some(f) = &a.file {
                item.files.push(format!("attachments/{f}"));
            }
        }
        items.push(item);
    }
    Ok(items)
}

/// Calendar events into the run directory — one item per event, transcripts
/// deliberately NOT fetched (that is the meetings source's job).
pub async fn sync_calendar(
    cfg: &Config,
    token_path: &Path,
    out_dir: &Path,
    wf: DateTime<Utc>,
    wt: DateTime<Utc>,
) -> Result<Vec<kindred_core::plugin::Item>> {
    let (http, token) = crate::auth::authed_client(cfg, token_path).await?;
    let graph = GraphClient::new(http, token).with_text_bodies();
    let raw_events = events_for(&graph, wf, wt).await?;
    kindred_core::progress::report(&format!("{} events in window", raw_events.len()));
    let events: Vec<InboxEvent> = raw_events
        .into_iter()
        .map(|e| to_inbox_event(None, e))
        .collect();
    write_records(&out_dir.join("events.json"), &events)?;
    let mut items = Vec::new();
    for e in &events {
        items.push(record_item("event", &e.id, "events.json", &e.subject, e)?);
    }
    Ok(items)
}

/// Meetings WITH transcripts into the run directory — the calendar is
/// listed internally (a transcript hangs off its online meeting), but only
/// transcript-bearing events become items, kind `meeting`.
pub async fn sync_meetings(
    cfg: &Config,
    token_path: &Path,
    out_dir: &Path,
    wf: DateTime<Utc>,
    wt: DateTime<Utc>,
) -> Result<Vec<kindred_core::plugin::Item>> {
    let (http, token) = crate::auth::authed_client(cfg, token_path).await?;
    let graph = GraphClient::new(http, token).with_text_bodies();
    let raw_events = events_for(&graph, wf, wt).await?;
    kindred_core::progress::report(&format!(
        "{} events in window; checking for transcripts…",
        raw_events.len()
    ));
    let files = fetch_transcripts(&graph, &raw_events, &out_dir.join("transcripts")).await?;
    let meetings: Vec<InboxEvent> = raw_events
        .into_iter()
        .zip(files.iter())
        .filter_map(|(e, f)| {
            f.as_ref()
                .map(|name| to_inbox_event(Some(format!("transcripts/{name}")), e))
        })
        .collect();
    kindred_core::progress::report(&format!("{} meetings carry transcripts", meetings.len()));
    write_records(&out_dir.join("meetings.json"), &meetings)?;
    let mut items = Vec::new();
    for m in &meetings {
        let mut item = record_item("meeting", &m.id, "meetings.json", &m.subject, m)?;
        if let Some(t) = &m.transcript_file {
            item.files.push(t.clone());
        }
        items.push(item);
    }
    Ok(items)
}

/// One provider record as a typed item: content-hashed from its canonical
/// JSON, located inside its bundle by provider id.
fn record_item<T: Serialize>(
    kind: &str,
    id: &str,
    bundle: &str,
    subject: &Option<String>,
    record: &T,
) -> Result<kindred_core::plugin::Item> {
    use sha2::Digest;
    let canon = serde_json::to_string(record)?;
    let hash = format!("{:x}", sha2::Sha256::digest(canon.as_bytes()));
    Ok(kindred_core::plugin::Item {
        id: id.to_string(),
        kind: kind.to_string(),
        version: None,
        content_hash: hash,
        files: vec![bundle.to_string()],
        locator: Some(id.to_string()),
        meta: subject.clone().unwrap_or_default(),
    })
}

/// Teams chats only — the known-slow crawl (first /me/chats can take
/// minutes), worth its own instance and cadence.
pub async fn sync_chats(
    cfg: &Config,
    token_path: &Path,
    out_dir: &Path,
    wf: DateTime<Utc>,
    wt: DateTime<Utc>,
) -> Result<Vec<kindred_core::plugin::Item>> {
    let (http, token) = crate::auth::authed_client(cfg, token_path).await?;
    let graph = GraphClient::new(http, token);
    let chats = chats_for(&graph, wf, wt).await?;
    write_records(&out_dir.join("chats.json"), &chats)?;
    // One item per chat MESSAGE — the chat is the container, the message is
    // the provider record an agent curates and cites.
    let mut items = Vec::new();
    for chat in &chats {
        for msg in &chat.messages {
            let mut item = record_item("chat-message", &msg.id, "chats.json", &None, msg)?;
            item.meta = chat.topic.clone().unwrap_or_else(|| "chat".into());
            items.push(item);
        }
    }
    Ok(items)
}

/// For each event, fetch its transcript (if any) into `out_dir`, returning the
/// written filename per event (index-aligned to `events`). Concurrent and
/// best-effort — an event with no joinUrl / meeting / matching transcript /
/// content yields `None`.
async fn fetch_transcripts(
    graph: &GraphClient,
    events: &[GraphEvent],
    out_dir: &Path,
) -> Result<Vec<Option<String>>> {
    let indexed: Vec<(usize, Option<String>)> = futures::stream::iter(events.iter().enumerate())
        .map(
            |(i, e)| async move { anyhow::Ok((i, fetch_one_transcript(graph, e, out_dir).await?)) },
        )
        .buffer_unordered(TRANSCRIPT_CONCURRENCY)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()?;
    let mut out = vec![None; events.len()];
    for (i, f) in indexed {
        out[i] = f;
    }
    Ok(out)
}

/// Resolve one event's transcript: joinUrl → online meeting → transcript list →
/// occurrence match → VTT content, written to `out_dir`. Returns its filename,
/// or `None` at any missing step (403/404 included).
async fn fetch_one_transcript(
    graph: &GraphClient,
    e: &GraphEvent,
    out_dir: &Path,
) -> Result<Option<String>> {
    let Some(join_url) = e
        .online_meeting
        .as_ref()
        .and_then(|m| m.join_url.as_deref())
    else {
        return Ok(None);
    };
    use crate::client::PageOutcome;
    // ALL pages (bounded), not just the first; per-item denial/absence is an
    // expected skip (not organizer / no transcript), never a run failure.
    let meetings = match graph
        .fetch_item_pages::<OnlineMeeting>(&urls::online_meeting_lookup_url(join_url))
        .await?
    {
        PageOutcome::Page(m, _) => m,
        PageOutcome::Absent | PageOutcome::Denied => return Ok(None),
    };
    let Some(mtg) = meetings.into_iter().next() else {
        return Ok(None);
    };
    let transcripts = match graph
        .fetch_item_pages::<Transcript>(&urls::transcripts_url(&mtg.id))
        .await?
    {
        PageOutcome::Page(t, _) => t,
        PageOutcome::Absent | PageOutcome::Denied => return Ok(None),
    };
    let Some(chosen_id) = pick_transcript_for(&e.start, &e.end, &transcripts).map(|t| t.id.clone())
    else {
        return Ok(None);
    };
    // Absence AND per-item denial both skip this transcript (not organizer
    // is an expected state); a denied whole-listing already failed earlier.
    let bytes = match graph
        .get_raw(
            &urls::transcript_content_url(&mtg.id, &chosen_id),
            "text/vtt",
        )
        .await?
    {
        crate::client::Fetched::Ok(b) => b,
        crate::client::Fetched::Absent | crate::client::Fetched::Denied => return Ok(None),
    };
    let fname = transcript_file_name(&e.start.date_time, e.subject.as_deref(), &e.id);
    std::fs::create_dir_all(out_dir)?;
    std::fs::write(out_dir.join(&fname), bytes)?;
    Ok(Some(fname))
}

/// Page one chat's messages, keeping those in `[from, to)` and stopping as soon
/// as a page reaches past the window start (newest-first order). A chat we
/// can't read (403/404) yields no messages. Returns oldest-first.
async fn fetch_chat_messages(
    graph: &GraphClient,
    from_t: &str,
    to_t: &str,
    chat: &GraphChat,
) -> Result<Vec<InboxChatMessage>> {
    use crate::client::PageOutcome;
    let mut acc: Vec<GraphChatMessage> = Vec::new();
    let mut next = Some(urls::chat_messages_url(&chat.id));
    let mut pages = 0u32;
    while let Some(url) = next {
        match graph.get_page_soft::<GraphChatMessage>(&url).await? {
            // A chat we can't read AT ALL (left, private, expired) is an
            // expected per-item skip — but only on the FIRST page. A later
            // page vanishing mid-pagination means an INCOMPLETE collection:
            // publishing the prefix would advance the window over unseen
            // messages.
            PageOutcome::Absent | PageOutcome::Denied if pages == 0 => break,
            PageOutcome::Absent | PageOutcome::Denied => {
                anyhow::bail!(
                    "chat {} disappeared mid-pagination (page {}) — incomplete, refusing",
                    chat.id,
                    pages + 1
                );
            }
            PageOutcome::Page(page_msgs, next_link) => {
                let reached = page_reached_window_start(from_t, &page_msgs);
                acc.extend(chat_messages_in_window(from_t, to_t, page_msgs));
                next = if reached { None } else { next_link };
            }
        }
        pages += 1;
        if pages >= 500 {
            anyhow::bail!(
                "chat {} paged past 500 pages — aborting (unbounded nextLink?)",
                chat.id
            );
        }
    }
    // Sort oldest-first by timestamp rather than trusting Graph's page order.
    acc.sort_by(|a, b| a.created_date_time.cmp(&b.created_date_time));
    Ok(acc.into_iter().map(to_inbox_chat_message).collect())
}
