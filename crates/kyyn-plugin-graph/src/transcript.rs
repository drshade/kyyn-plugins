//! Pure transcript logic: Graph timestamp parsing, occurrence matching, and
//! the transcript-file naming (slugify). A recurring Teams invite shares one
//! joinUrl, so a meeting's transcript list spans every past occurrence; we
//! attribute one to a calendar event only when its createdDateTime falls in
//! `[start, end + 6h]`.

use chrono::{DateTime, Duration, Utc};

use crate::graph::{DateTimeTimeZone, Transcript};

/// Parse either Graph timestamp shape into UTC:
///   `2026-06-02T09:00:00.0000000` (calendarView, no zone → treated as UTC),
///   `2026-06-02T10:01:00Z` (transcript), `2026-06-05T09:45:30.123456Z`.
pub fn parse_graph_time(s: &str) -> Option<DateTime<Utc>> {
    let s = if s.ends_with('Z') || s.ends_with('z') {
        s.to_string()
    } else {
        format!("{s}Z")
    };
    DateTime::parse_from_rfc3339(&s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Attribute a series' transcript list to a specific calendar occurrence: among
/// transcripts whose createdDateTime falls in `[start, end + 6h]`, return the
/// latest. `None` if either boundary fails to parse or none match.
pub fn pick_transcript_for<'a>(
    start: &DateTimeTimeZone,
    end: &DateTimeTimeZone,
    transcripts: &'a [Transcript],
) -> Option<&'a Transcript> {
    let start = parse_graph_time(&start.date_time)?;
    let end = parse_graph_time(&end.date_time)?;
    let window_end = end + Duration::hours(6);
    transcripts
        .iter()
        .filter_map(|t| {
            let ct = parse_graph_time(t.created_date_time.as_deref()?)?;
            (ct >= start && ct <= window_end).then_some((ct, t))
        })
        .max_by_key(|(ct, _)| *ct)
        .map(|(_, t)| t)
}

/// Lowercase, collapse runs of non-alphanumerics to a single `-`, strip
/// leading/trailing `-`, cap at 60 chars. Unicode alphanumerics pass through.
pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').chars().take(60).collect()
}

/// Attribute a series' attendance-report list to a calendar occurrence,
/// mirroring [`pick_transcript_for`]: among reports whose
/// meetingStartDateTime falls in `[start − 1h, end + 6h]` (early joiners
/// start a session before the calendar slot), return the latest.
pub fn pick_report_for<'a>(
    start: &DateTimeTimeZone,
    end: &DateTimeTimeZone,
    reports: &'a [crate::graph::AttendanceReport],
) -> Option<&'a crate::graph::AttendanceReport> {
    let start = parse_graph_time(&start.date_time)? - Duration::hours(1);
    let window_end = parse_graph_time(&end.date_time)? + Duration::hours(6);
    reports
        .iter()
        .filter_map(|r| {
            let st = parse_graph_time(r.meeting_start.as_deref()?)?;
            (st >= start && st <= window_end).then_some((st, r))
        })
        .max_by_key(|(st, _)| *st)
        .map(|(_, r)| r)
}

