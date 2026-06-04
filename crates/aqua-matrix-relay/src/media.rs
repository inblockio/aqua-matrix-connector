//! Decode an inbound Matrix media event into the transport-agnostic
//! [`InboundMedia`](crate::InboundMedia), so a handler can inspect an attachment
//! and (via [`AgentClient::download_media`](aqua_matrix_agent::AgentClient::download_media))
//! pull its decrypted bytes without ever naming a Matrix type. The matrix-sdk
//! `MediaSource` is tucked inside the opaque [`MediaHandle`].

use std::time::Duration;

use aqua_matrix_agent::{MediaHandle, MediaKind};
use matrix_sdk::ruma::events::room::message::{
    AudioMessageEventContent, FileMessageEventContent, ImageMessageEventContent,
    VideoMessageEventContent,
};
use matrix_sdk::ruma::events::room::MediaSource;
use matrix_sdk::ruma::UInt;

use crate::InboundMedia;

fn u64_opt(u: Option<UInt>) -> Option<u64> {
    u.map(u64::from)
}

fn duration_ms(d: Option<Duration>) -> Option<u64> {
    d.map(|d| d.as_millis() as u64)
}

fn handle(
    source: &MediaSource,
    filename: &str,
    mimetype: Option<String>,
    size: Option<u64>,
) -> MediaHandle {
    MediaHandle::new(source.clone(), filename.to_string(), mimetype, size)
}

pub(crate) fn from_image(c: &ImageMessageEventContent) -> InboundMedia {
    let info = c.info.as_deref();
    let mimetype = info.and_then(|i| i.mimetype.clone());
    let size = u64_opt(info.and_then(|i| i.size));
    let filename = c.filename().to_string();
    InboundMedia {
        kind: MediaKind::Image,
        handle: handle(&c.source, &filename, mimetype.clone(), size),
        filename,
        mimetype,
        size,
        duration_ms: None,
        width: u64_opt(info.and_then(|i| i.width)),
        height: u64_opt(info.and_then(|i| i.height)),
        is_voice: false,
        waveform: None,
    }
}

pub(crate) fn from_audio(c: &AudioMessageEventContent) -> InboundMedia {
    let info = c.info.as_deref();
    let mimetype = info.and_then(|i| i.mimetype.clone());
    let size = u64_opt(info.and_then(|i| i.size));
    let filename = c.filename().to_string();
    // An `m.audio` with the MSC3245 `voice` marker is a voice note.
    let is_voice = c.voice.is_some();
    // Prefer the MSC3245 audio-details duration; fall back to the AudioInfo one.
    let duration_ms = duration_ms(c.audio.as_ref().map(|a| a.duration))
        .or_else(|| duration_ms(info.and_then(|i| i.duration)));
    // Inbound waveform amplitudes are 0..=1024 (`UnstableAmplitude`).
    let waveform = c
        .audio
        .as_ref()
        .map(|a| a.waveform.iter().map(|amp| u64::from(amp.get()) as u16).collect());
    InboundMedia {
        kind: if is_voice {
            MediaKind::Voice
        } else {
            MediaKind::Audio
        },
        handle: handle(&c.source, &filename, mimetype.clone(), size),
        filename,
        mimetype,
        size,
        duration_ms,
        width: None,
        height: None,
        is_voice,
        waveform,
    }
}

pub(crate) fn from_video(c: &VideoMessageEventContent) -> InboundMedia {
    let info = c.info.as_deref();
    let mimetype = info.and_then(|i| i.mimetype.clone());
    let size = u64_opt(info.and_then(|i| i.size));
    let filename = c.filename().to_string();
    InboundMedia {
        kind: MediaKind::Video,
        handle: handle(&c.source, &filename, mimetype.clone(), size),
        filename,
        mimetype,
        size,
        duration_ms: duration_ms(info.and_then(|i| i.duration)),
        width: u64_opt(info.and_then(|i| i.width)),
        height: u64_opt(info.and_then(|i| i.height)),
        is_voice: false,
        waveform: None,
    }
}

pub(crate) fn from_file(c: &FileMessageEventContent) -> InboundMedia {
    let info = c.info.as_deref();
    let mimetype = info.and_then(|i| i.mimetype.clone());
    let size = u64_opt(info.and_then(|i| i.size));
    let filename = c.filename().to_string();
    InboundMedia {
        kind: MediaKind::File,
        handle: handle(&c.source, &filename, mimetype.clone(), size),
        filename,
        mimetype,
        size,
        duration_ms: None,
        width: None,
        height: None,
        is_voice: false,
        waveform: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::events::room::message::{
        AudioInfo, UnstableAudioDetailsContentBlock, UnstableVoiceContentBlock,
    };
    use matrix_sdk::ruma::events::room::ImageInfo;
    use matrix_sdk::ruma::UInt;

    // A throwaway unencrypted media source; the decoders only `.clone()` it into
    // the handle, so the URI value itself is irrelevant to what we assert.
    fn src() -> MediaSource {
        MediaSource::Plain("mxc://example.org/abc123".into())
    }

    fn uint(n: u64) -> UInt {
        UInt::new(n).expect("fits in UInt")
    }

    #[test]
    fn image_carries_dimensions() {
        let mut content = ImageMessageEventContent::new("pic.png".to_owned(), src());
        let mut info = ImageInfo::new();
        info.width = Some(uint(640));
        info.height = Some(uint(480));
        info.size = Some(uint(1234));
        content.info = Some(Box::new(info));

        let media = from_image(&content);
        assert_eq!(media.kind, MediaKind::Image);
        assert_eq!(media.width, Some(640));
        assert_eq!(media.height, Some(480));
        assert_eq!(media.size, Some(1234));
        assert!(!media.is_voice);
    }

    #[test]
    fn audio_with_voice_marker_is_voice() {
        let mut content = AudioMessageEventContent::new("note.ogg".to_owned(), src());
        content.audio = Some(UnstableAudioDetailsContentBlock::new(
            Duration::from_millis(3_200),
            Vec::new(),
        ));
        content.voice = Some(UnstableVoiceContentBlock::new());

        let media = from_audio(&content);
        assert_eq!(media.kind, MediaKind::Voice);
        assert!(media.is_voice);
        assert_eq!(media.duration_ms, Some(3_200));
    }

    #[test]
    fn plain_audio_is_audio_not_voice() {
        let mut content = AudioMessageEventContent::new("clip.mp3".to_owned(), src());
        let mut info = AudioInfo::new();
        info.duration = Some(Duration::from_millis(5_000));
        content.info = Some(Box::new(info));

        let media = from_audio(&content);
        assert_eq!(media.kind, MediaKind::Audio);
        assert!(!media.is_voice);
        // Falls back to the AudioInfo duration when no MSC3245 audio block.
        assert_eq!(media.duration_ms, Some(5_000));
    }
}
