//! In-memory [`Clipboard`] for tests of the sync logic.
//!
//! It follows the same contract as the real backends, including emitting a change event for its
//! own [`write_text`](Clipboard::write_text), so logic tested against it (anti-echo especially)
//! behaves the same on real clipboards.
//!
//! ```
//! use clipbroad_clipboard::Clipboard;
//! use clipbroad_clipboard::fake::{FakeClipboard, TEXT_FORMAT};
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> Result<(), clipbroad_clipboard::ClipboardError> {
//! let clipboard = FakeClipboard::new();
//! let mut watch = clipboard.watch()?;
//!
//! // Another app copies something.
//! clipboard.simulate_copy_text("hello");
//!
//! let change = watch.recv().await.expect("watch is open")?;
//! assert!(change.has_format(TEXT_FORMAT));
//! assert_eq!(clipboard.read_text()?.as_deref(), Some("hello"));
//! # Ok(())
//! # }
//! ```

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use clipbroad_core::sensitive::sensitive_marker;

use crate::{ChangeSender, Clipboard, ClipboardChange, ClipboardError, ClipboardWatch};

/// The format the fake offers for text. A MIME type, as on Linux.
pub const TEXT_FORMAT: &str = "text/plain;charset=utf-8";

/// An in-memory clipboard. Clones share the same state, so a test can keep one handle to
/// simulate other apps while the code under test owns another.
#[derive(Debug, Clone, Default)]
pub struct FakeClipboard {
    state: Arc<Mutex<State>>,
}

#[derive(Debug, Default)]
struct State {
    text: Option<String>,
    /// What the current content offers, checked by `read_text` like the real backends do.
    formats: Vec<String>,
    watchers: Vec<ChangeSender>,
}

impl FakeClipboard {
    /// An empty clipboard with no watchers.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Simulates another application copying plain text (offered as [`TEXT_FORMAT`]).
    pub fn simulate_copy_text(&self, text: impl Into<String>) {
        self.simulate_copy(Some(text.into()), vec![TEXT_FORMAT.to_owned()]);
    }

    /// Simulates another application copying arbitrary content.
    ///
    /// `text` is what [`read_text`](Clipboard::read_text) will return, unless `formats` holds a
    /// sensitive-content marker; `formats` is what the change event reports. Use it for content a
    /// plain-text copy can't express, e.g. a password manager (`Some("hunter2")` with
    /// `x-kde-passwordManagerHint` among the formats) or an image (`None` with `["image/png"]`).
    pub fn simulate_copy(&self, text: Option<String>, formats: Vec<String>) {
        let mut state = self.lock();
        let change = ClipboardChange::new(formats.clone());
        state.text = text;
        state.formats = formats;
        // Notify every live watcher and forget the ones whose consumer is gone.
        state
            .watchers
            .retain(|watcher| watcher.send(change.clone()).is_ok());
    }

    /// Simulates the clipboard being cleared (an event with no formats).
    pub fn simulate_clear(&self) {
        self.simulate_copy(None, Vec::new());
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic while holding the lock can only come from a failing test; the state itself is
        // always consistent, so keep going rather than cascading panics.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Clipboard for FakeClipboard {
    fn read_text(&self) -> Result<Option<String>, ClipboardError> {
        // Text and formats change together under the lock, so the check is atomic.
        let state = self.lock();
        match sensitive_marker(&state.formats) {
            Some(marker) => Err(ClipboardError::Sensitive { marker }),
            None => Ok(state.text.clone()),
        }
    }

    fn write_text(&self, text: &str) -> Result<(), ClipboardError> {
        self.simulate_copy_text(text);
        Ok(())
    }

    fn watch(&self) -> Result<ClipboardWatch, ClipboardError> {
        let (sender, watch) = ClipboardWatch::channel();
        self.lock().watchers.push(sender);
        Ok(watch)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    async fn next_change(watch: &mut ClipboardWatch) -> ClipboardChange {
        watch
            .recv()
            .await
            .expect("watch should be open")
            .expect("fake never fails")
    }

    async fn assert_no_change(watch: &mut ClipboardWatch) {
        let next = tokio::time::timeout(Duration::from_millis(20), watch.recv()).await;
        assert!(next.is_err(), "unexpected event: {next:?}");
    }

    #[test]
    fn starts_empty() {
        assert_eq!(FakeClipboard::new().read_text().expect("read"), None);
    }

    #[test]
    fn returns_what_was_written() {
        let clipboard = FakeClipboard::new();
        clipboard.write_text("written").expect("write");
        assert_eq!(
            clipboard.read_text().expect("read").as_deref(),
            Some("written")
        );
    }

    #[tokio::test]
    async fn simulated_copy_emits_event_and_is_readable() {
        let clipboard = FakeClipboard::new();
        let mut watch = clipboard.watch().expect("watch");

        clipboard.simulate_copy_text("from another app");

        let change = next_change(&mut watch).await;
        assert_eq!(change.formats, [TEXT_FORMAT]);
        assert_eq!(
            clipboard.read_text().expect("read").as_deref(),
            Some("from another app")
        );
    }

    #[tokio::test]
    async fn own_writes_are_reported_too() {
        let clipboard = FakeClipboard::new();
        let mut watch = clipboard.watch().expect("watch");

        clipboard.write_text("ours").expect("write");

        assert!(next_change(&mut watch).await.has_format(TEXT_FORMAT));
    }

    #[tokio::test]
    async fn no_event_for_content_present_before_watching() {
        let clipboard = FakeClipboard::new();
        clipboard.simulate_copy_text("old");
        let mut watch = clipboard.watch().expect("watch");
        assert_no_change(&mut watch).await;
    }

    #[tokio::test]
    async fn reports_formats_of_non_text_and_sensitive_content() {
        let clipboard = FakeClipboard::new();
        let mut watch = clipboard.watch().expect("watch");

        clipboard.simulate_copy(None, vec!["image/png".to_owned()]);
        assert_eq!(next_change(&mut watch).await.formats, ["image/png"]);
        assert_eq!(clipboard.read_text().expect("read"), None);

        clipboard.simulate_copy(
            Some("hunter2".to_owned()),
            vec![
                TEXT_FORMAT.to_owned(),
                "x-kde-passwordManagerHint".to_owned(),
            ],
        );
        assert!(
            next_change(&mut watch)
                .await
                .has_format("x-kde-passwordManagerHint")
        );
    }

    #[test]
    fn refuses_to_read_sensitive_content() {
        let clipboard = FakeClipboard::new();
        clipboard.simulate_copy(
            Some("hunter2".to_owned()),
            vec![
                TEXT_FORMAT.to_owned(),
                "x-kde-passwordManagerHint".to_owned(),
            ],
        );
        assert!(matches!(
            clipboard.read_text(),
            Err(ClipboardError::Sensitive {
                marker: "x-kde-passwordManagerHint"
            })
        ));

        // The next plain copy is readable again.
        clipboard.simulate_copy_text("public");
        assert_eq!(
            clipboard.read_text().expect("read").as_deref(),
            Some("public")
        );
    }

    #[tokio::test]
    async fn clear_reports_no_formats() {
        let clipboard = FakeClipboard::new();
        clipboard.simulate_copy_text("something");
        let mut watch = clipboard.watch().expect("watch");

        clipboard.simulate_clear();

        assert_eq!(
            next_change(&mut watch).await,
            ClipboardChange::new(Vec::new())
        );
        assert_eq!(clipboard.read_text().expect("read"), None);
    }

    #[tokio::test]
    async fn every_watch_gets_every_change_and_dropped_ones_are_forgotten() {
        let clipboard = FakeClipboard::new();
        let mut first = clipboard.watch().expect("watch");
        let second = clipboard.watch().expect("watch");
        drop(second);

        clipboard.simulate_copy_text("x");

        assert!(next_change(&mut first).await.has_format(TEXT_FORMAT));
        assert_eq!(clipboard.lock().watchers.len(), 1);
    }

    #[tokio::test]
    async fn clones_share_state() {
        let clipboard = FakeClipboard::new();
        let other_app = clipboard.clone();
        let mut watch = clipboard.watch().expect("watch");

        other_app.simulate_copy_text("shared");

        next_change(&mut watch).await;
        assert_eq!(
            clipboard.read_text().expect("read").as_deref(),
            Some("shared")
        );
    }

    #[test]
    fn usable_as_trait_object() {
        let clipboard: Arc<dyn Clipboard> = Arc::new(FakeClipboard::new());
        clipboard.write_text("dyn").expect("write");
        assert_eq!(clipboard.read_text().expect("read").as_deref(), Some("dyn"));
    }
}