/// `YYYY-MM-DD-<subject-slug>-<id-suffix>.vtt`. Falls back to `untitled` when
/// the subject is absent/empty; the id suffix is the last 8 chars of the
/// slugified event id (Graph ids share a long prefix — the tail carries the
/// entropy) to disambiguate same-day same-subject meetings.
pub fn transcript_file_name(
    start_date_time: &str,
    subject: Option<&str>,
    event_id: &str,
) -> String {
    let date: String = start_date_time.chars().take(10).collect();
    let slug = match subject.map(slugify) {
        Some(s) if !s.is_empty() => s,
        _ => "untitled".to_string(),
    };
    let id_slug = slugify(event_id);
    let n = id_slug.chars().count();
    let suffix: String = id_slug.chars().skip(n.saturating_sub(8)).collect();
    format!("{date}-{slug}-{suffix}.vtt")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_picker_matches_occurrences_with_early_join_grace() {
        use crate::graph::AttendanceReport;
        let dtz = |s: &str| DateTimeTimeZone {
            date_time: s.into(),
            time_zone: String::new(),
        };
        let report = |id: &str, start: &str| AttendanceReport {
            id: id.into(),
            meeting_start: Some(start.into()),
            meeting_end: None,
            total_participant_count: None,
        };
        let reports = vec![
            report("last-week", "2026-07-06T10:02:00Z"),
            report("early-join", "2026-07-13T09:45:00Z"),
            report("in-window", "2026-07-13T10:01:00Z"),
        ];
        // Latest in [start − 1h, end + 6h] wins; early joiners still match.
        let picked = pick_report_for(
            &dtz("2026-07-13T10:00:00"),
            &dtz("2026-07-13T11:00:00"),
            &reports,
        )
        .unwrap();
        assert_eq!(picked.id, "in-window");
        // A different week's occurrence never leaks across.
        assert!(
            pick_report_for(
                &dtz("2026-07-20T10:00:00"),
                &dtz("2026-07-20T11:00:00"),
                &reports,
            )
            .is_none()
        );
    }
    use crate::graph::{DateTimeTimeZone, Transcript};

    fn dtz(s: &str) -> DateTimeTimeZone {
        DateTimeTimeZone {
            date_time: s.to_string(),
            time_zone: "UTC".to_string(),
        }
    }
    fn tr(id: &str, created: Option<&str>) -> Transcript {
        Transcript {
            id: id.to_string(),
            created_date_time: created.map(String::from),
        }
    }

    #[test]
    fn parses_the_graph_timestamp_shapes() {
        assert!(parse_graph_time("2026-06-02T09:00:00.0000000").is_some()); // calendarView, no Z
        assert!(parse_graph_time("2026-06-02T10:01:00Z").is_some()); // transcript
        assert!(parse_graph_time("2026-06-05T09:45:30.123456Z").is_some()); // fractional + Z
        assert!(parse_graph_time("not a time").is_none());
    }

    #[test]
    fn picks_latest_transcript_in_window() {
        let start = dtz("2026-06-02T09:00:00.0000000");
        let end = dtz("2026-06-02T10:00:00.0000000");
        let ts = vec![
            tr("old", Some("2026-05-26T09:30:00Z")), // previous occurrence — out
            tr("a", Some("2026-06-02T10:05:00Z")),   // in window (within +6h)
            tr("b", Some("2026-06-02T11:30:00Z")),   // in window, later
            tr("late", Some("2026-06-02T17:00:00Z")), // > end+6h — out
        ];
        assert_eq!(
            pick_transcript_for(&start, &end, &ts).map(|t| t.id.as_str()),
            Some("b")
        );
    }

    #[test]
    fn no_match_returns_none() {
        let start = dtz("2026-06-02T09:00:00Z");
        let end = dtz("2026-06-02T10:00:00Z");
        assert!(pick_transcript_for(&start, &end, &[]).is_none());
        assert!(
            pick_transcript_for(&start, &end, &[tr("x", Some("2026-06-01T09:00:00Z"))]).is_none()
        );
        // Unparseable event boundary → None.
        assert!(
            pick_transcript_for(&dtz("nope"), &end, &[tr("x", Some("2026-06-02T09:30:00Z"))])
                .is_none()
        );
    }

    #[test]
    fn slugify_and_filename() {
        assert_eq!(slugify("UK & NL: Sales!!"), "uk-nl-sales");
        assert_eq!(slugify("  ---weird--- "), "weird");
        let name = transcript_file_name(
            "2026-07-06T09:00:00.0000000",
            Some("Exco Weekly Check-in"),
            "AAkALgAAABCDEFGH",
        );
        assert!(name.starts_with("2026-07-06-exco-weekly-check-in-"));
        assert!(name.ends_with(".vtt"));
        // untitled fallback
        assert!(
            transcript_file_name("2026-07-06T09:00:00Z", None, "id123")
                .starts_with("2026-07-06-untitled-")
        );
    }
}
