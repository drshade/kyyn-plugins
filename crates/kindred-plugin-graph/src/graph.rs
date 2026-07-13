//! JSON shapes of the Microsoft Graph responses we consume. Field names map to
//! Graph's camelCase payloads via `rename_all`; unknown keys are ignored.
//! Optional-in-practice fields are `Option`/defaulted so a sparse payload never
//! kills a sync.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct EmailAddress {
    pub name: Option<String>,
    pub address: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Recipient {
    pub email_address: EmailAddress,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageBody {
    #[allow(dead_code)]
    pub content_type: String,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphMessage {
    pub id: String,
    pub conversation_id: Option<String>,
    pub received_date_time: String,
    pub subject: Option<String>,
    pub body_preview: Option<String>,
    pub body: Option<MessageBody>,
    pub from: Option<Recipient>,
    #[serde(default)]
    pub to_recipients: Vec<Recipient>,
    #[serde(default)]
    pub cc_recipients: Vec<Recipient>,
    #[serde(default)]
    pub has_attachments: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphAttachment {
    pub id: String,
    #[serde(rename = "@odata.type")]
    pub odata_type: Option<String>,
    pub name: Option<String>,
    pub content_type: Option<String>,
    pub size: Option<u64>,
    pub is_inline: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DateTimeTimeZone {
    pub date_time: String,
    #[allow(dead_code)]
    pub time_zone: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attendee {
    pub email_address: EmailAddress,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphLocation {
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OnlineMeetingInfo {
    pub join_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphEvent {
    pub id: String,
    pub subject: Option<String>,
    pub start: DateTimeTimeZone,
    pub end: DateTimeTimeZone,
    pub organizer: Option<Recipient>,
    #[serde(default)]
    pub attendees: Vec<Attendee>,
    pub is_online_meeting: Option<bool>,
    pub location: Option<GraphLocation>,
    pub online_meeting: Option<OnlineMeetingInfo>,
}

/// Resolved online meeting (from `/me/onlineMeetings?$filter=JoinWebUrl eq '…'`).
#[derive(Debug, Clone, Deserialize)]
pub struct OnlineMeeting {
    pub id: String,
}

/// One transcript entry from `/me/onlineMeetings/{id}/transcripts`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transcript {
    pub id: String,
    pub created_date_time: Option<String>,
}

/// The `user` inside a chat message's `from` identitySet.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatUser {
    #[allow(dead_code)]
    pub id: Option<String>,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatMessageFrom {
    pub user: Option<ChatUser>,
}

/// One message from `/chats/{id}/messages`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphChatMessage {
    #[allow(dead_code)]
    pub id: String,
    pub message_type: Option<String>,
    pub created_date_time: String,
    pub from: Option<ChatMessageFrom>,
    pub body: Option<MessageBody>,
}

/// An expanded `conversationMember` from a chat's `members` collection.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMember {
    pub display_name: Option<String>,
    pub email: Option<String>,
}

/// One chat from `/me/chats?$expand=members`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphChat {
    pub id: String,
    pub topic: Option<String>,
    pub chat_type: Option<String>,
    #[allow(dead_code)]
    pub last_updated_date_time: Option<String>,
    #[serde(default)]
    pub members: Vec<ChatMember>,
}

/// The bits of a driveItem (`/shares/{token}/driveItem`, folder children)
/// we need.
#[derive(Debug, Clone, Deserialize)]
pub struct DriveItem {
    /// Stable Graph item id — survives renames and moves, so it is the
    /// provider identity for folder children (ADR 0001: filenames are
    /// never identity).
    pub id: Option<String>,
    pub name: String,
    #[serde(rename = "@microsoft.graph.downloadUrl")]
    pub download_url: Option<String>,
    /// Version identity for snapshot dedup.
    #[serde(rename = "eTag")]
    pub etag: Option<String>,
    #[serde(rename = "lastModifiedDateTime")]
    pub last_modified: Option<String>,
    pub size: Option<u64>,
    /// Present iff the item is a folder.
    pub folder: Option<FolderFacet>,
    #[serde(rename = "parentReference")]
    pub parent_reference: Option<ParentReference>,
}

/// The driveItem `folder` facet — presence alone marks folderness.
#[derive(Debug, Clone, Deserialize)]
pub struct FolderFacet {
    #[serde(rename = "childCount")]
    pub child_count: Option<u64>,
}

/// The driveItem `parentReference` — carries the drive a child listing needs.
#[derive(Debug, Clone, Deserialize)]
pub struct ParentReference {
    #[serde(rename = "driveId")]
    pub drive_id: Option<String>,
}
