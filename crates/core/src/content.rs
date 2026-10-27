use std::fmt;

use crate::ContentHash;

/// Something that can be on the clipboard and travel between machines.
///
/// Text and images exist today; files will be added as a new variant. Match on it exhaustively
/// (no `_` arm), so adding a variant fails the build at every place that must handle it.
///
/// `Debug` prints the size and hash, never the content, so logging a clip cannot leak what was copied.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum ClipContent {
    /// Plain UTF-8 text, exactly as the OS clipboard returned it (line endings untouched).
    Text(String),
    /// An image, always as PNG. Backends convert the OS's native format (TIFF on macOS, DIB on
    /// Windows) to and from PNG.
    Image(Png),
}

impl ClipContent {
    /// The text, if this is text content.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::Image(_) => None,
        }
    }

    /// Content hash used for anti-echo and the wire protocol.
    ///
    /// What gets hashed, per kind (this is part of the protocol; do not change it):
    /// - `Text`: the UTF-8 bytes of the string, with no prefix or normalisation.
    /// - `Image`: the PNG bytes, as they travel on the wire.
    ///
    /// Text and an image never hash the same bytes: a PNG starts with `0x89`, which no UTF-8 string
    /// can start with.
    ///
    /// ```
    /// use clipbroad_core::{ClipContent, ContentHash};
    ///
    /// let clip = ClipContent::from("hello");
    /// assert_eq!(clip.hash(), ContentHash::of(b"hello"));
    /// ```
    #[must_use]
    pub fn hash(&self) -> ContentHash {
        match self {
            Self::Text(text) => ContentHash::of(text.as_bytes()),
            Self::Image(png) => ContentHash::of(png.as_bytes()),
        }
    }
}

impl fmt::Debug for ClipContent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(text) => f
                .debug_struct("Text")
                .field("len", &text.len())
                .field("hash", &self.hash())
                .finish(),
            Self::Image(png) => f
                .debug_struct("Image")
                .field("len", &png.as_bytes().len())
                .field("hash", &self.hash())
                .finish(),
        }
    }
}

impl From<String> for ClipContent {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

impl From<&str> for ClipContent {
    fn from(text: &str) -> Self {
        Self::Text(text.to_owned())
    }
}

/// The bytes of a PNG file. Only built from bytes that start like a PNG (see [`Png::try_from`]),
/// so a backend never gets arbitrary bytes to put on the clipboard as an image.
///
/// The image itself is not decoded: `core` stays free of image codecs, and a PNG that is broken
/// past its header fails in the app that pastes it, like a broken PNG copied locally would.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Png(Vec<u8>);

impl Png {
    /// The 8-byte PNG signature, then the length (13) and type of the `IHDR` chunk, which the PNG
    /// spec requires to come first.
    const START: [u8; 16] = *b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR";

    /// The PNG file bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Gives back the PNG file bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

impl TryFrom<Vec<u8>> for Png {
    type Error = NotPng;

    /// Accepts `bytes` if they start with the PNG signature followed by an `IHDR` chunk.
    fn try_from(bytes: Vec<u8>) -> Result<Self, NotPng> {
        if bytes.starts_with(&Self::START) {
            Ok(Self(bytes))
        } else {
            Err(NotPng)
        }
    }
}

/// `Debug` prints the length, never the bytes.
impl fmt::Debug for Png {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Png").field("len", &self.0.len()).finish()
    }
}

/// The bytes given to [`Png::try_from`] do not start like a PNG file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("not a PNG image")]
pub struct NotPng;

/// A real 1x1 RGBA PNG (70 bytes), for tests.
#[cfg(test)]
pub(crate) const TINY_PNG: [u8; 70] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

/// [`TINY_PNG`] as clipboard content.
#[cfg(test)]
pub(crate) fn tiny_image() -> ClipContent {
    ClipContent::Image(Png::try_from(TINY_PNG.to_vec()).expect("TINY_PNG is a PNG"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn different_text_different_hash() {
        assert_ne!(
            ClipContent::from("a\nb").hash(),
            ClipContent::from("a\r\nb").hash()
        );
    }

    #[test]
    fn debug_does_not_show_the_content() {
        let text = format!("{:?}", ClipContent::from("hunter2"));
        assert!(!text.contains("hunter2"), "{text}");
        assert!(
            text.starts_with("Text { len: 7, hash: ContentHash("),
            "{text}"
        );
    }

    #[test]
    fn as_text_returns_the_text() {
        assert_eq!(ClipContent::from("hi").as_text(), Some("hi"));
        assert_eq!(tiny_image().as_text(), None);
    }

    #[test]
    fn image_hash_is_the_hash_of_the_png_bytes() {
        assert_eq!(tiny_image().hash(), ContentHash::of(&TINY_PNG));
    }

    #[test]
    fn png_accepts_a_real_png() {
        let png = Png::try_from(TINY_PNG.to_vec()).expect("a PNG");
        assert_eq!(png.as_bytes(), TINY_PNG);
        assert_eq!(png.into_bytes(), TINY_PNG);
    }

    #[test]
    fn png_rejects_other_bytes() {
        let mut no_ihdr = TINY_PNG.to_vec();
        no_ihdr[12..16].copy_from_slice(b"IDAT");
        let jpeg = vec![0xff, 0xd8, 0xff, 0xe0, 0, 0x10, b'J', b'F', b'I', b'F'];
        for bytes in [vec![], TINY_PNG[..15].to_vec(), no_ihdr, jpeg] {
            assert_eq!(Png::try_from(bytes), Err(NotPng));
        }
    }

    #[test]
    fn image_debug_does_not_show_the_bytes() {
        let image = tiny_image();
        assert_eq!(
            format!("{image:?}"),
            format!("Image {{ len: 70, hash: {:?} }}", image.hash())
        );
        let ClipContent::Image(png) = image else {
            panic!("tiny_image is an image");
        };
        assert_eq!(format!("{png:?}"), "Png { len: 70 }");
    }
}
