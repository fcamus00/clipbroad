//! [`Clipboard`] backend for macOS, on the general `NSPasteboard`.
//!
//! Built on the [`objc2_app_kit`] bindings. `NSPasteboard` is not tied to the main thread (the
//! bindings do not require a `MainThreadMarker` for it), so every call fetches the general
//! pasteboard on whatever thread it runs on. Each call runs inside its own autorelease pool,
//! because the threads here have no run loop that would drain one.
//!
//! # Threads
//!
//! - [`read_text`](Clipboard::read_text): runs on a short-lived thread so the caller can give up
//!   after [`READ_TIMEOUT`]. The app that copied may provide its data lazily, and a frozen app
//!   would otherwise block the caller. A read that times out leaves its thread to finish on its
//!   own.
//! - [`watch`](Clipboard::watch): macOS has no change notification, so one thread polls
//!   `changeCount` every [`POLL_INTERVAL`]. It ends at the first tick after the
//!   [`ClipboardWatch`] is dropped.
//!
//! # Privacy (macOS 15.4+)
//!
//! macOS can alert the user when an app reads the pasteboard programmatically. To stay clear of
//! that, the watcher only reads metadata (`changeCount` and the offered `types`). The text itself
//! is read only in [`read_text`](Clipboard::read_text), and only after checking that the
//! pasteboard offers text. [`MacosClipboard::access_behavior`] reports what the user allowed.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use objc2::available;
use objc2::rc::autoreleasepool;
use objc2_app_kit::{NSPasteboard, NSPasteboardAccessBehavior};
use objc2_foundation::{NSArray, NSInteger, NSString, ns_string};

use crate::{ChangeSender, Clipboard, ClipboardChange, ClipboardError, ClipboardWatch};

/// The type we read and write: `NSPasteboardTypeString`.
const TEXT_UTF8: &str = "public.utf8-plain-text";

/// Longest wait for the app that owns the pasteboard to hand over its content.
const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// How often a watch checks `changeCount` and whether its consumer is gone.
const POLL_INTERVAL: Duration = Duration::from_millis(300);

/// Wait before listing the types again when a change shows none (see [`poll_changes`]).
const EMPTY_RETRY_DELAY: Duration = Duration::from_millis(50);

/// The general macOS pasteboard (the one Cmd+C and Cmd+V use).
#[derive(Debug, Clone, Copy, Default)]
pub struct MacosClipboard;

/// What the user allows this app to do with the general pasteboard, set in System Settings >
/// Privacy & Security > Paste from Other Apps. Mirrors `NSPasteboardAccessBehavior`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessBehavior {
    /// The app never triggered a pasteboard alert, so it is not listed in System Settings yet.
    /// macOS asks the user on programmatic reads.
    Default,
    /// macOS asks the user before each programmatic read.
    Ask,
    /// Reads are allowed without asking.
    AlwaysAllow,
    /// Programmatic reads are denied without asking.
    AlwaysDeny,
    /// A value added by a newer macOS, as its raw number.
    Other(isize),
}

impl MacosClipboard {
    /// The pasteboard access the user granted this app, or `None` before macOS 15.4, which has
    /// no such setting (reads are always allowed there). Reading it does not trigger an alert.
    #[must_use]
    pub fn access_behavior(&self) -> Option<AccessBehavior> {
        if !available!(macos = 15.4) {
            return None;
        }
        let behavior = autoreleasepool(|_| NSPasteboard::generalPasteboard().accessBehavior());
        Some(match behavior {
            NSPasteboardAccessBehavior::Default => AccessBehavior::Default,
            NSPasteboardAccessBehavior::Ask => AccessBehavior::Ask,
            NSPasteboardAccessBehavior::AlwaysAllow => AccessBehavior::AlwaysAllow,
            NSPasteboardAccessBehavior::AlwaysDeny => AccessBehavior::AlwaysDeny,
            NSPasteboardAccessBehavior(other) => AccessBehavior::Other(other),
        })
    }
}

impl Clipboard for MacosClipboard {
    fn read_text(&self) -> Result<Option<String>, ClipboardError> {
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("mac-read".to_owned())
            .spawn(move || {
                // After a timeout nobody is waiting for the result anymore; dropping it is fine.
                let _ = result_tx.send(read_string());
            })
            .map_err(|error| ClipboardError::backend("spawning the read thread", error))?;
        match result_rx.recv_timeout(READ_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(ClipboardError::Backend {
                message: format!(
                    "the app that owns the clipboard did not send its content within {}s",
                    READ_TIMEOUT.as_secs()
                ),
                source: None,
            }),
            Err(RecvTimeoutError::Disconnected) => Err(ClipboardError::Backend {
                message: "the read thread ended without a result".to_owned(),
                source: None,
            }),
        }
    }

