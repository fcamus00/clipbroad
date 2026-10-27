use crate::ContentHash;

/// Something that can be on the clipboard and travel between machines.
///
/// Only text exists today. Images and files will be added as new variants. Match on it
/// exhaustively (no `_` arm), so adding a variant fails the build at every place that must handle it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    fn as_text_returns_the_text() {
        assert_eq!(ClipContent::from("hi").as_text(), Some("hi"));
    }
}
