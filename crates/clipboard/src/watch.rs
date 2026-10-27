//! Change notifications: the channel between a backend's watcher thread and the daemon.

use tokio::sync::mpsc;

use crate::ClipboardError;

/// One observed change of the system clipboard.
///
/// It says *that* the clipboard changed and which formats it now offers, but carries no content.
/// To get the content, call [`Clipboard::read_text`](crate::Clipboard::read_text) after deciding
/// that reading is wanted (for example, after checking `formats` for sensitive-content markers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardChange {
    /// The formats offered by the clipboard right after the change, using each platform's
    /// **native identifiers, verbatim**, in the order the OS reports them:
    ///
    /// - **Linux (Wayland, X11):** MIME types / targets as offered, e.g.
    ///   `text/plain;charset=utf-8`, `UTF8_STRING`, `x-kde-passwordManagerHint`.
    /// - **macOS:** pasteboard type identifiers (UTIs) of all items, e.g. `public.utf8-plain-text`,
    ///   `org.nspasteboard.ConcealedType`. Obtain them without reading the data.
    /// - **Windows:** registered formats by their registered name (`GetClipboardFormatNameW`),
    ///   e.g. `ExcludeClipboardContentFromMonitorProcessing`; predefined formats by their constant
    ///   name, e.g. `CF_UNICODETEXT`, `CF_HDROP`; any other unnamed format as `#` plus its decimal
    ///   id, e.g. `#49999`.
    ///
    /// Empty when the clipboard became empty (cleared, or on Wayland the owning app exited).
    pub formats: Vec<String>,
}

impl ClipboardChange {
    /// A change offering `formats` (see [`ClipboardChange::formats`] for the naming rules).
    #[must_use]
    pub fn new(formats: Vec<String>) -> Self {
        Self { formats }
    }

    /// Whether `format` is among the offered formats (exact, case-sensitive match).
    #[must_use]
    pub fn has_format(&self, format: &str) -> bool {
        self.formats.iter().any(|f| f == format)
    }
}

type Message = Result<ClipboardChange, ClipboardError>;

/// Backend side of a watch: reports changes from any thread, sync or async.
#[derive(Debug, Clone)]
pub struct ChangeSender {
    tx: mpsc::UnboundedSender<Message>,
}

impl ChangeSender {
    /// Reports a change. Never blocks; safe to call from a plain OS thread or a callback.
    ///
    /// # Errors
    ///
    /// [`WatchClosed`] if the consumer dropped its [`ClipboardWatch`]. The backend must then stop
    /// watching and release its resources (end the thread, unregister listeners).
    pub fn send(&self, change: ClipboardChange) -> Result<(), WatchClosed> {
        self.tx.send(Ok(change)).map_err(|_| WatchClosed)
    }

    /// Reports that watching failed for good (e.g. the compositor connection was lost) and gives
    /// up this sender. The consumer receives the error, then the stream ends once every clone of
    /// this sender is gone. Use this instead of silently stopping, so the daemon can tell the user.
    pub fn fail(self, error: ClipboardError) {
        // If the consumer is already gone there is nobody left to tell; ignoring is correct.
        let _ = self.tx.send(Err(error));
    }

    /// Whether the consumer dropped its [`ClipboardWatch`]. Polling backends should check this on
    /// every tick so they stop even when the clipboard does not change.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

/// Consumer side of a watch, returned by [`Clipboard::watch`](crate::Clipboard::watch).
///
/// Dropping it tells the backend to stop watching.
///
/// # Why a channel and not a callback
///
/// Real backends observe the clipboard in very different ways: Wayland and Windows block a
/// dedicated thread on an event source, macOS polls `changeCount` on a timer. A channel fits both:
/// the backend owns a plain OS thread and pushes into a [`ChangeSender`] with a non-blocking,
/// synchronous [`send`](ChangeSender::send); the daemon consumes from async code with
/// [`recv`](Self::recv). No backend code has to run inside the daemon's async runtime.
///
/// The channel is unbounded on purpose. Clipboard changes happen at human speed and events are
/// small (a list of format names, never the content), so memory is not a concern, while a bounded
/// channel would force backends to either block their OS event loop or drop the newest event,
/// which is the one that matters under "last writer wins".
#[derive(Debug)]
pub struct ClipboardWatch {
    rx: mpsc::UnboundedReceiver<Message>,
}

impl ClipboardWatch {
    /// Creates a connected pair: the backend keeps the [`ChangeSender`] (usually moving it into
    /// its watcher thread), and [`Clipboard::watch`](crate::Clipboard::watch) returns the
    /// [`ClipboardWatch`].
    #[must_use]
    pub fn channel() -> (ChangeSender, Self) {
        let (tx, rx) = mpsc::unbounded_channel();
        (ChangeSender { tx }, Self { rx })
    }

    /// Waits for the next change.
    ///
    /// Returns `Some(Ok(change))` for each change, `Some(Err(_))` once if the watcher failed, and
    /// `None` when the watcher has stopped (after an error, or if the backend went away). Once it
    /// returns `None` it keeps returning `None`.
    ///
    /// Cancel-safe: dropping the future (e.g. in `tokio::select!`) loses no change.
    pub async fn recv(&mut self) -> Option<Result<ClipboardChange, ClipboardError>> {
        self.rx.recv().await
    }
}

/// The consumer dropped its [`ClipboardWatch`]; the backend should stop watching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("clipboard watch was closed by the consumer")]
pub struct WatchClosed;

#[cfg(test)]
mod tests {
    use super::*;

    fn change(formats: &[&str]) -> ClipboardChange {
        ClipboardChange::new(formats.iter().map(|&f| f.to_owned()).collect())
    }

    #[tokio::test]
    async fn delivers_changes_sent_from_an_os_thread() {
        let (tx, mut watch) = ClipboardWatch::channel();
        std::thread::spawn(move || {
            tx.send(change(&["a"])).expect("watch is alive");
            tx.send(change(&["b"])).expect("watch is alive");
        })
        .join()
        .expect("sender thread panicked");

        assert_eq!(
            watch.recv().await.expect("open").expect("ok"),
            change(&["a"])
        );
        assert_eq!(
            watch.recv().await.expect("open").expect("ok"),
            change(&["b"])
        );
        assert!(
            watch.recv().await.is_none(),
            "stream ends when the sender is gone"
        );
    }

    #[tokio::test]
    async fn fail_delivers_the_error_then_ends() {
        let (tx, mut watch) = ClipboardWatch::channel();
        tx.fail(ClipboardError::Busy);
        assert!(matches!(
            watch.recv().await,
            Some(Err(ClipboardError::Busy))
        ));
        assert!(watch.recv().await.is_none());
    }

    #[test]
    fn sender_notices_when_the_watch_is_dropped() {
        let (tx, watch) = ClipboardWatch::channel();
        assert!(!tx.is_closed());
        drop(watch);
        assert!(tx.is_closed());
        assert_eq!(tx.send(change(&[])), Err(WatchClosed));
    }

    #[test]
    fn has_format_is_exact() {
        let c = change(&["text/plain", "x-kde-passwordManagerHint"]);
        assert!(c.has_format("x-kde-passwordManagerHint"));
        assert!(!c.has_format("text/plain;charset=utf-8"));
    }
}
