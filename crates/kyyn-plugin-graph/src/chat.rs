//! Pure chat-message windowing logic (the tricky part of chat sync), separated
//! from the async fetch so it can be unit-tested.

use crate::graph::GraphChatMessage;

/// A real chat message (as opposed to `systemEventMessage` noise — member
/// added, call ended, …). An absent `messageType` is treated as a real message.
pub fn is_real_message(m: &GraphChatMessage) -> bool {
    matches!(m.message_type.as_deref(), Some("message") | None)
}

/// Real messages whose `createdDateTime` falls in the half-open window
/// `[from, to)`. Graph's ISO-8601 `…Z` timestamps sort lexicographically in
/// chronological order, so plain string comparison is the window test.
pub fn chat_messages_in_window(
    from_t: &str,
    to_t: &str,
    msgs: Vec<GraphChatMessage>,
) -> Vec<GraphChatMessage> {
    msgs.into_iter()
        .filter(|m| {
            is_real_message(m)
                && m.created_date_time.as_str() >= from_t
                && m.created_date_time.as_str() < to_t
        })
        .collect()
}

/// Chat messages come newest-first, so once a page contains anything older than
/// the window start, no later (older) page can contribute — stop paging.
pub fn page_reached_window_start(from_t: &str, msgs: &[GraphChatMessage]) -> bool {
    msgs.iter().any(|m| m.created_date_time.as_str() < from_t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::GraphChatMessage;

    fn msg(created: &str, mtype: Option<&str>) -> GraphChatMessage {
        GraphChatMessage {
            id: created.to_string(),
            message_type: mtype.map(String::from),
            created_date_time: created.to_string(),
            from: None,
            body: None,
        }
    }

    const FROM: &str = "2026-07-01T00:00:00Z";
    const TO: &str = "2026-07-08T00:00:00Z";

    fn page() -> Vec<GraphChatMessage> {
        vec![
            msg("2026-07-05T13:59:00Z", Some("message")),
            msg("2026-07-04T10:00:00Z", Some("systemEventMessage")),
            msg("2026-06-01T08:00:00Z", Some("message")),
        ]
    }

    #[test]
    fn keeps_only_real_messages_in_window() {
        let kept = chat_messages_in_window(FROM, TO, page());
        assert_eq!(
            kept.iter()
                .map(|m| m.created_date_time.as_str())
                .collect::<Vec<_>>(),
            vec!["2026-07-05T13:59:00Z"]
        );
    }

    #[test]
    fn drops_message_on_or_after_to_bound() {
        let kept =
            chat_messages_in_window(FROM, TO, vec![msg("2026-07-08T00:00:00Z", Some("message"))]);
        assert!(kept.is_empty());
    }

    #[test]
    fn absent_message_type_is_real() {
        let kept = chat_messages_in_window(FROM, TO, vec![msg("2026-07-03T09:00:00Z", None)]);
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn stops_paging_once_page_reaches_before_window() {
        assert!(page_reached_window_start(FROM, &page()));
    }

    #[test]
    fn keeps_paging_while_all_at_or_after_window_start() {
        assert!(!page_reached_window_start(
            FROM,
            &[msg("2026-07-05T13:59:00Z", Some("message"))]
        ));
    }
}