    fn write_text(&self, text: &str) -> Result<(), ClipboardError> {
        autoreleasepool(|_| {
            let pasteboard = NSPasteboard::generalPasteboard();
            // Taking ownership of the pasteboard is required before writing to it.
            pasteboard.clearContents();
            if pasteboard.setString_forType(&NSString::from_str(text), ns_string!(TEXT_UTF8)) {
                Ok(())
            } else {
                Err(ClipboardError::Backend {
                    message: "macOS refused to set the pasteboard text".to_owned(),
                    source: None,
                })
            }
        })
    }

    fn watch(&self) -> Result<ClipboardWatch, ClipboardError> {
        let (changes, clipboard_watch) = ClipboardWatch::channel();
        // Read before returning, so a change right after `watch` returns is not taken as the
        // starting state.
        let start = autoreleasepool(|_| NSPasteboard::generalPasteboard().changeCount());
        thread::Builder::new()
            .name("mac-watch".to_owned())
            .spawn(move || poll_changes(start, &changes))
            .map_err(|error| ClipboardError::backend("spawning the watcher thread", error))?;
        Ok(clipboard_watch)
    }
}

/// Runs on a read thread. Requests the text only if the pasteboard offers it, so an image or a
/// file copy is never read.
fn read_string() -> Result<Option<String>, ClipboardError> {
    autoreleasepool(|_| {
        let pasteboard = NSPasteboard::generalPasteboard();
        let text_type = ns_string!(TEXT_UTF8);
        if pasteboard
            .availableTypeFromArray(&NSArray::from_slice(&[text_type]))
            .is_none()
        {
            return Ok(None);
        }
        match pasteboard.stringForType(text_type) {
            Some(text) => Ok(Some(text.to_string())),
            None if MacosClipboard.access_behavior() == Some(AccessBehavior::AlwaysDeny) => {
                Err(ClipboardError::Unavailable {
                    reason: "macOS denies clipboard access to this app; allow it in System \
                             Settings > Privacy & Security > Paste from Other Apps"
                        .to_owned(),
                })
            }
            // The pasteboard changed since the check, or the text could not be converted.
            None => Ok(None),
        }
    })
}

/// Runs on the watcher thread: reports each new `changeCount` until the consumer is gone.
fn poll_changes(mut last_count: NSInteger, changes: &ChangeSender) {
    loop {
        thread::sleep(POLL_INTERVAL);
        if changes.is_closed() {
            return;
        }
        let change = autoreleasepool(|_| {
            let pasteboard = NSPasteboard::generalPasteboard();
            let count = pasteboard.changeCount();
            (count != last_count).then(|| (count, formats_of(&pasteboard)))
        });
        let Some((count, mut formats)) = change else {
            continue;
        };
        // An app copies in two steps: `clearContents` bumps `changeCount`, then it writes the
        // data, which does not bump it again. A tick between the two sees no types, and reporting
        // that would lose the copy, so look once more before calling it empty.
        if formats.is_empty() {
            thread::sleep(EMPTY_RETRY_DELAY);
            formats = autoreleasepool(|_| formats_of(&NSPasteboard::generalPasteboard()));
        }
        last_count = count;
        if changes.send(ClipboardChange::new(formats)).is_err() {
            return;
        }
    }
}

/// The types offered by all pasteboard items, without reading any data. If macOS cannot list
/// them, no formats: the daemon never syncs such a change, so this fails closed.
fn formats_of(pasteboard: &NSPasteboard) -> Vec<String> {
    let Some(types) = pasteboard.types() else {
        tracing::warn!("could not list the pasteboard types; reporting the change with no formats");
        return Vec::new();
    };
    types.iter().map(|format| format.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn watcher_thread_ends_after_the_watch_is_dropped() {
        let (changes, watch) = ClipboardWatch::channel();
        let poller = thread::spawn(move || poll_changes(0, &changes));
        drop(watch);

        let deadline = Instant::now() + POLL_INTERVAL * 3;
        while !poller.is_finished() {
            assert!(Instant::now() < deadline, "watcher thread still running");
            thread::sleep(Duration::from_millis(20));
        }
    }
}
