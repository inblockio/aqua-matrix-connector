//! Reply threading for outgoing messages, and reply/thread metadata of inbound
//! ones.
//!
//! Conventions (as in github.com/IA-PieroCV/cc_matrix_channel and
//! github.com/jlxq0/matrix-mcp, re-implemented here):
//!
//! - A reply carries `m.relates_to.m.in_reply_to.event_id` on its FIRST event
//!   only; a long message split into several events does not quote the
//!   original on every chunk.
//! - When the original was posted in a thread, the reply is a genuine reply
//!   inside that thread (`rel_type: m.thread`, the thread root as `event_id`,
//!   `m.in_reply_to` the original, `is_falling_back: false`). Later chunks
//!   stay in the thread as plain thread messages (`is_falling_back: true`,
//!   `m.in_reply_to` the previous chunk, which is what the spec's fallback
//!   expects).
//! - Inbound: `in_reply_to` is only reported for a genuine reply. A thread
//!   message whose `m.in_reply_to` is the thread FALLBACK (the latest event in
//!   the thread, `is_falling_back: true`) did not choose that event, so it is
//!   not reported as a reply to it; its `thread_root` still is.

use anyhow::{anyhow, Context, Result};
use matrix_sdk::ruma::events::relation::{Reply as RelReply, Thread};
use matrix_sdk::ruma::events::room::message::{
    Relation, RoomMessageEventContent, RoomMessageEventContentWithoutRelation,
};
use matrix_sdk::ruma::events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent};
use matrix_sdk::ruma::{EventId, OwnedEventId};

/// What a reply points at: the original event, and the thread it lives in
/// (if any).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReplyTarget {
    pub event_id: String,
    pub thread_root: Option<String>,
}

fn owned_event_id(s: &str) -> Result<OwnedEventId> {
    OwnedEventId::try_from(s).map_err(|e| anyhow!("invalid event id {s:?}: {e}"))
}

/// The relation of the FIRST event of a reply.
pub fn first_relation(
    target: &ReplyTarget,
) -> Result<Relation<RoomMessageEventContentWithoutRelation>> {
    let original = owned_event_id(&target.event_id)?;
    Ok(match &target.thread_root {
        Some(root) => Relation::Thread(Thread::reply(owned_event_id(root)?, original)),
        None => Relation::Reply(RelReply::with_event_id(original)),
    })
}

/// The relation of a later chunk: in a thread, a plain thread message after
/// `previous`; outside a thread, none.
pub fn continuation_relation(
    thread_root: Option<&str>,
    previous: &str,
) -> Result<Option<Relation<RoomMessageEventContentWithoutRelation>>> {
    match thread_root {
        Some(root) => Ok(Some(Relation::Thread(Thread::plain(
            owned_event_id(root)?,
            owned_event_id(previous)?,
        )))),
        None => Ok(None),
    }
}

/// `(in_reply_to, thread_root)` of an inbound message's relation. See the
/// module docs for the thread-fallback rule.
pub fn reply_fields<C>(rel: Option<&Relation<C>>) -> (Option<String>, Option<String>) {
    match rel {
        Some(Relation::Reply(r)) => (Some(r.in_reply_to.event_id.to_string()), None),
        Some(Relation::Thread(t)) => (
            t.in_reply_to
                .as_ref()
                .filter(|_| !t.is_falling_back)
                .map(|r| r.event_id.to_string()),
            Some(t.event_id.to_string()),
        ),
        _ => (None, None),
    }
}

/// A Markdown message content with an optional relation.
pub fn markdown_with(
    text: &str,
    relation: Option<Relation<RoomMessageEventContentWithoutRelation>>,
) -> RoomMessageEventContent {
    let mut content = RoomMessageEventContent::text_markdown(text);
    content.relates_to = relation;
    content
}

