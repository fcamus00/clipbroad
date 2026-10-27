//! OS clipboard access: one `Clipboard` trait with a backend per platform
//! (Wayland, X11, macOS, Windows) plus an in-memory fake for tests.
//!
//! The daemon only ever talks to [`Clipboard`]; everything platform-specific lives in a backend
//! behind `#[cfg(target_os = ...)]`. Sync logic is tested against [`fake::FakeClipboard`].

mod error;
pub mod fake;
#[cfg(target_os = "macos")]
pub mod macos;
mod watch;
#[cfg(target_os = "linux")]
pub mod wayland;

pub use error::ClipboardError;
pub use watch::{ChangeSender, ClipboardChange, ClipboardWatch, WatchClosed};

/// Access to the system clipboard (on Linux: the regular clipboard, not the primary selection).
///
/// This is the contract every backend implements. Backends are written independently per
/// platform, so the rules below are what keeps them interchangeable; when in doubt, follow them
/// literally and document any platform limitation in the backend.
///
/// # Threading and blocking
///
/// - Implementations are `Send + Sync`; all methods take `&self`, so one instance can be shared
///   (e.g. in an `Arc`) and called from any thread. Use interior mutability where needed.
/// - [`read_text`](Self::read_text) and [`write_text`](Self::write_text) are synchronous and may
///   block briefly (talking to the compositor, waiting for another app to hand over data, retrying
///   a busy Windows clipboard). Aim for well under a second, and **never block for more than
///   2 seconds**: if the source app does not hand over the data in time (a frozen app on Wayland),
///   give up and return [`ClipboardError::Backend`]. The backend owns this limit because the
///   daemon cannot cancel a blocked thread. Async callers should run them with
///   `tokio::task::spawn_blocking`.
/// - No method requires a Tokio runtime or a particular calling thread. Backends that need a
///   dedicated thread (an event loop, a message window, a polling timer) spawn their own
///   `std::thread`.
///
/// # Content
///
/// - Text is UTF-8 and is returned and written **verbatim**: no trimming, no line-ending
///   conversion, no size limit at this layer.
/// - Sensitive-content markers (password managers) are *not* filtered here. Backends report them
///   in [`ClipboardChange::formats`] and the daemon decides.
///
/// # Watching
///
/// See [`watch`](Self::watch).
pub trait Clipboard: Send + Sync {
    /// Reads the current clipboard content as text.
    ///
    /// Returns:
    /// - `Ok(Some(text))` if the clipboard offers a text format. If several text formats are
    ///   offered, prefer the UTF-8 one (`text/plain;charset=utf-8` / `UTF8_STRING` on Linux,
    ///   `public.utf8-plain-text` on macOS, `CF_UNICODETEXT` on Windows). An offered but empty
    ///   text is `Ok(Some(""))`.
    /// - `Ok(None)` if the clipboard is empty, holds only non-text content (an image, files), or
    ///   the text cannot be decoded as UTF-8 without loss. None of these is an error.
    ///
    /// # Errors
    ///
    /// Only for real failures: [`ClipboardError::Unavailable`] when the clipboard cannot be
    /// accessed at all, [`ClipboardError::Busy`] when another app keeps it locked after a few
    /// retries, [`ClipboardError::Backend`] for anything else.
    fn read_text(&self) -> Result<Option<String>, ClipboardError>;

    /// Replaces the whole clipboard content with `text`.
    ///
    /// After `Ok(())`, other applications can paste `text` until something else takes the
    /// clipboard. Where the platform requires the source app to keep serving the data (Wayland),
    /// the backend does so in the background; the caller does nothing more. The text is offered in
    /// the platform's usual plain-text formats and with no sensitive-content markers. An empty
    /// `text` is written as empty text; it does not clear the clipboard.
    ///
    /// Writing produces a change event like any other change (see [`watch`](Self::watch)).
    ///
    /// # Errors
    ///
    /// Same categories as [`read_text`](Self::read_text).
    fn write_text(&self, text: &str) -> Result<(), ClipboardError>;

    /// Starts watching the clipboard and returns the stream of changes.
    ///
    /// Contract for implementations:
    ///
    /// - Create the pair with [`ClipboardWatch::channel`], start observing (typically moving the
    ///   [`ChangeSender`] into a new `std::thread`), and return the [`ClipboardWatch`]. Push one
    ///   [`ClipboardChange`] per observed change with [`ChangeSender::send`].
    /// - Return `Err` right away if watching cannot start (e.g. the compositor lacks a
    ///   data-control protocol). If it later fails for good, report it with
    ///   [`ChangeSender::fail`] instead of stopping silently.
    /// - No event for the content already present when watching starts; call
    ///   [`read_text`](Self::read_text) for that.
    /// - **Every** change is reported, including those caused by this process's own
    ///   [`write_text`](Self::write_text). Do not try to filter them: anti-echo is done by the
    ///   daemon, by content hash, the same way on every platform.
    /// - A clipboard that becomes empty is reported as a change with no formats.
    /// - If the formats cannot be listed after brief retries (on Windows another process may hold
    ///   the clipboard open), send the event with no formats and log a warning. Do not call
    ///   [`ChangeSender::fail`] for this: it is transient. The daemon treats an event with no
    ///   formats as "nothing to sync", so this fails closed.
    /// - Several quick changes may be reported as one (polling backends will naturally do this),
    ///   and a change may be reported when the content did not actually change. The formats in
    ///   an event must describe the clipboard at the moment the change was observed.
    /// - Each call starts an independent watch. When the consumer drops its [`ClipboardWatch`],
    ///   [`ChangeSender::send`] starts failing and [`ChangeSender::is_closed`] returns `true`; the
    ///   backend must then stop and release its resources, at the latest when it next wakes up
    ///   (next change or next polling tick).
    ///
    /// For consumers: an event says the clipboard *may* have changed and which formats it offers.
    /// Check the formats first (sensitive markers, non-text content), then call
    /// [`read_text`](Self::read_text) if wanted. The content can change again between the event
    /// and the read; if so, another event follows.
    ///
    /// # Errors
    ///
    /// [`ClipboardError::Unavailable`] if the platform cannot notify or be polled for changes in
    /// this session, [`ClipboardError::Backend`] if setting up the watcher failed.
    fn watch(&self) -> Result<ClipboardWatch, ClipboardError>;
}
