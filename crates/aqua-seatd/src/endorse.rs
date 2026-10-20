//! D10: the host endorsement carried in a knock.
//!
//! A seat on another host knocks at the hub's roster room with `reason` =
//! base64url of a JSON envelope `{"e": {seat, seat_did, host, host_did, ts},
//! "sig": <base64 Ed25519 signature>}`. The signature is made by the host key
//! (`host.pem`, D8) over a fixed text, not over JSON, so there is nothing to
//! canonicalise:
//!
//! ```text
//! claude-seats-endorsement-v1\n<seat>\n<seat_did>\n<host>\n<host_did>\n<ts>
//! ```
//!
//! The hub accepts a knock only if every check here passes, and then the
//! caller checks that the knocking MXID is the account of `seat_did`
//! (siwx-oidc `/resolve?did=`; that part needs the network and is in `hub.rs`).

use base64::{
    engine::general_purpose::STANDARD, engine::general_purpose::URL_SAFE_NO_PAD, Engine as _,
};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::hosts::{valid_did, valid_host_name, HostList};

/// An endorsement older or newer than this (seconds) is refused.
pub const MAX_SKEW_SECS: u64 = 600;
const DOMAIN: &str = "claude-seats-endorsement-v1";
/// Matrix keeps a knock `reason` short; refuse anything bigger before parsing.
pub const MAX_REASON_BYTES: usize = 2048;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endorsement {
    pub seat: String,
    pub seat_did: String,
    pub host: String,
    pub host_did: String,
    /// Unix seconds.
    pub ts: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    e: Endorsement,
    sig: String,
}

/// Why a knock is refused. `code()` is what the audit line and the decline
/// reason carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    Malformed,
    UnknownHost,
    HostNameMismatch,
    BadSignature,
    Stale,
    BadSeatName,
    BadSeatDid,
    MxidMismatch,
}

impl Reject {
    pub fn code(self) -> &'static str {
        match self {
            Reject::Malformed => "malformed",
            Reject::UnknownHost => "unknown_host",
            Reject::HostNameMismatch => "host_name_mismatch",
            Reject::BadSignature => "bad_signature",
            Reject::Stale => "stale",
            Reject::BadSeatName => "bad_seat_name",
            Reject::BadSeatDid => "bad_seat_did",
            Reject::MxidMismatch => "mxid_mismatch",
        }
    }
}

/// The bytes the host key signs.
pub fn signing_input(e: &Endorsement) -> Vec<u8> {
    format!(
        "{DOMAIN}\n{}\n{}\n{}\n{}\n{}",
        e.seat, e.seat_did, e.host, e.host_did, e.ts
    )
    .into_bytes()
}

/// Sign `e` with the host key and return the knock `reason`.
pub fn make_reason(key: &SigningKey, e: &Endorsement) -> String {
    let sig = key.sign(&signing_input(e));
    let env = Envelope {
        e: e.clone(),
        sig: STANDARD.encode(sig.to_bytes()),
    };
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(&env).expect("envelope serializes"))
}

/// The Ed25519 public key inside a `did:key:z6Mk...` DID.
pub fn verifying_key_of(did: &str) -> Option<VerifyingKey> {
    let z = did.strip_prefix("did:key:z")?;
    let bytes = bs58::decode(z).into_vec().ok()?;
    let key = bytes.strip_prefix(&[0xED, 0x01])?;
    VerifyingKey::from_bytes(key.try_into().ok()?).ok()
}

