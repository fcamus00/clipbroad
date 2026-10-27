//! Wire protocol: the messages peers exchange and how they are framed on a stream.
//!
//! A frame is a 4-byte big-endian length followed by that many bytes of a `postcard`-encoded
//! [`Message`]. [`encode`] builds a frame and [`decode`] reads one from the front of a buffer, so
//! the network layer does all the IO and this module stays pure.
//!
//! # Session rules
//!
//! - Each side sends [`Hello::CURRENT`] as its first message and expects a `Hello` first.
//!   [`decode`] turns a `Hello` with another version into [`FrameError::IncompatibleVersion`];
//!   the connection should then be closed.
//! - A received [`Clip`] comes from another machine and is not trusted. Get its content with
//!   [`Clip::into_content`], which checks the payload against the declared size and hash.
//!
//! # Compatibility
//!
//! Variant and field order is the wire format. Any change an older peer cannot read needs a
//! [`PROTOCOL_VERSION`] bump. `Hello` must stay readable by every version, so it stays variant 0
//! of [`Message`] with `protocol_version` as its first field, and new fields can only be appended.
//! [`decode`] ignores bytes left over after the message inside a frame, so an older peer still
//! reads the version from a newer, longer `Hello`.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{ClipContent, ContentHash, Png};

/// Wire protocol version. Bump on any incompatible message change.
///
/// History:
/// - 1: text clips.
/// - 2: image clips ([`ContentKind::Image`]). A version 1 peer cannot decode them.
pub const PROTOCOL_VERSION: u16 = 2;

/// Largest frame body accepted or sent, in bytes.
///
/// A protocol constant, not a setting: a receiver rejects anything bigger, so both sides must
/// agree on it. It also caps inline payloads, so the configured inline threshold
/// (`inline_max_bytes`) must stay below it, with room for a few hundred bytes of metadata.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// Size of the length prefix in front of every frame body.
const HEADER_LEN: usize = 4;

const TEXT_MIME: &str = "text/plain;charset=utf-8";
const PNG_MIME: &str = "image/png";

/// Everything that travels between peers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    /// First message on every connection. Must stay variant 0 (see the module docs).
    Hello(Hello),
    /// New clipboard content.
    Clip(Clip),
}

/// Opening message of a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// Protocol version of the sender. Must stay the first field (see the module docs).
    pub protocol_version: u16,
}

impl Hello {
    /// The `Hello` this build sends.
    pub const CURRENT: Self = Self {
        protocol_version: PROTOCOL_VERSION,
    };
}

/// Identity of a node: the 32 bytes of its iroh `EndpointId` (an Ed25519 public key).
///
/// `core` does not interpret the bytes, so it does not depend on iroh.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NodeId([u8; 32]);

impl NodeId {
    /// Wraps the raw bytes of an `EndpointId`.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw 32 bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Lowercase hex, 64 characters (the format peer ids have in the config file).
impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({self})")
    }
}

/// Number the origin gives a clip. The origin picks it (a counter is enough), so it is unique per
/// origin and `(origin, id)` names one clip across all machines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClipId(pub u64);

/// What kind of content a clip carries. Files will be added as a new variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ContentKind {
    /// UTF-8 text.
    Text,
    /// A PNG image.
    Image,
}

/// Clipboard content copied on one machine, sent to the others.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Clip {
    pub id: ClipId,
    /// Node where the content was copied.
    pub origin: NodeId,
    /// When it was copied, in milliseconds since the Unix epoch, by the origin's clock. The origin
    /// moves it past the newest clip it has seen, so it orders clips even with clocks out of sync
    /// (see [`crate::sync`]).
    pub timestamp_ms: u64,
    pub kind: ContentKind,
    /// MIME type of the content: `text/plain;charset=utf-8` for text, `image/png` for images.
    /// Informative only: the receiver decodes the payload by `kind`.
    pub mime: String,
    /// Content size in bytes.
    pub size: u64,
    /// Hash of the content bytes, the same as [`ClipContent::hash`].
    pub hash: ContentHash,
    pub payload: Payload,
}

