//! Minimal Microsoft Graph REST client: bearer GETs, `@odata` paging, and
//! Retry-After backoff on 429s. Hand-rolled — a handful of endpoints doesn't
//! justify a Graph SDK.

use std::time::Duration;

use anyhow::{Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;

pub const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";

const MAX_RETRIES: u32 = 5;
/// Hard cap on collection paging — a backstop against an `@odata.nextLink`
/// that never terminates (50/page × 500 = far more than any real window).
const MAX_PAGES: u32 = 500;

/// One page of a Graph collection; `@odata.nextLink` points at the next page.
#[derive(Deserialize)]
struct Paged<T> {
    #[serde(default = "Vec::new")]
    value: Vec<T>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

pub struct GraphClient {
    http: reqwest::Client,
    token: String,
    /// Optional `Prefer:` header sent with every GET (e.g. text email bodies).
    prefer: Option<&'static str>,
}

/// One page's typed outcome.
pub enum PageOutcome<T> {
    Page(Vec<T>, Option<String>),
    Absent,
    Denied,
}

/// A tolerant fetch's outcome: absence and denial are different facts.
pub enum Fetched {
    Ok(Vec<u8>),
    Absent,
    Denied,
}

impl GraphClient {
    pub fn new(http: reqwest::Client, token: String) -> Self {
        Self {
            http,
            token,
            prefer: None,
        }
    }

    /// Ask Outlook endpoints to render message/event bodies as PLAIN TEXT
    /// server-side — no HTML parsing on our side, and records stay readable
    /// and small. Non-Outlook endpoints ignore the preference.
    pub fn with_text_bodies(mut self) -> Self {
        self.prefer = Some("outlook.body-content-type=\"text\"");
        self
    }

    /// One bearer GET with backoff: 429 honours Retry-After; transient 5xx
    /// and network errors retry with bounded exponential backoff + jitter
    /// (a flaky minute must not turn into a permanently advanced window).
    /// The bearer token is only ever sent to the Graph origin.
    async fn request(&self, url: &str, accept: &str) -> Result<(reqwest::StatusCode, Vec<u8>)> {
        if !url.starts_with("https://graph.microsoft.com/") {
            bail!("refusing to send the Graph bearer token to a non-Graph origin: {url}");
        }
        let mut attempt: u32 = 1;
        loop {
            let backoff = |attempt: u32| {
                // 2^n seconds capped at 30, plus deterministic-ish jitter from
                // the attempt/pid (no RNG dependency needed for spacing).
                let base = 2u64.saturating_pow(attempt).min(30);
                let jitter = u64::from(std::process::id() % 7 + attempt % 5);
                Duration::from_secs(base + jitter % 5)
            };
            let mut req = self
                .http
                .get(url)
                .bearer_auth(&self.token)
                .header(reqwest::header::ACCEPT, accept);
            if let Some(p) = self.prefer {
                req = req.header("Prefer", p);
            }
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) if attempt < MAX_RETRIES => {
                    eprintln!(
                        "Graph request failed ({e}); retrying (attempt {attempt}/{MAX_RETRIES})"
                    );
                    tokio::time::sleep(backoff(attempt)).await;
                    attempt += 1;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let status = resp.status();
            if status.as_u16() == 429 && attempt < MAX_RETRIES {
                let wait = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(5)
                    .min(60);
                eprintln!(
                    "Rate-limited by Graph; waiting {wait}s (attempt {attempt}/{MAX_RETRIES})"
                );
                tokio::time::sleep(Duration::from_secs(wait)).await;
                attempt += 1;
                continue;
            }
            if status.is_server_error() && attempt < MAX_RETRIES {
                eprintln!(
                    "Graph returned HTTP {} — retrying (attempt {attempt}/{MAX_RETRIES})",
                    status.as_u16()
                );
                tokio::time::sleep(backoff(attempt)).await;
                attempt += 1;
                continue;
            }
            return Ok((status, resp.bytes().await?.to_vec()));
        }
    }

    /// GET and decode a JSON body, failing on any non-200.
    pub async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        let (status, body) = self.request(url, "application/json").await?;
        if status.is_success() {
            Ok(serde_json::from_slice(&body)?)
        } else {
            bail!("Graph GET failed (HTTP {}) for {url}", status.as_u16())
        }
    }

    /// GET raw bytes with TYPED negative outcomes: 404 is absence (the
    /// resource genuinely isn't there — transcript never produced, share
    /// deleted); 403 is DENIAL (a scope not consented or an object the user
    /// cannot read). Collapsing the two made permission failures look like
    /// successful empty runs, advancing the window over invisible data.
    pub async fn get_raw(&self, url: &str, accept: &str) -> Result<Fetched> {
        let (status, body) = self.request(url, accept).await?;
        match status.as_u16() {
            200 => Ok(Fetched::Ok(body)),
            404 => Ok(Fetched::Absent),
            403 => Ok(Fetched::Denied),
            other => bail!("Graph GET failed (HTTP {other}) for {url}"),
        }
    }

    /// Follow `@odata.nextLink` until the collection is exhausted — bounded
    /// (a nextLink that never terminates aborts loudly), and every hop is
    /// origin-checked before the bearer token travels (see `request`).
    pub async fn fetch_all_pages<T: DeserializeOwned>(&self, url: &str) -> Result<Vec<T>> {
        let mut acc = Vec::new();
        let mut next = Some(url.to_string());
        let mut pages = 0u32;
        while let Some(u) = next {
            let page: Paged<T> = self.get_json(&u).await?;
            acc.extend(page.value);
            next = page.next_link;
            pages += 1;
            // A long backfill pages for minutes — say so at coarse intervals.
            if pages.is_multiple_of(5) && next.is_some() {
                kindred_core::progress::report(&format!("{} records ({pages} pages)…", acc.len()));
            }
            if pages >= MAX_PAGES {
                bail!("paging exceeded {MAX_PAGES} pages for {url} — aborting");
            }
        }
        Ok(acc)
    }

    /// Fetch one page, tolerantly: `None` on 403/404, else the page's values +
    /// its `@odata.nextLink`. Lets a caller decide per-page whether to continue
    /// (e.g. chat messages, which stop once they pass the window start).
    pub async fn get_page_soft<T: DeserializeOwned>(&self, url: &str) -> Result<PageOutcome<T>> {
        match self.get_raw(url, "application/json").await? {
            Fetched::Absent => Ok(PageOutcome::Absent),
            // TYPED: the caller chooses policy — an inaccessible per-item
            // collection (a chat we're not in, a meeting we don't organize)
            // is an expected skip; a denied top-level listing fails the run.
            Fetched::Denied => Ok(PageOutcome::Denied),
            Fetched::Ok(body) => {
                let page: Paged<T> = serde_json::from_slice(&body)?;
                Ok(PageOutcome::Page(page.value, page.next_link))
            }
        }
    }

    /// Collect ALL pages of a per-item collection, bounded, with typed
    /// negative outcomes. First-page absence/denial reports as such; a page
    /// VANISHING mid-pagination is an incomplete collection and errors (a
    /// silent prefix would advance the window over unseen data).
    pub async fn fetch_item_pages<T: DeserializeOwned>(&self, url: &str) -> Result<PageOutcome<T>> {
        let mut acc: Vec<T> = Vec::new();
        let mut next = Some(url.to_string());
        let mut pages = 0u32;
        while let Some(u) = next {
            match self.get_page_soft::<T>(&u).await? {
                PageOutcome::Page(items, link) => {
                    acc.extend(items);
                    next = link;
                }
                PageOutcome::Absent if pages == 0 => return Ok(PageOutcome::Absent),
                PageOutcome::Denied if pages == 0 => return Ok(PageOutcome::Denied),
                PageOutcome::Absent | PageOutcome::Denied => {
                    bail!("collection disappeared mid-pagination for {u} — incomplete, refusing")
                }
            }
            pages += 1;
            if pages >= MAX_PAGES {
                bail!("paging exceeded {MAX_PAGES} pages for {url} — aborting");
            }
        }
        Ok(PageOutcome::Page(acc, None))
    }

    /// Like [`fetch_all_pages`] but tolerant: a 403/404 on any page (a scope not
    /// consented, or a resource the user can't read) yields the pages gathered
    /// so far instead of failing, so e.g. an ungranted Chat.Read never breaks a
    /// sync.
    pub async fn fetch_all_pages_soft<T: DeserializeOwned>(&self, url: &str) -> Result<Vec<T>> {
        let mut acc = Vec::new();
        let mut next = Some(url.to_string());
        let mut pages = 0u32;
        while let Some(u) = next {
            match self.get_raw(&u, "application/json").await? {
                Fetched::Absent if pages == 0 => break,
                Fetched::Absent => {
                    bail!(
                        "collection disappeared mid-pagination for {u} — an accumulated \
                         prefix must not advance the window as if complete"
                    )
                }
                // A 403 on a COLLECTION means the scope isn't granted: the
                // run must fail rather than look like a successful empty
                // window (granting the scope later would never backfill).
                Fetched::Denied => {
                    bail!(
                        "Graph denied access (HTTP 403) for {u} — the run cannot be treated as complete"
                    )
                }
                Fetched::Ok(body) => {
                    let page: Paged<T> = serde_json::from_slice(&body)?;
                    acc.extend(page.value);
                    next = page.next_link;
                }
            }
            pages += 1;
            // Guard against an endpoint whose `@odata.nextLink` never terminates
            // (or a filter Graph silently drops): bail rather than loop forever.
            if pages >= MAX_PAGES {
                bail!(
                    "paging exceeded {MAX_PAGES} pages for {url} — aborting (likely an unbounded nextLink)"
                );
            }
        }
        Ok(acc)
    }
}
