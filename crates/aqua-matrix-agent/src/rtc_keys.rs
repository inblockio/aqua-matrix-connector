//! Element Call **media-encryption-key** transport for [`AgentClient`].
//!
//! ## Why this exists
//!
//! Element Call (and Element X) E2E-encrypts call **media** per participant with
//! AES-GCM frame encryption (separate from Megolm room encryption). Each
//! participant generates its own per-call AES key(s) and distributes them to
//! every *other* member device by sending an
//! `io.element.call.encryption_keys` event as an **Olm-encrypted to-device**
//! message. A peer that receives the key can decrypt that participant's media
//! frames; a participant that never publishes its key produces frames nobody
//! else can decode.
//!
//! For the agent's WebRTC media (in the sibling `../aqua-agents` call backend)
//! to interoperate with Element X, the agent must:
//!   1. **SEND** its own media keys, so Element X can decrypt the agent's frames
//!      ([`AgentClient::send_call_encryption_keys`]).
//!   2. **RECEIVE + decrypt** each peer's keys, so the agent can decrypt that
//!      peer's frames ([`AgentClient::on_call_encryption_keys`]).
//!
//! This module is the **Matrix transport only** — it carries the AES key bytes
//! over Olm to-device events. It contains no WebRTC and no frame cipher; the
//! call backend feeds the received keys into its LiveKit `KeyProvider` and hands
//! its own generated keys here to publish.
//!
//! ## Wire format (matrix-js-sdk `ToDeviceKeyTransport`, ground-truthed live)
//!
//! Event type: `io.element.call.encryption_keys`. Decrypted to-device content:
//! ```json
//! { "keys": [ { "index": <u8>, "key": "<base64 of 16 random bytes>" } ],
//!   "member": { "id": "<sender user_id>", "claimed_device_id": "<sender device_id>" },
//!   "session": { "call_id": "", "application": "m.call", "scope": "m.room" },
//!   "room_id": "<room id>", "sent_ts": <ms> }
//! ```

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
// `CollectStrategy` is the one type matrix-sdk takes but does not re-export; it
// lives in matrix-sdk-base's crypto module (a direct dep added for this).
use matrix_sdk_base::crypto::CollectStrategy;
use matrix_sdk::{
    encryption::identities::Device,
    ruma::{events::macros::EventContent, events::AnyToDeviceEventContent, serde::Raw, RoomId},
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::AgentClient;

/// The unstable to-device event type Element Call uses to distribute per-call
/// AES media keys. Used as the wire `type` on both the send and receive paths.
pub const CALL_ENCRYPTION_KEYS_TYPE: &str = "io.element.call.encryption_keys";

// ---------------------------------------------------------------------------
// Wire content (serde, exact Element Call shape)
// ---------------------------------------------------------------------------

/// One AES media key for a participant: a key `index` (Element Call uses
/// `(i+1) % 256` as it ratchets) and the 16 random key bytes, base64-encoded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallKey {
    /// Key ring index this key occupies (frame trailer carries the same index).
    pub index: u8,
    /// Base64 of the 16 random AES key bytes.
    pub key: String,
}

/// The sending participant's identity, as Element Call reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallMember {
    /// The sender's Matrix user id.
    pub id: String,
    /// The sender's Matrix device id (the LiveKit identity is `<id>:<device>`).
    pub claimed_device_id: String,
}

/// The MatrixRTC session descriptor. For a room-scoped `m.call` it is always
/// `call_id=""`, `application="m.call"`, `scope="m.room"`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallSession {
    pub call_id: String,
    pub application: String,
    pub scope: String,
}

impl Default for CallSession {
    fn default() -> Self {
        Self {
            call_id: String::new(),
            application: "m.call".to_owned(),
            scope: "m.room".to_owned(),
        }
    }
}

/// Decrypted content of an `io.element.call.encryption_keys` to-device event.
///
/// The `#[ruma_event(type = ..., kind = ToDevice)]` derive generates the
/// `StaticEventContent` + `ToDeviceEventContent` impls matrix-sdk needs to
/// dispatch a typed `ToDeviceEvent<CallEncryptionKeysEventContent>` to an
/// `add_event_handler` closure (the receive path), and to render the wire JSON
/// for `encrypt_and_send_raw_to_device` (the send path). The derive also emits a
/// `CallEncryptionKeysEvent` type alias (`= ToDeviceEvent<…EventContent>`) — the
/// `…EventContent` ident is chosen so that stripped-suffix alias does NOT collide
/// with the public decoded [`CallEncryptionKeys`] struct below.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, EventContent)]
#[ruma_event(type = "io.element.call.encryption_keys", kind = ToDevice)]
pub struct CallEncryptionKeysEventContent {
    /// The participant's media keys (usually one per ratchet generation).
    pub keys: Vec<CallKey>,
    /// Who sent the keys (user + device).
    pub member: CallMember,
    /// The MatrixRTC session these keys belong to.
    #[serde(default)]
    pub session: CallSession,
    /// The Matrix room the call is in.
    pub room_id: String,
    /// Wall-clock send time in ms — used by Element Call to drop stale keys.
    pub sent_ts: u64,
}