impl Clip {
    /// Builds a clip that carries `content` inline.
    #[must_use]
    pub fn new(id: ClipId, origin: NodeId, timestamp_ms: u64, content: ClipContent) -> Self {
        let hash = content.hash();
        let (kind, mime, bytes) = match content {
            ClipContent::Text(text) => (ContentKind::Text, TEXT_MIME, text.into_bytes()),
            ClipContent::Image(png) => (ContentKind::Image, PNG_MIME, png.into_bytes()),
        };
        Self {
            id,
            origin,
            timestamp_ms,
            kind,
            mime: mime.to_owned(),
            size: bytes.len() as u64,
            hash,
            payload: Payload::Inline(bytes),
        }
    }

    /// The content, once the payload is checked against the declared size and hash.
    ///
    /// This check is the receiver's duty and this is the place it happens: the fields come from
    /// another machine and are not trusted until it passes.
    ///
    /// # Errors
    ///
    /// [`ClipError`] if the size or hash does not match the payload, if a text payload is not
    /// valid UTF-8, or if an image payload does not start like a PNG.
    pub fn into_content(self) -> Result<ClipContent, ClipError> {
        let Payload::Inline(bytes) = self.payload;
        let actual = bytes.len() as u64;
        if actual != self.size {
            return Err(ClipError::SizeMismatch {
                declared: self.size,
                actual,
            });
        }
        if ContentHash::of(&bytes) != self.hash {
            return Err(ClipError::HashMismatch);
        }
        match self.kind {
            ContentKind::Text => String::from_utf8(bytes)
                .map(ClipContent::Text)
                .map_err(|_| ClipError::NotUtf8),
            ContentKind::Image => Png::try_from(bytes)
                .map(ClipContent::Image)
                .map_err(|_| ClipError::NotPng),
        }
    }
}

/// Where a clip's content bytes are.
///
/// Only inline exists today. Large files will add a variant that announces the content and sends
/// the bytes separately (phase 4).
///
/// `Debug` prints the length, never the bytes, so logging a clip cannot leak what was copied.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Payload {
    /// The content bytes travel inside the message.
    Inline(Vec<u8>),
}

impl fmt::Debug for Payload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inline(bytes) => f.debug_struct("Inline").field("len", &bytes.len()).finish(),
        }
    }
}

/// Why a frame could not be encoded or decoded.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FrameError {
    /// The frame body is over [`MAX_FRAME_LEN`]. On decode, this is detected from the length
    /// prefix alone, before the body arrives.
    #[error("frame of {len} bytes is over the {max}-byte limit", max = MAX_FRAME_LEN)]
    TooLarge { len: usize },

    /// The frame body is not a valid message.
    #[error("malformed message")]
    Malformed(#[from] postcard::Error),

    /// The peer's `Hello` carries a different protocol version.
    #[error("peer speaks protocol version {theirs}, this build speaks {ours}", ours = PROTOCOL_VERSION)]
    IncompatibleVersion { theirs: u16 },
}

/// Why a received clip was rejected by [`Clip::into_content`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ClipError {
    #[error("clip declares {declared} bytes but carries {actual}")]
    SizeMismatch { declared: u64, actual: u64 },

    #[error("clip content does not match its declared hash")]
    HashMismatch,

    #[error("text clip is not valid UTF-8")]
    NotUtf8,

    #[error("image clip is not a PNG")]
    NotPng,
}

/// Encodes `message` as a frame, ready to write to a stream.
///
/// # Errors
///
/// [`FrameError::TooLarge`] if the message is over [`MAX_FRAME_LEN`] (a clip too big to go inline).
pub fn encode(message: &Message) -> Result<Vec<u8>, FrameError> {
    let mut frame = postcard::to_extend(message, vec![0; HEADER_LEN])?;
    let len = frame.len() - HEADER_LEN;
    let header = match u32::try_from(len) {
        Ok(len32) if len <= MAX_FRAME_LEN => len32.to_be_bytes(),
        _ => return Err(FrameError::TooLarge { len }),
    };
    frame[..HEADER_LEN].copy_from_slice(&header);
    Ok(frame)
}

