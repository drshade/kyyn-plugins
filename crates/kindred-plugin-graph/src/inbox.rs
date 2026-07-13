//! The normalized, agent-facing records written to `.inbox/`, and the pure
//! conversions from Graph payloads (incl. Sent/Received direction from the
//! owner's identity). Serialized field names match the Haskell tool's output.

use serde::Serialize;

use crate::config::Config;
use crate::graph::{ChatMember, GraphChat, GraphChatMessage, GraphEvent, GraphMessage, Recipient};

#[derive(Debug, Clone, Serialize)]
pub enum Direction {
    Sent,
    Received,
}

#[derive(Debug, Clone, Serialize)]
pub struct InboxAttendee {
    pub name: Option<String>,
    pub address: Option<String>,
}

/// One attachment as evidence: saved to a file when fetched (sha256 anchors
/// it via the record hash), or listed with a `skipped` reason when not.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxAttachment {
    pub name: String,
    pub content_type: Option<String>,
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxEmail {
    pub id: String,
    pub thread_id: Option<String>,
    pub received_date_time: String,
    pub direction: Direction,
    pub from: InboxAttendee,
    pub to: Vec<InboxAttendee>,
    pub cc: Vec<InboxAttendee>,
    pub subject: Option<String>,
    pub body_preview: Option<String>,
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<InboxAttachment>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxEvent {
    pub id: String,
    pub subject: Option<String>,
    pub start: String,
    pub end: String,
    pub organizer: Option<InboxAttendee>,
    pub attendees: Vec<InboxAttendee>,
    pub teams: bool,
    pub location: Option<String>,
    pub transcript_file: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxChatMessage {
    pub id: String,
    pub created_date_time: String,
    pub from: Option<String>,
    pub body: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxChat {
    pub id: String,
    pub topic: Option<String>,
    pub chat_type: Option<String>,
    pub members: Vec<InboxAttendee>,
    pub messages: Vec<InboxChatMessage>,
}

fn to_attendee(e: &crate::graph::EmailAddress) -> InboxAttendee {
    InboxAttendee {
        name: e.name.clone(),
        address: e.address.clone(),
    }
}

fn from_recipient(r: &Recipient) -> InboxAttendee {
    to_attendee(&r.email_address)
}

pub fn to_inbox_email(cfg: &Config, m: GraphMessage) -> InboxEmail {
    let sender_addr = m
        .from
        .as_ref()
        .and_then(|r| r.email_address.address.as_deref());
    let direction = if cfg.is_own_address(sender_addr) {
        Direction::Sent
    } else {
        Direction::Received
    };
    InboxEmail {
        id: m.id,
        thread_id: m.conversation_id,
        received_date_time: m.received_date_time,
        direction,
        from: m
            .from
            .as_ref()
            .map(from_recipient)
            .unwrap_or(InboxAttendee {
                name: None,
                address: None,
            }),
        to: m.to_recipients.iter().map(from_recipient).collect(),
        cc: m.cc_recipients.iter().map(from_recipient).collect(),
        subject: m.subject,
        body_preview: m.body_preview,
        body: m.body.map(|b| b.content),
        // Filled by the sync layer after the per-message attachment fetch.
        attachments: Vec::new(),
    }
}

pub fn to_inbox_event(transcript_file: Option<String>, e: GraphEvent) -> InboxEvent {
    InboxEvent {
        id: e.id,
        subject: e.subject,
        start: e.start.date_time,
        end: e.end.date_time,
        organizer: e.organizer.as_ref().map(from_recipient),
        attendees: e
            .attendees
            .iter()
            .map(|a| to_attendee(&a.email_address))
            .collect(),
        teams: e.is_online_meeting.unwrap_or(false),
        location: e.location.and_then(|l| l.display_name),
        transcript_file,
    }
}

pub fn to_inbox_chat_message(m: GraphChatMessage) -> InboxChatMessage {
    InboxChatMessage {
        id: m.id,
        created_date_time: m.created_date_time,
        from: m.from.and_then(|f| f.user).and_then(|u| u.display_name),
        body: m.body.map(|b| b.content),
    }
}

fn member_to_attendee(c: &ChatMember) -> InboxAttendee {
    InboxAttendee {
        name: c.display_name.clone(),
        address: c.email.clone(),
    }
}

pub fn to_inbox_chat(c: GraphChat, messages: Vec<InboxChatMessage>) -> InboxChat {
    InboxChat {
        id: c.id,
        topic: c.topic,
        chat_type: c.chat_type,
        members: c.members.iter().map(member_to_attendee).collect(),
        messages,
    }
}
