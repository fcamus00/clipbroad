use std::error::Error;

/// Error returned by [`Clipboard`](crate::Clipboard) operations.
///
/// "The clipboard is empty" and "the clipboard holds no text" are **not** errors; see
/// [`Clipboard::read_text`](crate::Clipboard::read_text).
#[derive(Debug, thiserror::Error)]
pub enum ClipboardError {
    /// The clipboard cannot be used at all in this session, and retrying will not help until the
    /// environment changes. Examples: no Wayland compositor, compositor without a data-control
    /// protocol, macOS denied pasteboard access.
    #[error("clipboard unavailable: {reason}")]
    Unavailable {
        /// Human-readable cause, shown to the user as is.
        reason: String,
    },

    /// Another application is holding the clipboard right now (Windows `OpenClipboard`
    /// contention). Transient: the caller may retry later. Backends should already retry briefly
    /// before returning this.
    #[error("clipboard is busy (held by another application)")]
    Busy,

    /// Any other failure reported by the OS or the backend library.
    #[error("clipboard backend error: {message}")]
    Backend {
        /// What the backend was doing when it failed, e.g. "reading text/plain offer".
        message: String,
        /// The underlying error, if any.
        #[source]
        source: Option<Box<dyn Error + Send + Sync>>,
    },
}

impl ClipboardError {
    /// Shorthand for [`ClipboardError::Backend`] with an underlying error.
    pub fn backend(
        message: impl Into<String>,
        source: impl Into<Box<dyn Error + Send + Sync>>,
    ) -> Self {
        Self::Backend {
            message: message.into(),
            source: Some(source.into()),
        }
    }
}