/// Decodes the frame at the front of `buf`.
///
/// Returns `Ok(None)` while `buf` does not hold a whole frame yet: read more bytes and call again.
/// Otherwise returns the message and how many bytes of `buf` it took; the caller drops those from
/// the front of its buffer. The length is checked against [`MAX_FRAME_LEN`] as soon as the 4-byte
/// prefix is in, so a peer cannot make the caller buffer a huge frame.
///
/// ```
/// use clipbroad_core::protocol::{self, Hello, Message};
///
/// let frame = protocol::encode(&Message::Hello(Hello::CURRENT))?;
/// assert!(protocol::decode(&frame[..3])?.is_none());
/// assert_eq!(
///     protocol::decode(&frame)?,
///     Some((Message::Hello(Hello::CURRENT), frame.len()))
/// );
/// # Ok::<(), protocol::FrameError>(())
/// ```
///
/// # Errors
///
/// [`FrameError::TooLarge`] if the prefix declares more than [`MAX_FRAME_LEN`],
/// [`FrameError::Malformed`] if the body is not a valid message, and
/// [`FrameError::IncompatibleVersion`] for a `Hello` with another protocol version.
pub fn decode(buf: &[u8]) -> Result<Option<(Message, usize)>, FrameError> {
    let Some((header, rest)) = buf.split_first_chunk::<HEADER_LEN>() else {
        return Ok(None);
    };
    let len = usize::try_from(u32::from_be_bytes(*header)).unwrap_or(usize::MAX);
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge { len });
    }
    let Some(body) = rest.get(..len) else {
        return Ok(None);
    };
    // Leftover bytes are ignored on purpose: see "Compatibility" in the module docs.
    let (message, _) = postcard::take_from_bytes::<Message>(body)?;
    if let Message::Hello(hello) = &message
        && hello.protocol_version != PROTOCOL_VERSION
    {
        return Err(FrameError::IncompatibleVersion {
            theirs: hello.protocol_version,
        });
    }
    Ok(Some((message, HEADER_LEN + len)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::{TINY_PNG, tiny_image};

    fn text_clip(text: &str) -> Clip {
        Clip::new(
            ClipId(7),
            NodeId::from_bytes([3; 32]),
            1_700_000_000_000,
            ClipContent::from(text),
        )
    }

    fn image_clip() -> Clip {
        Clip::new(
            ClipId(8),
            NodeId::from_bytes([3; 32]),
            1_700_000_000_000,
            tiny_image(),
        )
    }

    /// Builds a frame around an arbitrary body, valid or not.
    fn frame_of(body: &[u8]) -> Vec<u8> {
        let len = u32::try_from(body.len()).expect("test body fits in u32");
        [&len.to_be_bytes()[..], body].concat()
    }

    fn round_trip(message: &Message) -> Message {
        let frame = encode(message).expect("encode");
        let (decoded, used) = decode(&frame).expect("decode").expect("whole frame");
        assert_eq!(used, frame.len());
        decoded
    }

    #[test]
    fn hello_round_trips() {
        let hello = Message::Hello(Hello::CURRENT);
        assert_eq!(round_trip(&hello), hello);
    }

    #[test]
    fn clip_round_trips() {
        let clip = Message::Clip(text_clip("héllo\r\n🦀"));
        assert_eq!(round_trip(&clip), clip);
    }

    #[test]
    fn hello_wire_format_never_changes() {
        // Length 2, variant 0 (Hello), version 2. Older and newer peers depend on these bytes.
        let frame = encode(&Message::Hello(Hello::CURRENT)).expect("encode");
        assert_eq!(frame, [0, 0, 0, 2, 0, 2]);
    }

    #[test]
    fn truncated_frame_asks_for_more_bytes() {
        let frame = encode(&Message::Clip(text_clip("hello"))).expect("encode");
        for cut in 0..frame.len() {
            assert!(
                decode(&frame[..cut]).expect("not an error").is_none(),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn body_shorter_than_its_message_is_malformed() {
        let frame = encode(&Message::Clip(text_clip("hello"))).expect("encode");
        let short = frame_of(&frame[HEADER_LEN..frame.len() - 1]);
        assert!(matches!(decode(&short), Err(FrameError::Malformed(_))));
    }

    #[test]
    fn garbage_body_is_malformed() {
        assert!(matches!(
            decode(&frame_of(&[])),
            Err(FrameError::Malformed(_))
        ));
        assert!(matches!(
            decode(&frame_of(&[200])),
            Err(FrameError::Malformed(_))
        ));
    }

    #[test]
    fn oversized_length_is_rejected_before_the_body_arrives() {
        let too_big = u32::try_from(MAX_FRAME_LEN + 1).expect("fits in u32");
        for len in [too_big, u32::MAX] {
            assert!(matches!(
                decode(&len.to_be_bytes()),
                Err(FrameError::TooLarge { .. })
            ));
        }
    }

    #[test]
    fn oversized_clip_is_not_encoded() {
        let clip = Message::Clip(text_clip(&"x".repeat(MAX_FRAME_LEN)));
        assert!(matches!(encode(&clip), Err(FrameError::TooLarge { .. })));
    }

    #[test]
    fn other_protocol_version_is_rejected() {
        let hello = Message::Hello(Hello {
            protocol_version: PROTOCOL_VERSION + 1,
        });
        let frame = encode(&hello).expect("encode");
        assert!(matches!(
            decode(&frame),
            Err(FrameError::IncompatibleVersion { theirs }) if theirs == PROTOCOL_VERSION + 1
        ));
    }

    #[test]
    fn newer_hello_with_more_fields_still_reports_its_version() {
        #[derive(Serialize)]
        enum FutureMessage {
            Hello {
                protocol_version: u16,
                nickname: String,
            },
        }
        let body = postcard::to_allocvec(&FutureMessage::Hello {
            protocol_version: 9,
            nickname: "laptop".to_owned(),
        })
        .expect("serialize");
        assert!(matches!(
            decode(&frame_of(&body)),
            Err(FrameError::IncompatibleVersion { theirs: 9 })
        ));
    }

    #[test]
    fn frames_back_to_back_decode_in_order() {
        let first = Message::Hello(Hello::CURRENT);
        let second = Message::Clip(text_clip("hi"));
        let mut buf = encode(&first).expect("encode");
        buf.extend(encode(&second).expect("encode"));

        let (decoded, used) = decode(&buf).expect("decode").expect("whole frame");
        assert_eq!(decoded, first);
        let (decoded, rest) = decode(&buf[used..]).expect("decode").expect("whole frame");
        assert_eq!(decoded, second);
        assert_eq!(used + rest, buf.len());
    }

    #[test]
    fn intact_clip_gives_back_its_content() {
        let content = text_clip("hello").into_content().expect("intact");
        assert_eq!(content, ClipContent::from("hello"));
    }

    #[test]
    fn tampered_payload_is_rejected() {
        let mut clip = text_clip("hello");
        clip.payload = Payload::Inline(b"jello".to_vec());
        assert_eq!(clip.into_content(), Err(ClipError::HashMismatch));
    }

    #[test]
    fn wrong_size_is_rejected() {
        let mut clip = text_clip("hello");
        clip.size = 4;
        assert_eq!(
            clip.into_content(),
            Err(ClipError::SizeMismatch {
                declared: 4,
                actual: 5
            })
        );
    }

    #[test]
    fn non_utf8_text_is_rejected() {
        let bytes = vec![0xff, 0xfe];
        let mut clip = text_clip("");
        clip.size = 2;
        clip.hash = ContentHash::of(&bytes);
        clip.payload = Payload::Inline(bytes);
        assert_eq!(clip.into_content(), Err(ClipError::NotUtf8));
    }

    #[test]
    fn image_clip_round_trips_and_gives_back_the_png() {
        let clip = image_clip();
        assert_eq!(clip.kind, ContentKind::Image);
        assert_eq!(clip.mime, "image/png");
        assert_eq!(clip.size, 70);
        assert_eq!(clip.hash, ContentHash::of(&TINY_PNG));
        assert_eq!(clip.payload, Payload::Inline(TINY_PNG.to_vec()));

        let Message::Clip(received) = round_trip(&Message::Clip(clip)) else {
            panic!("a clip decodes as a clip");
        };
        assert_eq!(received.into_content(), Ok(tiny_image()));
    }

    #[test]
    fn tampered_image_is_rejected() {
        let mut bytes = TINY_PNG.to_vec();
        bytes[20] ^= 1; // image width
        let mut clip = image_clip();
        clip.payload = Payload::Inline(bytes);
        assert_eq!(clip.into_content(), Err(ClipError::HashMismatch));
    }

    #[test]
    fn image_that_is_not_a_png_is_rejected() {
        // Size and hash match, so only the PNG check can catch it.
        let bytes = b"GIF89a, not a PNG".to_vec();
        let mut clip = image_clip();
        clip.size = bytes.len() as u64;
        clip.hash = ContentHash::of(&bytes);
        clip.payload = Payload::Inline(bytes);
        assert_eq!(clip.into_content(), Err(ClipError::NotPng));
    }

    #[test]
    fn debug_does_not_show_the_payload() {
        let text = format!("{:?}", text_clip("hunter2"));
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("payload: Inline { len: 7 }"), "{text}");
    }
}
