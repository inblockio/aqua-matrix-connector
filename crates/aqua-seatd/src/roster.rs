//! The roster: the members of the `claude-seats` room and the claims they made.
//!
//! A seat claims its entry with a state event `io.inblock.claude_seat` whose
//! state key is its own MXID. The homeserver only lets that user set a state
//! key that starts with their own `@`, so the claim of one account cannot be
//! forged by another. What the claim says (seat name, DID, host) is the
//! account's own assertion; the binding of DID to MXID is siwx-oidc's.

use std::collections::HashSet;

use aqua_seat::proto::Peer;
use aqua_seat::{seatname, CLAIM_EVENT_TYPE};
use serde_json::{json, Value};

/// Matrix display name of a seat (D4).
pub fn display_name(seat: &str, host: &str) -> String {
    format!("{seat}@{host}")
}

/// Content of the claim event.
pub fn claim_content(seat: &str, did: &str, host: &str, device_id: Option<&str>) -> Value {
    let mut v = json!({"v": 1, "seat": seat, "did": did, "host": host});
    if let Some(d) = device_id {
        v["device_id"] = json!(d);
    }
    v
}

fn host_ok(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 64
        && h.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

fn did_ok(d: &str) -> bool {
    d.starts_with("did:key:")
        && d.len() <= 200
        && d.chars().all(|c| c.is_ascii_alphanumeric() || c == ':')
}

/// Parse one raw state event (`{"type","state_key","sender","content"}`) into
/// a [`Peer`], or `None` when it is not a valid claim by a joined member.
pub fn parse_claim(event: &Value, joined: &HashSet<String>) -> Option<Peer> {
    if event.get("type")?.as_str()? != CLAIM_EVENT_TYPE {
        return None;
    }
    let sender = event.get("sender")?.as_str()?;
    let state_key = event.get("state_key")?.as_str()?;
    if !sender.eq_ignore_ascii_case(state_key) {
        return None;
    }
    if !joined.contains(&sender.to_ascii_lowercase()) {
        return None;
    }
    let c = event.get("content")?;
    let seat = c.get("seat")?.as_str()?;
    let did = c.get("did")?.as_str()?;
    let host = c.get("host")?.as_str()?;
    if !seatname::is_valid(seat) || !did_ok(did) || !host_ok(host) {
        return None;
    }
    Some(Peer {
        seat: seat.to_string(),
        did: did.to_string(),
        host: host.to_string(),
        mxid: sender.to_string(),
    })
}

/// Resolve the `to` of `seat_send` against the roster: `@mxid`, `did:key:...`,
/// `seat` or `seat@host`.
pub fn resolve<'a>(peers: &'a [Peer], query: &str) -> Result<&'a Peer, String> {
    let q = query.trim();
    if q.is_empty() {
        return Err("empty recipient".into());
    }
    let known = || {
        let mut names: Vec<String> = peers
            .iter()
            .map(|p| format!("{}@{}", p.seat, p.host))
            .collect();
        names.sort();
        if names.is_empty() {
            "the roster is empty".to_string()
        } else {
            format!("known seats: {}", names.join(", "))
        }
    };
    if q.starts_with('@') {
        return peers
            .iter()
            .find(|p| p.mxid.eq_ignore_ascii_case(q))
            .ok_or_else(|| format!("{q} is not on the roster ({})", known()));
    }
    if q.starts_with("did:") {
        return peers
            .iter()
            .find(|p| p.did == q)
            .ok_or_else(|| format!("{q} is not on the roster ({})", known()));
    }
    let (name, host) = match q.split_once('@') {
        Some((n, h)) => (n, Some(h)),
        None => (q, None),
    };
    let hits: Vec<&Peer> = peers
        .iter()
        .filter(|p| p.seat == name && host.is_none_or(|h| p.host.eq_ignore_ascii_case(h)))
        .collect();
    match hits.as_slice() {
        [one] => Ok(one),
        [] => Err(format!("no seat {q:?} on the roster ({})", known())),
        many => Err(format!(
            "seat {q:?} is ambiguous: {}; address it as name@host",
            many.iter()
                .map(|p| format!("{}@{}", p.seat, p.host))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(seat: &str, host: &str, n: u8) -> Peer {
        Peer {
            seat: seat.into(),
            did: format!("did:key:z6MkTest{n}"),
            host: host.into(),
            mxid: format!("@u{n}:dev.matrix.inblock.io"),
        }
    }

    fn ev(sender: &str, key: &str, content: Value) -> Value {
        json!({"type": CLAIM_EVENT_TYPE, "sender": sender, "state_key": key, "content": content})
    }

    fn joined(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_ascii_lowercase()).collect()
    }

    #[test]
    fn a_valid_claim_by_a_joined_member_parses() {
        let c = claim_content("alpha", "did:key:z6MkAbc", "hostA", Some("AQUA_x"));
        let p = parse_claim(&ev("@u:x", "@u:x", c), &joined(&["@u:x"])).unwrap();
        assert_eq!(
            (
                p.seat.as_str(),
                p.did.as_str(),
                p.host.as_str(),
                p.mxid.as_str()
            ),
            ("alpha", "did:key:z6MkAbc", "hostA", "@u:x")
        );
    }

    #[test]
    fn a_claim_for_someone_elses_state_key_is_rejected() {
        let c = claim_content("alpha", "did:key:z6MkAbc", "hostA", None);
        assert!(parse_claim(&ev("@evil:x", "@u:x", c), &joined(&["@evil:x", "@u:x"])).is_none());
    }

    #[test]
    fn a_claim_by_a_non_member_or_with_bad_fields_is_rejected() {
        let ok = claim_content("alpha", "did:key:z6MkAbc", "hostA", None);
        assert!(parse_claim(&ev("@u:x", "@u:x", ok.clone()), &joined(&[])).is_none());
        for bad in [
            claim_content("Alpha", "did:key:z6MkAbc", "hostA", None),
            claim_content("../x", "did:key:z6MkAbc", "hostA", None),
            claim_content("room", "did:key:z6MkAbc", "hostA", None),
            claim_content("alpha", "did:web:x", "hostA", None),
            claim_content("alpha", "did:key:z6Mk Abc", "hostA", None),
            claim_content("alpha", "did:key:z6MkAbc", "ho st", None),
            claim_content("alpha", "did:key:z6MkAbc", "", None),
        ] {
            assert!(
                parse_claim(&ev("@u:x", "@u:x", bad.clone()), &joined(&["@u:x"])).is_none(),
                "{bad}"
            );
        }
        let wrong_type =
            json!({"type": "m.room.name", "sender": "@u:x", "state_key": "@u:x", "content": ok});
        assert!(parse_claim(&wrong_type, &joined(&["@u:x"])).is_none());
    }

    #[test]
    fn resolve_by_name_host_did_and_mxid() {
        let peers = vec![
            peer("alpha", "hostA", 1),
            peer("beta", "hostA", 2),
            peer("alpha", "hostB", 3),
        ];
        assert_eq!(
            resolve(&peers, "beta").unwrap().mxid,
            "@u2:dev.matrix.inblock.io"
        );
        assert_eq!(
            resolve(&peers, "alpha@hostB").unwrap().mxid,
            "@u3:dev.matrix.inblock.io"
        );
        assert_eq!(
            resolve(&peers, "alpha@HOSTA").unwrap().mxid,
            "@u1:dev.matrix.inblock.io"
        );
        assert_eq!(resolve(&peers, "did:key:z6MkTest2").unwrap().seat, "beta");
        assert_eq!(
            resolve(&peers, "@U2:dev.matrix.inblock.io").unwrap().seat,
            "beta"
        );
    }

    #[test]
    fn resolve_refuses_ambiguous_and_unknown_with_a_useful_error() {
        let peers = vec![peer("alpha", "hostA", 1), peer("alpha", "hostB", 3)];
        let e = resolve(&peers, "alpha").unwrap_err();
        assert!(
            e.contains("ambiguous") && e.contains("alpha@hostA") && e.contains("alpha@hostB"),
            "{e}"
        );
        let e = resolve(&peers, "nobody").unwrap_err();
        assert!(e.contains("known seats"), "{e}");
        assert!(resolve(&[], "x").unwrap_err().contains("empty"));
        assert!(resolve(&peers, "  ").is_err());
    }
}
