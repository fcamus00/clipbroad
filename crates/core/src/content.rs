use std::fmt;

use crate::ContentHash;

/// Something that can be on the clipboard and travel between machines.
///
/// Only text exists today. Images and files will be added as new variants. Match on it
/// exhaustively (no `_` arm), so adding a variant fails the build at every place that must handle it.
///
/// `Debug` prints the size and hash, never the content, so logging a clip cannot leak what was copied.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum ClipContent {
    /// Plain UTF-8 text, exactly as the OS clipboard returned it (line endings untouched).
    Text(String),
}

impl ClipContent {
    /// The text, if this is text content.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
        }
    }

    /// Content hash used for anti-echo and the wire protocol.
    ///
    /// What gets hashed, per kind (this is part of the protocol; do not change it):
    /// - `Text`: the UTF-8 bytes of the string, with no prefix or normalisation.
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
    }
}