impl crate::AgentClient {
    /// Resolve a reply target by event id from a JOINED room: loads (and
    /// decrypts) the event, which also proves it is in that room, and reads
    /// its thread. Fails for an unknown event or one in another room.
    pub async fn reply_target_in_room(&self, room_id: &str, event_id: &str) -> Result<ReplyTarget> {
        let room = self.joined_room(room_id)?;
        let eid = <&EventId>::try_from(event_id)
            .map_err(|e| anyhow!("invalid event id {event_id:?}: {e}"))?;
        let ev = room.event(eid, None).await.with_context(|| {
            format!("reply_to event {event_id} was not found in room {room_id}")
        })?;
        let thread_root = match ev.raw().deserialize() {
            Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(m))) => m
                .as_original()
                .and_then(|o| reply_fields(o.content.relates_to.as_ref()).1),
            Ok(AnySyncTimelineEvent::MessageLike(_)) => None,
            Ok(AnySyncTimelineEvent::State(_)) => {
                anyhow::bail!("reply_to {event_id} is a state event, not a message")
            }
            Err(e) => anyhow::bail!("reply_to {event_id} could not be read ({e})"),
        };
        Ok(ReplyTarget {
            event_id: event_id.to_string(),
            thread_root,
        })
    }

    /// Like [`send_to_room_chunked`](Self::send_to_room_chunked), but the
    /// first chunk is a reply to `reply` (threaded when the original is in a
    /// thread) and later chunks stay in that thread. The room must be joined.
    /// Returns the last delivered event id.
    pub async fn send_to_room_chunked_reply(
        &self,
        room_id: &str,
        message: &str,
        reply: Option<&ReplyTarget>,
    ) -> Result<String> {
        let room = self.joined_room(room_id)?;
        let thread_root = reply.and_then(|r| r.thread_root.clone());
        let mut last = String::new();
        for (i, chunk) in crate::split_for_matrix(message, crate::STREAM_ROLLOVER_BYTES)
            .into_iter()
            .enumerate()
        {
            let relation = match (i, reply) {
                (0, Some(r)) => Some(first_relation(r)?),
                (0, None) => None,
                _ => continuation_relation(thread_root.as_deref(), &last)?,
            };
            last = crate::retry_finalize("send-room-reply", || {
                let content = markdown_with(&chunk, relation.clone());
                let room = room.clone();
                async move {
                    let resp = room
                        .send(content)
                        .await
                        .context("failed to send room message")?;
                    Ok(resp.response.event_id.to_string())
                }
            })
            .await?;
        }
        Ok(last)
    }

    /// [`send_media_to_room`](Self::send_media_to_room) as a reply (matrix-sdk
    /// builds the relation from the loaded original: a genuine thread reply
    /// when `reply.thread_root` is set, a plain reply otherwise).
    pub async fn send_media_to_room_reply(
        &self,
        room_id: &str,
        path: impl AsRef<std::path::Path>,
        caption: Option<&str>,
        reply: Option<&ReplyTarget>,
    ) -> Result<String> {
        use matrix_sdk::room::reply::{EnforceThread, Reply};
        use matrix_sdk::ruma::events::room::message::{AddMentions, ReplyWithinThread};
        let reply = reply
            .map(|r| -> Result<Reply> {
                Ok(Reply {
                    event_id: owned_event_id(&r.event_id)?,
                    enforce_thread: if r.thread_root.is_some() {
                        EnforceThread::Threaded(ReplyWithinThread::Yes)
                    } else {
                        EnforceThread::Unthreaded
                    },
                    add_mentions: AddMentions::No,
                })
            })
            .transpose()?;
        self.send_media_to_room_with(room_id, path, caption, reply)
            .await
    }

    /// The room id of the true 1:1 DM with `target`, created (and marked in
    /// `m.direct`) when none exists. Same resolution as every DM send.
    pub async fn dm_room_for(&self, target: &str) -> Result<String> {
        let uid = <&matrix_sdk::ruma::UserId>::try_from(target)
            .map_err(|e| anyhow!("invalid target: {e}"))?;
        Ok(self.ensure_dm_room(uid).await?.room_id().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn wire(c: &RoomMessageEventContent) -> Value {
        serde_json::to_value(c).unwrap()
    }

    #[test]
    fn plain_reply_has_in_reply_to_only() {
        let t = ReplyTarget {
            event_id: "$orig:x".into(),
            thread_root: None,
        };
        let c = markdown_with("hi", Some(first_relation(&t).unwrap()));
        let v = wire(&c);
        assert_eq!(
            v["m.relates_to"],
            json!({"m.in_reply_to": {"event_id": "$orig:x"}})
        );
        assert_eq!(v["body"], "hi");
        // no thread continuation outside a thread
        assert!(continuation_relation(None, "$prev:x").unwrap().is_none());
    }

    #[test]
    fn threaded_reply_stays_in_the_thread() {
        let t = ReplyTarget {
            event_id: "$orig:x".into(),
            thread_root: Some("$root:x".into()),
        };
        let v = wire(&markdown_with("hi", Some(first_relation(&t).unwrap())));
        let r = &v["m.relates_to"];
        assert_eq!(r["rel_type"], "m.thread");
        assert_eq!(r["event_id"], "$root:x");
        assert_eq!(r["m.in_reply_to"]["event_id"], "$orig:x");
        // a genuine reply: is_falling_back false (ruma omits the default)
        assert!(r["is_falling_back"].is_null() || r["is_falling_back"] == json!(false));
        // later chunk: plain thread message, falling back to the previous chunk
        let next = continuation_relation(Some("$root:x"), "$chunk1:x")
            .unwrap()
            .unwrap();
        let v = wire(&markdown_with("more", Some(next)));
        let r = &v["m.relates_to"];
        assert_eq!(r["rel_type"], "m.thread");
        assert_eq!(r["event_id"], "$root:x");
        assert_eq!(r["m.in_reply_to"]["event_id"], "$chunk1:x");
        assert_eq!(r["is_falling_back"], json!(true));
    }

    #[test]
    fn bad_ids_are_errors() {
        let t = ReplyTarget {
            event_id: "not-an-event".into(),
            thread_root: None,
        };
        assert!(first_relation(&t).is_err());
    }

    fn parse(content: Value) -> (Option<String>, Option<String>) {
        let mut c = json!({"msgtype": "m.text", "body": "x"});
        c.as_object_mut()
            .unwrap()
            .extend(content.as_object().unwrap().clone());
        let c: RoomMessageEventContent = serde_json::from_value(c).unwrap();
        reply_fields(c.relates_to.as_ref())
    }

    #[test]
    fn inbound_reply_fields() {
        assert_eq!(parse(json!({})), (None, None));
        assert_eq!(
            parse(json!({"m.relates_to": {"m.in_reply_to": {"event_id": "$a:x"}}})),
            (Some("$a:x".into()), None)
        );
        // genuine reply inside a thread
        assert_eq!(
            parse(
                json!({"m.relates_to": {"rel_type": "m.thread", "event_id": "$root:x",
                "m.in_reply_to": {"event_id": "$a:x"}, "is_falling_back": false}})
            ),
            (Some("$a:x".into()), Some("$root:x".into()))
        );
        // thread fallback: NOT a reply to the latest event, still in the thread
        assert_eq!(
            parse(
                json!({"m.relates_to": {"rel_type": "m.thread", "event_id": "$root:x",
                "m.in_reply_to": {"event_id": "$latest:x"}, "is_falling_back": true}})
            ),
            (None, Some("$root:x".into()))
        );
        // our own outgoing shapes round-trip through the parser
        let t = ReplyTarget {
            event_id: "$o:x".into(),
            thread_root: Some("$r:x".into()),
        };
        let c = markdown_with("x", Some(first_relation(&t).unwrap()));
        let back: RoomMessageEventContent = serde_json::from_value(wire(&c)).unwrap();
        assert_eq!(
            reply_fields(back.relates_to.as_ref()),
            (Some("$o:x".into()), Some("$r:x".into()))
        );
    }
}
