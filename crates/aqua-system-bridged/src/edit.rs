//! `edit_message`: the `m.replace` content the bridge sends, its size guard,
//! and the checks on the event it replaces. Only the bridge's own text
//! messages are ever edited; clients ignore edits from other senders, and the
//! daemon does not even try.

use anyhow::{anyhow, Context, Result};
use matrix_sdk::ruma::events::room::message::{
    MessageType, Relation, ReplacementMetadata, RoomMessageEventContent,
};
use matrix_sdk::ruma::events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent};
use matrix_sdk::ruma::{EventId, OwnedEventId, RoomId, UserId};
use matrix_sdk::Client;

/// Largest serialized replacement content accepted, in bytes. Matrix caps a
/// whole event at 65,536 bytes; an edit carries the text twice (fallback and
/// `m.new_content`, each as Markdown and HTML), and Megolm base64-expands the
/// payload by 4/3 in encrypted rooms: 44,000 * 4/3 is about 58,700, which
/// leaves room for the encryption and event envelope.
pub const MAX_EDIT_CONTENT_BYTES: usize = 44_000;

/// The replacement for `original`: `markdown` rendered exactly as
/// `send_message` renders it, carried in `m.new_content`, with the `* `
/// fallback body for clients without edit support.
pub fn replacement(markdown: &str, original: OwnedEventId) -> RoomMessageEventContent {
    RoomMessageEventContent::text_markdown(markdown)
        .make_replacement(ReplacementMetadata::new(original, None))
}

/// Refuse a replacement whose encoded content would push the event past the
/// homeserver's size cap (`413 M_TOO_LARGE`). Returns the encoded size.
pub fn check_size(content: &RoomMessageEventContent) -> Result<usize, String> {
    let n = serde_json::to_vec(content)
        .map_err(|e| format!("cannot encode the edit: {e}"))?
        .len();
    if n > MAX_EDIT_CONTENT_BYTES {
        return Err(format!(
            "the edit would be a {n}-byte event (an edit carries the new text twice, as Markdown and HTML); \
             the cap is {MAX_EDIT_CONTENT_BYTES}. Shorten it, or send a new message instead."
        ));
    }
    Ok(n)
}

/// Whether `original` may be replaced by the bridge (`own`): a text
/// `m.room.message` the bridge sent itself, not redacted and not itself an
/// edit.
pub fn check_editable(own: &UserId, original: &AnySyncTimelineEvent) -> Result<(), String> {
    let id = original.event_id();
    if original.sender() != own {
        return Err(format!(
            "REFUSED: event {id} was sent by {}, not by the bridge ({own}); the bridge only edits its own messages",
            original.sender()
        ));
    }
    let msg = match original {
        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(m)) => m,
        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomEncrypted(_)) => {
            return Err(format!(
                "REFUSED: event {id} cannot be decrypted by the bridge, so it cannot be checked or edited"
            ))
        }
        other => {
            return Err(format!(
                "REFUSED: event {id} is an {} event, not an m.room.message",
                other.event_type()
            ))
        }
    };
    let Some(orig) = msg.as_original() else {
        return Err(format!(
            "REFUSED: event {id} was redacted; it cannot be edited"
        ));
    };
    if let Some(Relation::Replacement(r)) = &orig.content.relates_to {
        return Err(format!(
            "REFUSED: event {id} is itself an edit of {}; edit that original event id instead",
            r.event_id
        ));
    }
    if !matches!(orig.content.msgtype, MessageType::Text(_)) {
        return Err(format!(
            "REFUSED: event {id} is an {} message; only text messages can be edited",
            orig.content.msgtype()
        ));
    }
    Ok(())
}

