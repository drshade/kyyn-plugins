//! Pure Graph URL builders. Kept separate from the HTTP client so they can be
//! unit-tested (mirrors the Haskell `ClientSpec`).

use chrono::{DateTime, Utc};

use crate::client::GRAPH_BASE;
use crate::window::iso_utc;

/// Mail received in the half-open window `[from, to)`, optionally narrowed
/// by a config-declared OData `$filter` fragment (percent-encoded whole and
/// ANDed onto the window clause).
pub fn messages_url(from: DateTime<Utc>, to: DateTime<Utc>, mail_filter: Option<&str>) -> String {
    let narrowing = match mail_filter {
        Some(f) => format!("%20and%20({})", percent_encode(f)),
        None => String::new(),
    };
    format!(
        "{GRAPH_BASE}/me/messages\
         ?$select=id,conversationId,receivedDateTime,subject,bodyPreview,body,from,toRecipients,ccRecipients,hasAttachments\
         &$orderby=receivedDateTime%20desc\
         &$top=50\
         &$filter=receivedDateTime%20ge%20{}%20and%20receivedDateTime%20lt%20{}{narrowing}",
        iso_utc(from),
        iso_utc(to)
    )
}

/// An email's attachment LIST — metadata only, never contentBytes (large
/// payloads are streamed per attachment via [`attachment_value_url`]).
pub fn message_attachments_url(message_id: &str) -> String {
    format!(
        "{GRAPH_BASE}/me/messages/{message_id}/attachments\
         ?$select=id,name,contentType,size,isInline"
    )
}

/// One attachment's raw bytes (fileAttachment only).
pub fn attachment_value_url(message_id: &str, attachment_id: &str) -> String {
    format!("{GRAPH_BASE}/me/messages/{message_id}/attachments/{attachment_id}/$value")
}

/// Calendar events spanning the window.
pub fn calendar_view_url(from: DateTime<Utc>, to: DateTime<Utc>) -> String {
    format!(
        "{GRAPH_BASE}/me/calendarView\
         ?startDateTime={}\
         &endDateTime={}\
         &$select=id,subject,start,end,organizer,attendees,isOnlineMeeting,location,onlineMeeting\
         &$top=50",
        iso_utc(from),
        iso_utc(to)
    )
}

/// `/me/chats` with members expanded, filtered server-side to chats updated at
/// or after `from` (a chat older than the window can't hold an in-window
/// message). Graph rejects `$orderby` on lastUpdatedDateTime but supports
/// `$filter … ge`.
pub fn chats_url(from: DateTime<Utc>) -> String {
    format!(
        "{GRAPH_BASE}/me/chats?$filter=lastUpdatedDateTime%20ge%20{}&$expand=members&$top=50",
        iso_utc(from)
    )
}

/// Messages in one chat, EXPLICITLY newest-created first. Graph's default
/// order is lastModifiedDateTime desc — an old message edited recently then
/// sorts first, and a crawler that stops "once past the window start" quits
/// before newer unedited messages on later pages (silent data loss).
pub fn chat_messages_url(chat_id: &str) -> String {
    format!("{GRAPH_BASE}/chats/{chat_id}/messages?$top=50&$orderby=createdDateTime%20desc")
}

/// `/me/onlineMeetings?$filter=JoinWebUrl eq '{encoded}'`.
pub fn online_meeting_lookup_url(join_url: &str) -> String {
    format!(
        "{GRAPH_BASE}/me/onlineMeetings?$filter=JoinWebUrl%20eq%20'{}'",
        percent_encode(join_url)
    )
}

/// `/me/onlineMeetings/{id}/attendanceReports` — one per held session.
pub fn attendance_reports_url(meeting_id: &str) -> String {
    format!("{GRAPH_BASE}/me/onlineMeetings/{meeting_id}/attendanceReports")
}

/// `…/attendanceReports/{rid}/attendanceRecords` — per-participant identity,
/// total seconds attended, role, and join/leave intervals.
pub fn attendance_records_url(meeting_id: &str, report_id: &str) -> String {
    format!(
        "{GRAPH_BASE}/me/onlineMeetings/{meeting_id}/attendanceReports/{report_id}/attendanceRecords"
    )
}

/// `/me/onlineMeetings/{id}/transcripts`.
pub fn transcripts_url(meeting_id: &str) -> String {
    format!("{GRAPH_BASE}/me/onlineMeetings/{meeting_id}/transcripts")
}

/// `/me/onlineMeetings/{mid}/transcripts/{tid}/content?$format=text/vtt`.
pub fn transcript_content_url(meeting_id: &str, transcript_id: &str) -> String {
    format!(
        "{GRAPH_BASE}/me/onlineMeetings/{meeting_id}/transcripts/{transcript_id}/content?$format=text/vtt"
    )
}

/// `/shares/{token}/driveItem` — no `$select` so the pre-authenticated
/// `@microsoft.graph.downloadUrl` annotation survives.
pub fn share_drive_item_url(token: &str) -> String {
    format!("{GRAPH_BASE}/shares/{token}/driveItem")
}

/// `/drives/{driveId}/items/{itemId}/children` — one folder level, paged;
/// again no `$select` so file children keep their download annotation.
pub fn drive_item_children_url(drive_id: &str, item_id: &str) -> String {
    format!("{GRAPH_BASE}/drives/{drive_id}/items/{item_id}/children")
}

/// Percent-encode a value for use inside a Graph `$filter` string literal —
/// matches Haskell's `urlEncode True` (encode everything but unreserved marks).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The narrowing clause is percent-encoded whole and ANDed onto the
    /// window filter; no filter, no clause.
    #[test]
    fn messages_url_narrows_by_mail_filter() {
        let f = chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 5, 1, 0, 0, 0).unwrap();
        let t = chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 5, 2, 0, 0, 0).unwrap();
        let plain = messages_url(f, t, None);
        assert!(!plain.contains("%28"), "no stray parens: {plain}");
        let narrowed = messages_url(f, t, Some("from/emailAddress/address eq 'tom@x.za'"));
        assert!(narrowed.starts_with(&plain), "appended, not restructured");
        assert!(narrowed.contains("%20and%20("), "{narrowed}");
        assert!(narrowed.contains("tom%40x.za"), "{narrowed}");
    }
}
