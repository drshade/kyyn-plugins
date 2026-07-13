//! Pure date-window logic: resolving `--from`/`--to` into a UTC window, naming
//! the per-range block directory, and de-duplicating fetched records.

use std::collections::HashMap;
use std::hash::Hash;

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, Utc};

/// Graph filters want plain second-precision ISO-8601 UTC.
pub fn iso_utc(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn start_of_day(day: NaiveDate) -> DateTime<Utc> {
    day.and_hms_opt(0, 0, 0).unwrap().and_utc()
}

/// Resolve the explicit fetch window. The window is **half-open** `[from, to)`:
/// the lower bound is inclusive, the upper bound exclusive. An omitted `to` runs
/// to `now`. Both bounds are precise instants — this is the thin default seam for
/// `to = now`; the real filtering (`created/received >= from AND < to`) lives in
/// each `sync_*` stream.
pub fn resolve_window(
    now: DateTime<Utc>,
    from: DateTime<Utc>,
    to: Option<DateTime<Utc>>,
) -> (DateTime<Utc>, DateTime<Utc>) {
    (from, to.unwrap_or(now))
}

/// Parse a block-bound token back into an instant, accepting BOTH forms a block
/// dir name can carry: a legacy date bound `%Y-%m-%d` (→ that date at 00:00Z) or
/// the new timestamp bound `%Y%m%dT%H%M%SZ` (→ that instant). `None` if neither
/// form matches.
pub fn parse_bound(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(start_of_day(d));
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%SZ") {
        return Some(dt.and_utc());
    }
    None
}

/// The look-ahead window for `agenda`: from the from-date (default today) out to
/// the inclusive to-date, or `days` days when no to-date is given.
pub fn resolve_agenda_window(
    now: DateTime<Utc>,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
    days: i64,
) -> (DateTime<Utc>, DateTime<Utc>) {
    let start = from.unwrap_or_else(|| now.date_naive());
    let end = match to {
        Some(d) => d + Duration::days(1),
        None => start + Duration::days(days),
    };
    (start_of_day(start), start_of_day(end))
}

/// Directory name for a block: `<from>_<to>`, each bound an FS-safe timestamp
/// `%Y%m%dT%H%M%SZ` (e.g. `20260708T193000Z_20260708T210000Z`; neither bound
/// contains `_`, so the single separator is unambiguous). An omitted `to` is
/// resolved to `now`. Legacy blocks are date-named — `parse_bound` reads both.
pub fn block_label(from: DateTime<Utc>, to: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let fmt = |d: DateTime<Utc>| d.format("%Y%m%dT%H%M%SZ").to_string();
    format!("{}_{}", fmt(from), fmt(to.unwrap_or(now)))
}

/// De-duplicate by key, keeping the last occurrence (overlapping Graph pages
/// could in principle repeat a record). Order is not preserved — callers sort.
pub fn dedupe_by<T, K, F>(items: Vec<T>, key: F) -> Vec<T>
where
    K: Eq + Hash,
    F: Fn(&T) -> K,
{
    let mut map: HashMap<K, T> = HashMap::new();
    for item in items {
        map.insert(key(&item), item);
    }
    map.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }
    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn window_is_half_open_upper_bound_exclusive() {
        let now = dt("2026-07-06T13:30:00Z");
        // bounded: [from, to) — both bounds pass through verbatim, upper is EXCLUSIVE.
        assert_eq!(
            resolve_window(
                now,
                dt("2026-07-01T09:15:00Z"),
                Some(dt("2026-07-06T11:00:00Z"))
            ),
            (dt("2026-07-01T09:15:00Z"), dt("2026-07-06T11:00:00Z"))
        );
        // open-ended: runs to now
        assert_eq!(
            resolve_window(now, dt("2026-07-01T00:00:00Z"), None),
            (dt("2026-07-01T00:00:00Z"), now)
        );
    }

    #[test]
    fn parse_bound_accepts_legacy_date_and_timestamp() {
        // legacy date bound → midnight UTC
        assert_eq!(parse_bound("2026-06-29"), Some(dt("2026-06-29T00:00:00Z")));
        // new timestamp bound → that instant
        assert_eq!(
            parse_bound("20260708T193000Z"),
            Some(dt("2026-07-08T19:30:00Z"))
        );
        // round-trips block_label's own format
        let ts = dt("2026-07-08T21:05:09Z");
        let label = block_label(ts, Some(ts), ts);
        let (from, to) = label.split_once('_').unwrap();
        assert_eq!(parse_bound(from), Some(ts));
        assert_eq!(parse_bound(to), Some(ts));
        // garbage → None
        assert_eq!(parse_bound("not-a-bound"), None);
    }

    #[test]
    fn agenda_window_defaults_and_overrides() {
        let now = dt("2026-06-26T13:30:00Z");
        assert_eq!(
            resolve_agenda_window(now, None, None, 14),
            (dt("2026-06-26T00:00:00Z"), dt("2026-07-10T00:00:00Z"))
        );
        assert_eq!(
            resolve_agenda_window(now, Some(day(2026, 7, 1)), None, 30),
            (dt("2026-07-01T00:00:00Z"), dt("2026-07-31T00:00:00Z"))
        );
        // explicit to is inclusive and ignores days
        assert_eq!(
            resolve_agenda_window(now, None, Some(day(2026, 7, 3)), 14),
            (dt("2026-06-26T00:00:00Z"), dt("2026-07-04T00:00:00Z"))
        );
    }

    #[test]
    fn block_label_bounded_and_open() {
        let now = dt("2026-06-26T14:05:00Z");
        assert_eq!(
            block_label(
                dt("2026-06-01T08:30:00Z"),
                Some(dt("2026-06-21T17:00:00Z")),
                now
            ),
            "20260601T083000Z_20260621T170000Z"
        );
        // omitted to resolves to now
        assert_eq!(
            block_label(dt("2026-06-01T00:00:00Z"), None, now),
            "20260601T000000Z_20260626T140500Z"
        );
    }

    #[test]
    fn dedupe_keeps_last_occurrence() {
        let items = vec![(1, "a"), (2, "b"), (1, "c")];
        let mut out = dedupe_by(items, |(k, _)| *k);
        out.sort();
        assert_eq!(out, vec![(1, "c"), (2, "b")]);
    }
}