/// Everything the hub can check without the network. Returns the endorsement
/// on success; `now` is unix seconds.
pub fn check(reason: &str, now: u64, hosts: &mut HostList) -> Result<Endorsement, Reject> {
    if reason.len() > MAX_REASON_BYTES {
        return Err(Reject::Malformed);
    }
    let raw = URL_SAFE_NO_PAD
        .decode(reason.trim())
        .map_err(|_| Reject::Malformed)?;
    let env: Envelope = serde_json::from_slice(&raw).map_err(|_| Reject::Malformed)?;
    let e = env.e;
    if !valid_did(&e.host_did) || !valid_host_name(&e.host) {
        return Err(Reject::Malformed);
    }
    let Some(listed) = hosts.lookup(&e.host_did) else {
        return Err(Reject::UnknownHost);
    };
    if listed.name != e.host {
        return Err(Reject::HostNameMismatch);
    }
    let sig_bytes = STANDARD
        .decode(env.sig.trim())
        .map_err(|_| Reject::Malformed)?;
    let sig = Signature::from_slice(&sig_bytes).map_err(|_| Reject::Malformed)?;
    let vk = verifying_key_of(&e.host_did).ok_or(Reject::Malformed)?;
    vk.verify(&signing_input(&e), &sig)
        .map_err(|_| Reject::BadSignature)?;
    if now.abs_diff(e.ts) > MAX_SKEW_SECS {
        return Err(Reject::Stale);
    }
    if !aqua_seat::seatname::is_valid(&e.seat) {
        return Err(Reject::BadSeatName);
    }
    if !valid_did(&e.seat_did) {
        return Err(Reject::BadSeatDid);
    }
    Ok(e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    struct Host {
        key: SigningKey,
        did: String,
    }

    fn host() -> Host {
        let key = SigningKey::generate(&mut OsRng);
        let mut bytes = vec![0xED, 0x01];
        bytes.extend_from_slice(key.verifying_key().as_bytes());
        let did = format!("did:key:z{}", bs58::encode(bytes).into_string());
        Host { key, did }
    }

    fn list(hosts: &[(&Host, &str)]) -> (tempfile::TempDir, HostList) {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("hosts.toml");
        let mut s = String::new();
        for (h, name) in hosts {
            s.push_str(&format!(
                "[[hosts]]\ndid = \"{}\"\nname = \"{name}\"\n",
                h.did
            ));
        }
        std::fs::write(&p, s).unwrap();
        (t, HostList::new(p))
    }

    fn endorsement(h: &Host, ts: u64) -> Endorsement {
        Endorsement {
            seat: "lane-a".into(),
            seat_did: "did:key:z6MkSeatSeatSeatSeatSeatSeatSeatSeatSeatSeat1".into(),
            host: "clawi".into(),
            host_did: h.did.clone(),
            ts,
        }
    }

    const NOW: u64 = 1_760_000_000;

    #[test]
    fn a_fresh_endorsement_by_a_listed_host_passes() {
        let h = host();
        let (_t, mut l) = list(&[(&h, "clawi")]);
        let e = endorsement(&h, NOW - 5);
        let got = check(&make_reason(&h.key, &e), NOW, &mut l).unwrap();
        assert_eq!(got, e);
        // the verifying key really comes out of the DID
        assert!(verifying_key_of(&h.did).is_some());
    }

    #[test]
    fn unknown_host_is_refused_even_with_a_valid_signature() {
        let h = host();
        let other = host();
        let (_t, mut l) = list(&[(&other, "clawi")]);
        let r = check(&make_reason(&h.key, &endorsement(&h, NOW)), NOW, &mut l);
        assert_eq!(r, Err(Reject::UnknownHost));
        let (_t, mut empty) = list(&[]);
        assert_eq!(
            check(&make_reason(&h.key, &endorsement(&h, NOW)), NOW, &mut empty),
            Err(Reject::UnknownHost)
        );
    }

    #[test]
    fn a_signature_by_another_key_or_over_other_fields_is_bad() {
        let h = host();
        let imposter = host();
        let (_t, mut l) = list(&[(&h, "clawi")]);
        // listed host_did, signed by someone else
        let r = check(
            &make_reason(&imposter.key, &endorsement(&h, NOW)),
            NOW,
            &mut l,
        );
        assert_eq!(r, Err(Reject::BadSignature));
        // fields changed after signing
        let e = endorsement(&h, NOW);
        let reason = make_reason(&h.key, &e);
        let raw = URL_SAFE_NO_PAD.decode(&reason).unwrap();
        let mut env: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        env["e"]["seat"] = "someone-else".into();
        let tampered = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&env).unwrap());
        assert_eq!(check(&tampered, NOW, &mut l), Err(Reject::BadSignature));
    }

    #[test]
    fn stale_or_future_timestamps_are_refused() {
        let h = host();
        let (_t, mut l) = list(&[(&h, "clawi")]);
        for ts in [NOW - MAX_SKEW_SECS - 1, NOW + MAX_SKEW_SECS + 1, 0] {
            assert_eq!(
                check(&make_reason(&h.key, &endorsement(&h, ts)), NOW, &mut l),
                Err(Reject::Stale),
                "{ts}"
            );
        }
        for ts in [NOW - MAX_SKEW_SECS, NOW + MAX_SKEW_SECS] {
            assert!(
                check(&make_reason(&h.key, &endorsement(&h, ts)), NOW, &mut l).is_ok(),
                "{ts}"
            );
        }
    }

    #[test]
    fn host_name_must_match_the_allow_list() {
        let h = host();
        let (_t, mut l) = list(&[(&h, "clawi")]);
        let mut e = endorsement(&h, NOW);
        e.host = "laptop".into();
        assert_eq!(
            check(&make_reason(&h.key, &e), NOW, &mut l),
            Err(Reject::HostNameMismatch)
        );
    }

    #[test]
    fn seat_name_and_seat_did_must_be_well_formed() {
        let h = host();
        let (_t, mut l) = list(&[(&h, "clawi")]);
        for bad in ["Bad Seat!", "../x", "", "room", "UPPER"] {
            let mut e = endorsement(&h, NOW);
            e.seat = bad.into();
            assert_eq!(
                check(&make_reason(&h.key, &e), NOW, &mut l),
                Err(Reject::BadSeatName),
                "{bad:?}"
            );
        }
        let mut e = endorsement(&h, NOW);
        e.seat_did = "did:web:example.com".into();
        assert_eq!(
            check(&make_reason(&h.key, &e), NOW, &mut l),
            Err(Reject::BadSeatDid)
        );
    }

    #[test]
    fn garbage_is_malformed_not_a_panic() {
        let h = host();
        let (_t, mut l) = list(&[(&h, "clawi")]);
        for bad in [
            "",
            "###",
            "aGVsbG8",
            &"A".repeat(5000),
            &URL_SAFE_NO_PAD.encode(b"{\"e\":1}"),
        ] {
            assert_eq!(check(bad, NOW, &mut l), Err(Reject::Malformed), "{bad:.20}");
        }
        // unknown fields are refused too
        let raw = URL_SAFE_NO_PAD.encode(br#"{"e":{"seat":"a","seat_did":"d","host":"h","host_did":"x","ts":1,"extra":1},"sig":"AA"}"#);
        assert_eq!(check(&raw, NOW, &mut l), Err(Reject::Malformed));
    }

    #[test]
    fn reject_codes_are_stable_strings() {
        let codes: Vec<&str> = [
            Reject::Malformed,
            Reject::UnknownHost,
            Reject::HostNameMismatch,
            Reject::BadSignature,
            Reject::Stale,
            Reject::BadSeatName,
            Reject::BadSeatDid,
            Reject::MxidMismatch,
        ]
        .iter()
        .map(|r| r.code())
        .collect();
        assert_eq!(
            codes,
            [
                "malformed",
                "unknown_host",
                "host_name_mismatch",
                "bad_signature",
                "stale",
                "bad_seat_name",
                "bad_seat_did",
                "mxid_mismatch"
            ]
        );
    }
}
