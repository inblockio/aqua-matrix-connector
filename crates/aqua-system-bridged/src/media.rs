//! Inbound attachment download for `fetch_attachment`.
//!
//! The ciphertext is fetched through matrix-sdk's media API with the cache
//! OFF (nothing lands in the crypto/event store). matrix-sdk picks the
//! authenticated endpoint `/_matrix/client/v1/media/download/...` whenever the
//! homeserver advertises it (Matrix 1.11+; the legacy v3 endpoints 404 on
//! matrix.inblock.io). Decryption uses matrix-sdk-crypto's
//! `AttachmentDecryptor` (AES-256-CTR with the event's key/iv), which verifies
//! the SHA-256 of the ciphertext from the event's `hashes` and fails on a
//! mismatch, so a tampered or truncated download is never stored.

use std::io::Read as _;

use anyhow::{anyhow, bail, Context, Result};
use aqua_system_bridge::inbox::MediaRef;
use matrix_sdk::media::{MediaFormat, MediaRequestParameters};
use matrix_sdk::ruma::events::room::message::MessageType;
use matrix_sdk::ruma::events::room::{EncryptedFile, MediaSource};
use matrix_sdk::ruma::events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent};
use matrix_sdk::ruma::{EventId, RoomId};
use matrix_sdk::Client;

/// Record an attachment's media reference (source JSON + declared metadata)
/// for the inbox, from an inbound message. `None` for non-media msgtypes.
pub fn media_ref(msgtype: &MessageType) -> Option<MediaRef> {
    let (source, mimetype, size) = match msgtype {
        MessageType::File(c) => {
            let i = c.info.as_deref();
            (
                &c.source,
                i.and_then(|i| i.mimetype.clone()),
                i.and_then(|i| i.size),
            )
        }
        MessageType::Image(c) => {
            let i = c.info.as_deref();
            (
                &c.source,
                i.and_then(|i| i.mimetype.clone()),
                i.and_then(|i| i.size),
            )
        }
        MessageType::Audio(c) => {
            let i = c.info.as_deref();
            (
                &c.source,
                i.and_then(|i| i.mimetype.clone()),
                i.and_then(|i| i.size),
            )
        }
        MessageType::Video(c) => {
            let i = c.info.as_deref();
            (
                &c.source,
                i.and_then(|i| i.mimetype.clone()),
                i.and_then(|i| i.size),
            )
        }
        _ => return None,
    };
    let source = serde_json::to_value(source).ok()?;
    Some(MediaRef {
        source,
        mimetype,
        size: size.map(u64::from),
    })
}

/// Decrypt an E2EE attachment and verify its SHA-256 (of the ciphertext, as
/// declared in the event). Any mismatch, bad key/iv or missing hash is an error.
pub fn decrypt_verified(ciphertext: &[u8], file: &EncryptedFile) -> Result<Vec<u8>> {
    let mut cursor = std::io::Cursor::new(ciphertext);
    let mut reader =
        matrix_sdk_base::crypto::AttachmentDecryptor::new(&mut cursor, file.clone().into())
            .map_err(|e| anyhow!("attachment encryption info rejected: {e}"))?;
    let mut out = Vec::with_capacity(ciphertext.len());
    reader.read_to_end(&mut out).map_err(|e| {
        anyhow!("attachment failed verification/decryption ({e}); refusing to store it")
    })?;
    Ok(out)
}

/// Fall back for inbox entries recorded before the media reference was kept:
/// load the event from the room (decrypted with this Client's store).
async fn media_ref_from_event(client: &Client, room_id: &str, event_id: &str) -> Result<MediaRef> {
    let room_id = <&RoomId>::try_from(room_id).map_err(|e| anyhow!("bad room id: {e}"))?;
    let event_id = <&EventId>::try_from(event_id).map_err(|e| anyhow!("bad event id: {e}"))?;
    let room = client
        .get_room(room_id)
        .ok_or_else(|| anyhow!("room {room_id} is not known to the bridge"))?;
    let ev = room
        .event(event_id, None)
        .await
        .context("cannot load the attachment's event")?;
    let Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(msg))) =
        ev.raw().deserialize()
    else {
        bail!("event {event_id} is not a (decryptable) room message");
    };
    let orig = msg
        .as_original()
        .ok_or_else(|| anyhow!("event {event_id} was redacted"))?;
    media_ref(&orig.content.msgtype)
        .ok_or_else(|| anyhow!("event {event_id} carries no attachment"))
}

