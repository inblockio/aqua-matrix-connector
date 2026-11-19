//! Outbound sends into a resolved room, with `reply_to` threading. Shared by
//! the host daemon (which resolves a person's DM under its own rules) and the
//! in-process backend. Every call runs on the caller's ONE live Client.

use aqua_matrix_agent::{AgentClient, ReplyTarget};
use aqua_messenger::{ReplyRef, Sent};

/// The reply target inside `room_id`. An inbox-derived reference must be in
/// that very room; a bare event id is loaded from the room (which proves it
/// is there and tells whether it is in a thread).
pub async fn reply_target(
    agent: &AgentClient,
    room_id: &str,
    reply: Option<&ReplyRef>,
) -> anyhow::Result<Option<ReplyTarget>> {
    let Some(r) = reply else {
        return Ok(None);
    };
    let target = match r.room_id.as_deref() {
        Some(rid) if rid != room_id => {
            return Err(anyhow::anyhow!(
                "reply_to {} is a message in room {rid}, but this send resolves to room {room_id}; \
                 a reply must go to the room the original is in (send without reply_to)",
                r.event_id
            ))
        }
        Some(_) => ReplyTarget {
            event_id: r.event_id.clone(),
            thread_root: r.thread_root.clone(),
        },
        None => agent.reply_target_in_room(room_id, &r.event_id).await?,
    };
    Ok(Some(if r.into_thread {
        into_thread(target)
    } else {
        target
    }))
}

/// The target of a `thread` send: the thread the message is already in (its
/// root), else the message itself is the root of the thread the send starts.
/// Plain `reply_to` leaves a message outside a thread as a quoting reply.
pub fn into_thread(mut target: ReplyTarget) -> ReplyTarget {
    if target.thread_root.is_none() {
        target.thread_root = Some(target.event_id.clone());
    }
    target
}

/// Send Markdown into `room_id` (chunked; replying to `reply` if given).
pub async fn send_text(
    agent: &AgentClient,
    room_id: &str,
    markdown: &str,
    reply: Option<&ReplyRef>,
) -> anyhow::Result<Sent> {
    let target = reply_target(agent, room_id, reply).await?;
    let event_id = if target.is_none() {
        agent.send_to_room_chunked(room_id, markdown).await?
    } else {
        agent
            .send_to_room_chunked_reply(room_id, markdown, target.as_ref())
            .await?
    };
    Ok(Sent {
        event_id,
        thread_root: target.and_then(|t| t.thread_root),
    })
}

/// Upload `path` into `room_id` as an (E2EE) attachment, replying to `reply`
/// if given.
pub async fn send_file(
    agent: &AgentClient,
    room_id: &str,
    path: &std::path::Path,
    caption: &str,
    reply: Option<&ReplyRef>,
) -> anyhow::Result<Sent> {
    let target = reply_target(agent, room_id, reply).await?;
    let event_id = agent
        .send_media_to_room_reply(room_id, path, Some(caption), target.as_ref())
        .await?;
    Ok(Sent {
        event_id,
        thread_root: target.and_then(|t| t.thread_root),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aqua_matrix_agent::reply::first_relation;
    use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;

    fn relation_of(t: &ReplyTarget) -> serde_json::Value {
        let mut c = RoomMessageEventContent::text_markdown("x");
        c.relates_to = Some(first_relation(t).unwrap());
        serde_json::to_value(&c).unwrap()["m.relates_to"].clone()
    }

    /// A `thread` send to a message that is not in a thread yet starts one:
    /// the message is the root.
    #[test]
    fn thread_send_to_a_plain_message_starts_a_thread_on_it() {
        let t = into_thread(ReplyTarget {
            event_id: "$q:x".into(),
            thread_root: None,
        });
        let r = relation_of(&t);
        assert_eq!(r["rel_type"], "m.thread");
        assert_eq!(r["event_id"], "$q:x");
        assert_eq!(r["m.in_reply_to"]["event_id"], "$q:x");
    }

    /// A `thread` send to a message that is itself in a thread uses ITS root,
    /// never a nested thread on the message.
    #[test]
    fn thread_send_to_a_threaded_message_uses_the_root() {
        let t = into_thread(ReplyTarget {
            event_id: "$inner:x".into(),
            thread_root: Some("$root:x".into()),
        });
        assert_eq!(t.thread_root.as_deref(), Some("$root:x"));
        let r = relation_of(&t);
        assert_eq!(r["rel_type"], "m.thread");
        assert_eq!(r["event_id"], "$root:x");
        assert_eq!(r["m.in_reply_to"]["event_id"], "$inner:x");
    }

    /// Without `thread` a reply to a plain message stays a quoting reply: no
    /// thread relation appears.
    #[test]
    fn plain_reply_to_stays_unthreaded() {
        let r = relation_of(&ReplyTarget {
            event_id: "$q:x".into(),
            thread_root: None,
        });
        assert!(r["rel_type"].is_null(), "{r}");
        assert_eq!(r["m.in_reply_to"]["event_id"], "$q:x");
    }
}
