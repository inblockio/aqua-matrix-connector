//! The hub's side of D10: decide on knocks at the roster room. Runs in the hub
//! seat (the room's creator). A knock is accepted iff the endorsement passes
//! `endorse::check` (signature by a host in hosts.toml, fresh, sane seat name)
//! AND the knocking MXID is the account of `seat_did`. Everything decided is
//! written to `hub-audit.jsonl` (public data only).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use aqua_matrix_agent::AgentClient;
use matrix_sdk::{room::Room, RoomMemberships};
use serde_json::{json, Value};

use crate::daemon::Daemon;
use crate::endorse::{self, Endorsement, Reject};
use crate::seat::{unix_ms, unix_now, SeatShared};

/// What to do with one knock.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Invite(Endorsement),
    Decline(Reject),
    /// Could not decide yet (siwx-oidc unreachable): leave the knock pending.
    Retry(String),
}

/// What siwx-oidc's `/resolve?did=` says about a DID.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub mxid: Option<String>,
    pub exists: bool,
}

/// Parse a `/resolve` answer.
pub fn parse_resolve(body: &str) -> Result<Resolved> {
    let v: Value = serde_json::from_str(body).context("resolve answer is not JSON")?;
    Ok(Resolved {
        mxid: v.get("mxid").and_then(Value::as_str).map(str::to_string),
        exists: v.get("exists").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// Does `resolved` say the account of the endorsed DID is `knocker`?
pub fn binding_holds(resolved: &Resolved, knocker: &str) -> bool {
    resolved.exists
        && resolved
            .mxid
            .as_deref()
            .is_some_and(|m| m.eq_ignore_ascii_case(knocker))
}

/// The DID -> MXID direction of `/resolve` is computed from the DID by the
/// provider (never from the user-writable profile field), so it is the check.
async fn resolve_did(siwx_url: &str, did: &str) -> Result<Resolved> {
    let url = format!("{}/resolve", siwx_url.trim_end_matches('/'));
    let resp = reqwest::Client::new()
        .get(url)
        .query(&[("did", did)])
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("siwx-oidc /resolve unreachable")?;
    let status = resp.status();
    let text = resp.text().await.context("reading /resolve")?;
    if !status.is_success() {
        return Err(anyhow!("siwx-oidc /resolve answered {status}"));
    }
    parse_resolve(&text)
}

/// Decide on a knock by `mxid` with `reason`.
pub async fn decide(d: &Daemon, mxid: &str, reason: Option<&str>) -> Verdict {
    let Some(reason) = reason else {
        return Verdict::Decline(Reject::Malformed);
    };
    let checked = {
        let mut hosts = d.hosts.lock().unwrap();
        endorse::check(reason, unix_now(), &mut hosts)
    };
    let e = match checked {
        Ok(e) => e,
        Err(r) => return Verdict::Decline(r),
    };
    match resolve_did(&d.cfg.siwx_url, &e.seat_did).await {
        Ok(r) if binding_holds(&r, mxid) => Verdict::Invite(e),
        Ok(_) => Verdict::Decline(Reject::MxidMismatch),
        Err(err) => Verdict::Retry(format!("{err:#}")),
    }
}

/// Append one audit line (0600).
pub fn audit(d: &Daemon, line: Value) {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    let path = d.cfg.root.join("hub-audit.jsonl");
    let mut v = line;
    v["ts_ms"] = json!(unix_ms());
    let r = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
        .and_then(|mut f| writeln!(f, "{v}"));
    if let Err(e) = r {
        tracing::error!("hub-audit.jsonl write failed: {e}");
    }
}

/// The `reason` of a knocking member's `m.room.member` event.
fn knock_reason(m: &matrix_sdk::room::RoomMember) -> Option<String> {
    use matrix_sdk::deserialized_responses::SyncOrStrippedState;
    match &**m.event() {
        SyncOrStrippedState::Sync(ev) => ev.as_original().and_then(|o| o.content.reason.clone()),
        SyncOrStrippedState::Stripped(ev) => ev.content.reason.clone(),
    }
}

/// Look at every pending knock at the roster room and invite or decline.
pub async fn process_knocks(
    d: &Arc<Daemon>,
    shared: &SeatShared,
    agent: &AgentClient,
    room: &Room,
) -> Result<()> {
    let knockers = room
        .members(RoomMemberships::KNOCK)
        .await
        .context("listing knocks")?;
    for m in knockers {
        let mxid = m.user_id().to_string();
        let reason = knock_reason(&m);
        match decide(d, &mxid, reason.as_deref()).await {
            Verdict::Invite(e) => match room.invite_user_by_id(m.user_id()).await {
                Ok(()) => {
                    tracing::info!(seat = %e.seat, host = %e.host, "knock accepted: invited");
                    audit(
                        d,
                        json!({"verdict": "invited", "mxid": mxid, "seat": e.seat, "host": e.host, "host_did": e.host_did}),
                    );
                }
                Err(err) => {
                    tracing::warn!("inviting {mxid} failed: {err:#}");
                    audit(
                        d,
                        json!({"verdict": "retry", "mxid": mxid, "code": "invite_failed"}),
                    );
                }
            },
            Verdict::Decline(r) => {
                tracing::warn!(%mxid, code = r.code(), "knock declined");
                audit(
                    d,
                    json!({"verdict": "declined", "mxid": mxid, "code": r.code()}),
                );
                if let Err(err) = room
                    .kick_user(
                        m.user_id(),
                        Some(&format!("claude-seats: declined ({})", r.code())),
                    )
                    .await
                {
                    tracing::warn!("declining {mxid} failed: {err:#}");
                }
            }
            Verdict::Retry(why) => {
                tracing::warn!(%mxid, "cannot decide on a knock yet: {why}");
                audit(
                    d,
                    json!({"verdict": "retry", "mxid": mxid, "code": "resolve_unavailable"}),
                );
            }
        }
    }
    let _ = (shared, agent);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_answers_parse() {
        let r = parse_resolve(r#"{"did":"did:key:z6MkA","mxid":"@x:dev.matrix.inblock.io","exists":true,"attested":true}"#).unwrap();
        assert_eq!(
            r,
            Resolved {
                mxid: Some("@x:dev.matrix.inblock.io".into()),
                exists: true
            }
        );
        let r =
            parse_resolve(r#"{"did":null,"mxid":null,"exists":false,"attested":false}"#).unwrap();
        assert_eq!(
            r,
            Resolved {
                mxid: None,
                exists: false
            }
        );
        assert!(parse_resolve("<html>").is_err());
    }

    #[test]
    fn the_binding_needs_an_existing_account_with_the_same_mxid() {
        let ok = Resolved {
            mxid: Some("@X:srv".into()),
            exists: true,
        };
        assert!(binding_holds(&ok, "@x:srv"));
        assert!(!binding_holds(&ok, "@y:srv"));
        assert!(!binding_holds(
            &Resolved {
                exists: false,
                ..ok.clone()
            },
            "@x:srv"
        ));
        assert!(!binding_holds(
            &Resolved {
                mxid: None,
                exists: true
            },
            "@x:srv"
        ));
    }
}