/// Download, size-check, decrypt and verify one attachment on the live
/// Client. Returns the plaintext bytes and the declared mime type.
pub async fn fetch(
    client: &Client,
    room_id: &str,
    event_id: &str,
    media: Option<MediaRef>,
    max_bytes: u64,
) -> Result<(Vec<u8>, Option<String>)> {
    let media = match media {
        Some(m) => m,
        None => media_ref_from_event(client, room_id, event_id).await?,
    };
    if let Some(declared) = media.size {
        if declared > max_bytes {
            bail!("declared size {declared} bytes exceeds the cap of {max_bytes} bytes");
        }
    }
    let source: MediaSource = serde_json::from_value(media.source.clone())
        .context("stored media reference is malformed")?;
    let url = match &source {
        MediaSource::Plain(u) => u.clone(),
        MediaSource::Encrypted(f) => f.url.clone(),
    };
    // Raw bytes via the (authenticated) content repository, cache OFF.
    let req = MediaRequestParameters {
        source: MediaSource::Plain(url),
        format: MediaFormat::File,
    };
    let raw = client
        .media()
        .get_media_content(&req, false)
        .await
        .context("media download failed")?;
    if raw.len() as u64 > max_bytes {
        bail!(
            "downloaded {} bytes, above the cap of {max_bytes} bytes; discarded",
            raw.len()
        );
    }
    let bytes = match &source {
        MediaSource::Plain(_) => raw,
        MediaSource::Encrypted(f) => decrypt_verified(&raw, f)?,
    };
    Ok((bytes, media.mimetype))
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::OwnedMxcUri;
    use matrix_sdk_base::crypto::AttachmentEncryptor;

    fn encrypt(plain: &[u8]) -> (Vec<u8>, EncryptedFile) {
        let mut cursor = std::io::Cursor::new(plain.to_vec());
        let mut enc = AttachmentEncryptor::new(&mut cursor);
        let mut ct = Vec::new();
        enc.read_to_end(&mut ct).unwrap();
        let info = enc.finish();
        let url = OwnedMxcUri::from("mxc://localhost/abc");
        (
            ct,
            EncryptedFile::new(url, info.encryption_info, info.hashes),
        )
    }

    #[test]
    fn decrypts_and_verifies() {
        let plain = b"attachment payload \x00\x01\x02 with binary".repeat(100);
        let (ct, file) = encrypt(&plain);
        assert_ne!(ct, plain);
        assert_eq!(decrypt_verified(&ct, &file).unwrap(), plain);
    }

    #[test]
    fn hash_mismatch_is_rejected() {
        let (mut ct, file) = encrypt(b"important bytes");
        ct[3] ^= 0x01;
        let err = decrypt_verified(&ct, &file).unwrap_err().to_string();
        assert!(err.contains("verification"), "{err}");
        // Truncation is caught by the same hash check.
        let (ct, file) = encrypt(b"important bytes");
        assert!(decrypt_verified(&ct[..ct.len() - 1], &file).is_err());
    }

    #[test]
    fn media_ref_roundtrips_the_source() {
        let (_, file) = encrypt(b"x");
        let src = MediaSource::Encrypted(Box::new(file));
        let v = serde_json::to_value(&src).unwrap();
        assert!(v.get("file").is_some());
        let back: MediaSource = serde_json::from_value(v).unwrap();
        assert!(matches!(back, MediaSource::Encrypted(_)));
    }
}