// ---------------------------------------------------------------------------
// Receive-side surfaced type (decoded, ready for the LiveKit KeyProvider)
// ---------------------------------------------------------------------------

/// A decoded inbound media-key set, surfaced to the call backend. The base64 is
/// already decoded into raw key bytes so the consumer can hand each
/// `(index, bytes)` straight to a LiveKit `KeyProvider::set_key`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallEncryptionKeys {
    /// The to-device event sender's Matrix user id (decrypted-event `sender`).
    pub sender_user_id: String,
    /// The sender's device id, from the content `member.claimed_device_id`.
    pub sender_device_id: String,
    /// The room the call is in (content `room_id`).
    pub room_id: String,
    /// `(index, key_bytes)` pairs — base64 already decoded.
    pub keys: Vec<(u8, Vec<u8>)>,
}

// ---------------------------------------------------------------------------
// Minimal, dependency-free base64 (standard alphabet, with padding)
// ---------------------------------------------------------------------------
//
// The connector deliberately pulls in no `base64` crate for one tiny codec.
// Element Call emits standard-alphabet base64 *with* padding for the 16 key
// bytes; we encode the same and decode tolerantly (accept missing padding).

const B64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64_ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64_ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(B64_ALPHABET[(n & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn b64_val(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some((c - b'A') as u32),
        b'a'..=b'z' => Some((c - b'a' + 26) as u32),
        b'0'..=b'9' => Some((c - b'0' + 52) as u32),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn base64_decode(input: &str) -> Result<Vec<u8>> {
    // Collect significant symbols (ignore '=' padding and any whitespace).
    let syms: Vec<u32> = input
        .bytes()
        .filter(|&c| c != b'=' && !c.is_ascii_whitespace())
        .map(|c| b64_val(c).ok_or_else(|| anyhow!("invalid base64 byte: {c:#x}")))
        .collect::<Result<_>>()?;

    let mut out = Vec::with_capacity(syms.len() * 3 / 4);
    for chunk in syms.chunks(4) {
        match chunk.len() {
            // A lone symbol can't form a byte — malformed.
            1 => return Err(anyhow!("invalid base64 length")),
            2 => {
                let n = (chunk[0] << 18) | (chunk[1] << 12);
                out.push((n >> 16) as u8);
            }
            3 => {
                let n = (chunk[0] << 18) | (chunk[1] << 12) | (chunk[2] << 6);
                out.push((n >> 16) as u8);
                out.push((n >> 8) as u8);
            }
            _ => {
                let n = (chunk[0] << 18) | (chunk[1] << 12) | (chunk[2] << 6) | chunk[3];
                out.push((n >> 16) as u8);
                out.push((n >> 8) as u8);
                out.push(n as u8);
            }
        }
    }
    Ok(out)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// AgentClient API: send + receive
// ---------------------------------------------------------------------------

impl AgentClient {
    /// Olm-encrypt and send this agent's media key for `room_id` to **every
    /// device of every other member** of the room — the Element Call key-share.
    ///
    /// `key_index` is the key-ring slot (Element Call ratchets `(i+1) % 256`);
    /// `key` is the raw 16 AES key bytes (base64-encoded here for the wire).
    ///
    /// The content's `member.id` / `member.claimed_device_id` are this agent's
    /// own user id + device id (so peers attribute the key to the agent's
    /// `<user>:<device>` LiveKit identity); `session` is the room-scoped
    /// `m.call` default; `sent_ts` is now.
    ///
    /// Recipients are collected as all devices of all joined room members
    /// *except this agent's own user id*. Sent with
    /// [`CollectStrategy::AllDevices`] so it reaches **unverified** Element X
    /// devices too (we never cross-sign-verify a human's device, so a
    /// verification-gated strategy would silently withhold the key and break
    /// media). Returns `Ok(())` even if `encrypt_and_send_raw_to_device`
    /// reports per-device failures (logged) — a single unreachable device must
    /// not abort the whole share; it errors only if the content can't be built
    /// or no recipient devices exist.
    pub async fn send_call_encryption_keys(
        &self,
        room_id: &str,
        key_index: u8,
        key: &[u8],
    ) -> Result<()> {
        let own_user = self.user_id().to_owned();
        let device_id = self
            .device_id()
            .ok_or_else(|| anyhow!("agent has no device_id; cannot send call encryption keys"))?;

        let content = CallEncryptionKeysEventContent {
            keys: vec![CallKey {
                index: key_index,
                key: base64_encode(key),
            }],
            member: CallMember {
                id: own_user.clone(),
                claimed_device_id: device_id,
            },
            session: CallSession::default(),
            room_id: room_id.to_owned(),
            sent_ts: now_ms(),
        };

        // Collect every device of every OTHER joined room member. `Device`s come
        // from the crypto store (populated by sync); we own them in `devices`
        // and pass borrows to the send call.
        let devices = self.other_member_devices(room_id, &own_user).await?;
        if devices.is_empty() {
            return Err(anyhow!(
                "no recipient devices for call encryption keys in {room_id} \
                 (no other members, or their devices not yet synced)"
            ));
        }
        let device_refs: Vec<&Device> = devices.iter().collect();

        // Raw<AnyToDeviceEventContent>: serialize our typed content to JSON then
        // re-tag it as the generic to-device-content the API takes (same pattern
        // as `update_registry`'s account-data raw cast). matrix-sdk Olm-encrypts
        // this per recipient device.
        let raw: Raw<AnyToDeviceEventContent> = Raw::new(&content)
            .context("failed to serialize call encryption keys content")?
            .cast_unchecked();

        let failures = self
            .client()
            .encryption()
            .encrypt_and_send_raw_to_device(
                device_refs,
                CALL_ENCRYPTION_KEYS_TYPE,
                raw,
                CollectStrategy::AllDevices,
            )
            .await
            .context("encrypt_and_send_raw_to_device failed")?;

        if failures.is_empty() {
            tracing::info!(
                room_id,
                key_index,
                recipients = devices.len(),
                "sent call encryption keys to all recipient devices"
            );
        } else {
            tracing::warn!(
                room_id,
                key_index,
                recipients = devices.len(),
                failed = failures.len(),
                "sent call encryption keys; some recipient devices failed (withheld/unreachable): {failures:?}"
            );
        }
        Ok(())
    }

    /// Gather every [`Device`] of every joined member of `room_id` *except*
    /// `own_user`'s devices. Best-effort per user: a user whose devices haven't
    /// synced yet contributes none (logged), rather than failing the whole set.
    async fn other_member_devices(
        &self,
        room_id: &str,
        own_user: &str,
    ) -> Result<Vec<Device>> {
        use matrix_sdk::RoomMemberships;

        let room_id: &RoomId = room_id
            .try_into()
            .map_err(|e| anyhow!("invalid room_id: {e}"))?;
        let room = self
            .client()
            .get_room(room_id)
            .ok_or_else(|| anyhow!("room {room_id} not found / not joined"))?;

        let members = room
            .members(RoomMemberships::JOIN)
            .await
            .context("failed to list joined room members")?;

        let mut devices = Vec::new();
        for member in members {
            let uid = member.user_id();
            if uid.as_str() == own_user {
                continue;
            }
            match self.client().encryption().get_user_devices(uid).await {
                Ok(user_devices) => {
                    for d in user_devices.devices() {
                        devices.push(d);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        user_id = %uid,
                        "failed to list devices for room member; skipping: {e:#}"
                    );
                }
            }
        }
        Ok(devices)
    }

    /// Register a to-device handler for incoming `io.element.call.encryption_keys`
    /// and return the receiving end of a channel that yields each decrypted,
    /// base64-decoded [`CallEncryptionKeys`].
    ///
    /// matrix-sdk decrypts Olm to-device events during `client.sync(...)` and
    /// dispatches them to handlers registered via `add_event_handler`. The
    /// `#[ruma_event(kind = ToDevice)]` derive lets us register a **typed**
    /// `ToDeviceEvent<CallEncryptionKeysEventContent>` handler, which only fires for
    /// our event type and hands us already-deserialized content. (No
    /// `EncryptionInfo` arg is requested here; the call backend trusts the
    /// `member` fields for identity, matching matrix-js-sdk's transport.)
    ///
    /// The handler is driven by whatever `sync` loop is running on this client
    /// (e.g. the relay's `run_daemon` sync task) — call this **after** connect
    /// and while a sync is (or will be) active. The returned `Receiver` lives in
    /// the caller's event loop; the channel is unbounded-ish (`capacity`), and
    /// keys are dropped (logged) if the consumer falls behind.
    ///
    /// Returns a `Receiver`; keeping it alive keeps the handler useful (dropping
    /// it just makes sends no-op — the handler stays registered but discards).
    pub fn on_call_encryption_keys(&self, capacity: usize) -> mpsc::Receiver<CallEncryptionKeys> {
        use matrix_sdk::ruma::events::ToDeviceEvent;

        let (tx, rx) = mpsc::channel(capacity.max(1));
        self.client().add_event_handler(
            move |ev: ToDeviceEvent<CallEncryptionKeysEventContent>| {
                let tx = tx.clone();
                async move {
                    let sender_user_id = ev.sender.to_string();
                    let content = ev.content;

                    // Decode each base64 key; skip (and log) any that don't decode
                    // rather than dropping the whole event.
                    let mut keys = Vec::with_capacity(content.keys.len());
                    for k in &content.keys {
                        match base64_decode(&k.key) {
                            Ok(bytes) => keys.push((k.index, bytes)),
                            Err(e) => tracing::warn!(
                                sender = %sender_user_id,
                                index = k.index,
                                "dropping undecodable call encryption key: {e:#}"
                            ),
                        }
                    }

                    let decoded = CallEncryptionKeys {
                        sender_user_id,
                        sender_device_id: content.member.claimed_device_id,
                        room_id: content.room_id,
                        keys,
                    };

                    // `try_send`: never block the sync dispatch loop. A full
                    // channel means the consumer is behind — drop + log.
                    if let Err(e) = tx.try_send(decoded) {
                        tracing::warn!("call encryption keys receiver lagging; dropped event: {e}");
                    }
                }
            },
        );
        rx
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn base64_roundtrip_and_known_vectors() {
        // Known vectors against the standard alphabet (with padding).
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_decode("Zm9vYg==").unwrap(), b"foob");

        // 16-byte key (the Element Call media-key length) round-trips.
        let key: Vec<u8> = (0u8..16).collect();
        let encoded = base64_encode(&key);
        assert_eq!(base64_decode(&encoded).unwrap(), key);

        // Tolerant decode: missing padding still works.
        assert_eq!(base64_decode("Zm9vYg").unwrap(), b"foob");
    }

    /// Round-trip the content struct through the EXACT JSON shape Element Call
    /// emits (field names, the empty `call_id`, `application:"m.call"`,
    /// `scope:"m.room"`, base64 key), proving serde matches the wire format both
    /// directions.
    #[test]
    fn call_encryption_keys_content_json_roundtrip() {
        // 16 random-looking bytes -> the base64 a real payload carries.
        let key_bytes: Vec<u8> = vec![
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let key_b64 = base64_encode(&key_bytes);

        let wire = json!({
            "keys": [ { "index": 1, "key": key_b64 } ],
            "member": {
                "id": "@agent:matrix.inblock.io",
                "claimed_device_id": "ABCDEFGHIJ"
            },
            "session": {
                "call_id": "",
                "application": "m.call",
                "scope": "m.room"
            },
            "room_id": "!room:matrix.inblock.io",
            "sent_ts": 1_733_000_000_000u64
        });

        // Deserialize the wire JSON into the typed content.
        let content: CallEncryptionKeysEventContent =
            serde_json::from_value(wire.clone()).expect("wire JSON should deserialize");

        assert_eq!(content.keys.len(), 1);
        assert_eq!(content.keys[0].index, 1);
        assert_eq!(content.keys[0].key, key_b64);
        assert_eq!(base64_decode(&content.keys[0].key).unwrap(), key_bytes);
        assert_eq!(content.member.id, "@agent:matrix.inblock.io");
        assert_eq!(content.member.claimed_device_id, "ABCDEFGHIJ");
        assert_eq!(content.session.call_id, "");
        assert_eq!(content.session.application, "m.call");
        assert_eq!(content.session.scope, "m.room");
        assert_eq!(content.room_id, "!room:matrix.inblock.io");
        assert_eq!(content.sent_ts, 1_733_000_000_000u64);

        // Re-serialize and confirm it matches the original wire JSON exactly.
        let reserialized = serde_json::to_value(&content).expect("content should serialize");
        assert_eq!(reserialized, wire);
    }

    /// The `session` field defaults to the room-scoped `m.call` descriptor when
    /// omitted (matrix-js-sdk always sends it, but be lenient on receive).
    #[test]
    fn session_defaults_when_absent() {
        let wire = json!({
            "keys": [],
            "member": { "id": "@a:b", "claimed_device_id": "DEV" },
            "room_id": "!r:b",
            "sent_ts": 1u64
        });
        let content: CallEncryptionKeysEventContent = serde_json::from_value(wire).unwrap();
        assert_eq!(content.session, CallSession::default());
        assert_eq!(content.session.application, "m.call");
        assert_eq!(content.session.scope, "m.room");
    }

    /// The derive must wire the unstable event type through `StaticEventContent`
    /// so the typed to-device handler dispatches on the right `type`.
    #[test]
    fn event_type_matches_element_call() {
        use matrix_sdk::ruma::events::StaticEventContent;
        assert_eq!(
            <CallEncryptionKeysEventContent as StaticEventContent>::TYPE,
            CALL_ENCRYPTION_KEYS_TYPE
        );
        assert_eq!(CALL_ENCRYPTION_KEYS_TYPE, "io.element.call.encryption_keys");
    }
}