/// Load `original` from `room_id` on the live Client (the homeserver answers
/// per room, so an event from another room is not found) and apply
/// [`check_editable`].
pub async fn ensure_editable(client: &Client, room_id: &str, original: &EventId) -> Result<()> {
    let rid =
        <&RoomId>::try_from(room_id).map_err(|e| anyhow!("invalid room id {room_id}: {e}"))?;
    let room = client
        .get_room(rid)
        .ok_or_else(|| anyhow!("room {room_id} is not known to the bridge"))?;
    let own = client
        .user_id()
        .ok_or_else(|| anyhow!("the bridge Client is not logged in"))?;
    let ev = room.event(original, None).await.with_context(|| {
        format!(
            "cannot load event {original} from room {room_id} (unknown id, or not in that room)"
        )
    })?;
    let parsed = ev
        .raw()
        .deserialize()
        .with_context(|| format!("event {original} could not be parsed"))?;
    check_editable(own, &parsed).map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::user_id;
    use serde_json::{json, Value};

    const OWN: &str = "@bridge:x";

    fn event(v: Value) -> AnySyncTimelineEvent {
        serde_json::from_value(v).unwrap()
    }

    fn message(sender: &str, content: Value) -> AnySyncTimelineEvent {
        event(json!({
            "type": "m.room.message",
            "event_id": "$orig:x",
            "sender": sender,
            "origin_server_ts": 1,
            "content": content,
        }))
    }

    fn text(body: &str) -> Value {
        json!({"msgtype": "m.text", "body": body})
    }

    #[test]
    fn replacement_carries_the_new_text_and_a_fallback() {
        let md = "# Train Report\n\n- **green**: 3 trains";
        let content = replacement(md, OwnedEventId::try_from("$orig:x").unwrap());
        let v = serde_json::to_value(&content).unwrap();
        assert_eq!(v["m.relates_to"]["rel_type"], "m.replace");
        assert_eq!(v["m.relates_to"]["event_id"], "$orig:x");
        assert_eq!(v["msgtype"], "m.text");
        assert_eq!(v["m.new_content"]["msgtype"], "m.text");
        assert_eq!(v["m.new_content"]["body"], md);
        assert_eq!(v["m.new_content"]["format"], "org.matrix.custom.html");
        let html = v["m.new_content"]["formatted_body"].as_str().unwrap();
        assert!(html.contains("<h1>Train Report</h1>"), "{html}");
        assert!(html.contains("<strong>green</strong>"), "{html}");
        assert!(v["body"].as_str().unwrap().starts_with("* # Train Report"));
        assert!(v["formatted_body"].as_str().unwrap().starts_with("* <h1>"));
    }

    #[test]
    fn replacement_renders_like_send_message() {
        let md = aqua_system_bridge::format::tag_markdown("| a | b |\n|---|---|\n| 1 | 2 |", "r@h");
        let sent = serde_json::to_value(RoomMessageEventContent::text_markdown(&md)).unwrap();
        let edit = serde_json::to_value(replacement(&md, OwnedEventId::try_from("$o:x").unwrap()))
            .unwrap();
        assert_eq!(edit["m.new_content"]["body"], sent["body"]);
        assert_eq!(
            edit["m.new_content"]["formatted_body"],
            sent["formatted_body"]
        );
    }

    #[test]
    fn oversized_edits_are_refused() {
        let id = OwnedEventId::try_from("$o:x").unwrap();
        assert!(check_size(&replacement("short report", id.clone())).is_ok());
        let max = aqua_system_bridge::MAX_MESSAGE_BYTES;
        let table = format!("| c | c |\n|---|---|\n{}", "| a | b |\n".repeat(max / 10));
        let e = check_size(&replacement(&table[..max], id)).unwrap_err();
        assert!(e.contains("cap is"), "{e}");
    }

    #[test]
    fn own_text_message_is_editable() {
        let own = user_id!("@bridge:x");
        assert_eq!(check_editable(own, &message(OWN, text("report"))), Ok(()));
        let notice = message(OWN, json!({"msgtype": "m.notice", "body": "n"}));
        assert!(check_editable(own, &notice)
            .unwrap_err()
            .contains("m.notice"));
    }

    #[test]
    fn other_senders_are_refused() {
        let own = user_id!("@bridge:x");
        let e = check_editable(own, &message("@tim:x", text("hi"))).unwrap_err();
        assert!(e.starts_with("REFUSED") && e.contains("@tim:x"), "{e}");
    }

    #[test]
    fn edits_are_refused_as_originals() {
        let own = user_id!("@bridge:x");
        let edit = message(
            OWN,
            json!({
                "msgtype": "m.text",
                "body": "* new",
                "m.new_content": {"msgtype": "m.text", "body": "new"},
                "m.relates_to": {"rel_type": "m.replace", "event_id": "$first:x"},
            }),
        );
        let e = check_editable(own, &edit).unwrap_err();
        assert!(e.contains("itself an edit of $first:x"), "{e}");
        // a reply is still an original message
        let reply = message(
            OWN,
            json!({"msgtype": "m.text", "body": "r", "m.relates_to": {"m.in_reply_to": {"event_id": "$q:x"}}}),
        );
        assert_eq!(check_editable(own, &reply), Ok(()));
    }

    #[test]
    fn non_text_and_non_message_events_are_refused() {
        let own = user_id!("@bridge:x");
        let file = message(
            OWN,
            json!({"msgtype": "m.file", "body": "a.pdf", "url": "mxc://x/y"}),
        );
        assert!(check_editable(own, &file).unwrap_err().contains("m.file"));
        let reaction = event(json!({
            "type": "m.reaction",
            "event_id": "$re:x",
            "sender": OWN,
            "origin_server_ts": 1,
            "content": {"m.relates_to": {"rel_type": "m.annotation", "event_id": "$orig:x", "key": "+1"}},
        }));
        assert!(check_editable(own, &reaction)
            .unwrap_err()
            .contains("not an m.room.message"));
        let state = event(json!({
            "type": "m.room.topic",
            "state_key": "",
            "event_id": "$st:x",
            "sender": OWN,
            "origin_server_ts": 1,
            "content": {"topic": "t"},
        }));
        assert!(check_editable(own, &state)
            .unwrap_err()
            .contains("not an m.room.message"));
        let encrypted = event(json!({
            "type": "m.room.encrypted",
            "event_id": "$enc:x",
            "sender": OWN,
            "origin_server_ts": 1,
            "content": {
                "algorithm": "m.megolm.v1.aes-sha2",
                "ciphertext": "AAAA",
                "device_id": "DEV",
                "sender_key": "KEY",
                "session_id": "SESS",
            },
        }));
        assert!(check_editable(own, &encrypted)
            .unwrap_err()
            .contains("cannot be decrypted"));
    }

    #[test]
    fn redacted_messages_are_refused() {
        let own = user_id!("@bridge:x");
        let redacted = event(json!({
            "type": "m.room.message",
            "event_id": "$orig:x",
            "sender": OWN,
            "origin_server_ts": 1,
            "content": {},
            "unsigned": {"redacted_because": {
                "type": "m.room.redaction",
                "event_id": "$red:x",
                "sender": OWN,
                "origin_server_ts": 2,
                "redacts": "$orig:x",
                "content": {"redacts": "$orig:x"},
            }},
        }));
        assert!(check_editable(own, &redacted)
            .unwrap_err()
            .contains("redacted"));
    }
}
