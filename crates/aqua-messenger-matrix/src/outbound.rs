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
    match r.room_id.as_deref() {
        Some(rid) if rid != room_id => Err(anyhow::anyhow!(
            "reply_to {} is a message in room {rid}, but this send resolves to room {room_id}; \
             a reply must go to the room the original is in (send without reply_to)",
            r.event_id
        )),
        Some(_) => Ok(Some(ReplyTarget {
            event_id: r.event_id.clone(),
            thread_root: r.thread_root.clone(),
        })),
        None => agent
            .reply_target_in_room(room_id, &r.event_id)
            .await
            .map(Some),
    }
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
