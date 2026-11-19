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

/// A reply that must land in a Matrix thread: the room the thread lives in
/// (an answer goes where the root is, which for a DM is the room the message
/// arrived in) and the reply target inside it. Carried by an agent from the
/// inbound message to every send of its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadReply {
    pub room_id: String,
    pub target: ReplyTarget,
}

impl ThreadReply {
    /// The thread reply for a message `event_id` in `room_id` whose thread root
    /// is `thread_root`: `Some` only for a message that is in a thread, `None`
    /// for one in the main timeline (which is answered with no reply relation).
    pub fn of(room_id: &str, event_id: &str, thread_root: Option<&str>) -> Option<Self> {
        thread_root.map(|root| Self {
            room_id: room_id.to_string(),
            target: ReplyTarget {
                event_id: event_id.to_string(),
                thread_root: Some(root.to_string()),
            },
        })
    }
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

/// The relation of chunk `index` of a (possibly split) reply, `previous` being
/// the event id of the chunk before it. The first chunk carries
/// [`first_relation`] of `reply` (none without a reply); a later chunk carries
/// [`continuation_relation`], so it stays in the thread when there is one and
/// carries no relation outside a thread. With `reply == None` every chunk is
/// relation-free: a plain send stays byte-identical to one without reply support.
pub fn chunk_relation(
    index: usize,
    reply: Option<&ReplyTarget>,
    previous: &str,
) -> Result<Option<Relation<RoomMessageEventContentWithoutRelation>>> {
    match (index, reply) {
        (0, Some(r)) => Ok(Some(first_relation(r)?)),
        (0, None) => Ok(None),
        _ => continuation_relation(reply.and_then(|r| r.thread_root.as_deref()), previous),
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
        let mut last = String::new();
        for (i, chunk) in crate::split_for_matrix(message, crate::STREAM_ROLLOVER_BYTES)
            .into_iter()
            .enumerate()
        {
            let relation = chunk_relation(i, reply, &last)?;
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

    /// [`send_to_room_chunked_reply`](Self::send_to_room_chunked_reply) that
    /// survives a dead access token and can carry a real user mention, like
    /// [`send_to_room_chunked_with_refresh`](Self::send_to_room_chunked_with_refresh)
    /// (which is this call with `reply == None`): each chunk is sent with the
    /// retry-until-acknowledged loop; on `M_UNKNOWN_TOKEN` the client reauths
    /// once and that CHUNK is retried, so chunks already delivered are never
    /// sent twice. With `Some(mention)` the LAST chunk carries it (see
    /// [`mention_content`](crate::mention_content)); the relation (a reply, or a
    /// plain thread message for a later chunk) is set on top of it. Returns the
    /// last delivered event id.
    pub async fn send_to_room_chunked_reply_with_refresh(
        &mut self,
        room_id: &str,
        message: &str,
        mention: Option<&crate::RoomMention<'_>>,
        reply: Option<&ReplyTarget>,
    ) -> Result<String> {
        let chunks = crate::split_for_matrix(message, crate::STREAM_ROLLOVER_BYTES);
        let n = chunks.len();
        let mut last = String::new();
        for (i, chunk) in chunks.iter().enumerate() {
            let mut content = match mention {
                Some(m) if i + 1 == n => crate::mention_content(chunk, m)?,
                _ => RoomMessageEventContent::text_markdown(chunk),
            };
            content.relates_to = chunk_relation(i, reply, &last)?;
            let first = crate::retry_finalize("send-room", || {
                self.send_content_once(room_id, content.clone())
            })
            .await;
            last = match first {
                Ok(id) => id,
                Err(e) if crate::is_unknown_token(&e) => {
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        "room send rejected with M_UNKNOWN_TOKEN; refreshing token and retrying the chunk once"
                    );
                    self.reauth_token_only()
                        .await
                        .context("token refresh after M_UNKNOWN_TOKEN failed")?;
                    crate::retry_finalize("send-room", || {
                        self.send_content_once(room_id, content.clone())
                    })
                    .await
                    .context("room send still failed after token refresh")?
                }
                Err(e) => return Err(e),
            };
        }
        Ok(last)
    }

    /// [`send_media_to_room_reply`](Self::send_media_to_room_reply) wrapped with
    /// a refresh-then-retry on `M_UNKNOWN_TOKEN`, like
    /// [`send_file_with_refresh`](Self::send_file_with_refresh): the attachment
    /// send has no retry of its own, so on that error reauth once and resend
    /// the same file once more.
    pub async fn send_media_to_room_reply_with_refresh(
        &mut self,
        room_id: &str,
        path: impl AsRef<std::path::Path>,
        caption: Option<&str>,
        reply: Option<&ReplyTarget>,
    ) -> Result<String> {
        let path = path.as_ref();
        match self
            .send_media_to_room_reply(room_id, path, caption, reply)
            .await
        {
            Ok(id) => Ok(id),
            Err(e) if crate::is_unknown_token(&e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "room file send rejected with M_UNKNOWN_TOKEN; refreshing token and retrying once"
                );
                self.reauth_token_only()
                    .await
                    .context("token refresh after M_UNKNOWN_TOKEN failed")?;
                self.send_media_to_room_reply(room_id, path, caption, reply)
                    .await
                    .context("room file send still failed after token refresh")
            }
            Err(e) => Err(e),
        }
    }

    /// Keep `thread` only when its room is the DM this agent talks to `target`
    /// in, otherwise `None`. An agent that answers a person in their DM must
    /// not start answering into some other room just because the person wrote
    /// there in a thread (a group room, a second DM): such a message is
    /// answered in the DM, as it was before threads were honoured. `None` in,
    /// `None` out, without a lookup.
    pub async fn thread_in_dm_with(
        &self,
        target: &str,
        thread: Option<ThreadReply>,
    ) -> Option<ThreadReply> {
        let t = thread?;
        let dm = self.dm_room_id(target).await.ok().flatten();
        if dm.as_deref() == Some(t.room_id.as_str()) {
            Some(t)
        } else {
            tracing::info!(
                "thread message in {} is not in the DM with {target}; answering in the DM",
                t.room_id
            );
            None
        }
    }

    /// The answer of an agent that talks to `target` in a DM, as text: in the
    /// DM exactly as [`send_dm_chunked_with_refresh`](Self::send_dm_chunked_with_refresh)
    /// when `thread` is `None`, and as a genuine reply inside the thread (every
    /// later chunk too) when it is `Some`. The thread's own room is used, not
    /// the DM resolved for `target`: a thread is answered where its root is.
    pub async fn send_dm_chunked_in_thread_with_refresh(
        &mut self,
        target: &str,
        message: &str,
        thread: Option<&ThreadReply>,
    ) -> Result<String> {
        match thread {
            None => self.send_dm_chunked_with_refresh(target, message).await,
            Some(t) => {
                self.send_to_room_chunked_reply_with_refresh(
                    &t.room_id,
                    message,
                    None,
                    Some(&t.target),
                )
                .await
            }
        }
    }

    /// [`send_file_with_refresh`](Self::send_file_with_refresh) for a thread-aware
    /// agent: the file goes to the DM with `target` when `thread` is `None`
    /// (exactly as before), into the thread as a reply when it is `Some`.
    pub async fn send_file_in_thread_with_refresh(
        &mut self,
        target: &str,
        path: impl AsRef<std::path::Path>,
        caption: Option<&str>,
        thread: Option<&ThreadReply>,
    ) -> Result<String> {
        match thread {
            None => self.send_file_with_refresh(target, path, caption).await,
            Some(t) => {
                self.send_media_to_room_reply_with_refresh(
                    &t.room_id,
                    path,
                    caption,
                    Some(&t.target),
                )
                .await
            }
        }
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

    /// Chunk relations of a split reply: the first chunk is the reply, later
    /// ones stay in the thread; with no reply (or no thread) nothing is added.
    #[test]
    fn chunk_relations_follow_the_thread() {
        let wire_rel = |r: Option<Relation<RoomMessageEventContentWithoutRelation>>| {
            wire(&markdown_with("x", r))["m.relates_to"].clone()
        };
        // no reply: every chunk is relation-free (byte-identical to a plain send)
        for i in 0..3 {
            assert!(chunk_relation(i, None, "$prev:x").unwrap().is_none());
        }
        // a plain reply: only the first chunk quotes the original
        let plain = ReplyTarget {
            event_id: "$orig:x".into(),
            thread_root: None,
        };
        assert_eq!(
            wire_rel(chunk_relation(0, Some(&plain), "").unwrap()),
            json!({"m.in_reply_to": {"event_id": "$orig:x"}})
        );
        assert!(chunk_relation(1, Some(&plain), "$c0:x").unwrap().is_none());
        // a threaded reply: first chunk a genuine thread reply, then plain
        // thread messages chained on the previous chunk
        let threaded = ReplyTarget {
            event_id: "$orig:x".into(),
            thread_root: Some("$root:x".into()),
        };
        let first = wire_rel(chunk_relation(0, Some(&threaded), "").unwrap());
        assert_eq!(first["rel_type"], "m.thread");
        assert_eq!(first["event_id"], "$root:x");
        assert_eq!(first["m.in_reply_to"]["event_id"], "$orig:x");
        let later = wire_rel(chunk_relation(2, Some(&threaded), "$c1:x").unwrap());
        assert_eq!(later["rel_type"], "m.thread");
        assert_eq!(later["event_id"], "$root:x");
        assert_eq!(later["m.in_reply_to"]["event_id"], "$c1:x");
        assert_eq!(later["is_falling_back"], json!(true));
    }

    /// A mention rides the last chunk and keeps the relation set on top of it.
    #[test]
    fn a_mention_and_a_thread_relation_coexist() {
        let m = crate::RoomMention {
            user_id: "@asker:x",
            display: "asker",
        };
        let mut c = crate::mention_content("hello", &m).unwrap();
        c.relates_to = chunk_relation(
            0,
            Some(&ReplyTarget {
                event_id: "$orig:x".into(),
                thread_root: Some("$root:x".into()),
            }),
            "",
        )
        .unwrap();
        let v = wire(&c);
        assert_eq!(v["m.relates_to"]["rel_type"], "m.thread");
        assert_eq!(v["m.mentions"]["user_ids"][0], "@asker:x");
    }

    /// A thread reply exists only for a message that is in a thread, and it
    /// answers in that message's own room.
    #[test]
    fn thread_reply_is_some_only_inside_a_thread() {
        assert_eq!(ThreadReply::of("!dm:x", "$q:x", None), None);
        assert_eq!(
            ThreadReply::of("!dm:x", "$q:x", Some("$root:x")),
            Some(ThreadReply {
                room_id: "!dm:x".into(),
                target: ReplyTarget {
                    event_id: "$q:x".into(),
                    thread_root: Some("$root:x".into()),
                },
            })
        );
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
