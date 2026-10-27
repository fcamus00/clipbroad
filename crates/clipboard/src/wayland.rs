//! [`Clipboard`] backend for Wayland compositors with a data-control protocol
//! (`ext-data-control-v1` or `wlr-data-control-unstable-v1`), such as Hyprland, Sway or KDE.
//!
//! Built on [`wl_clipboard_rs`]. Every call opens its own short-lived compositor connection, so
//! the type holds no state and is trivially `Send + Sync`.
//!
//! # Threads
//!
//! - [`write_text`](Clipboard::write_text): on Wayland the app that copied must keep serving the
//!   content to whoever pastes. `wl_clipboard_rs` does this on a background thread that lives
//!   until another copy replaces ours (the compositor then cancels our offer and the thread ends)
//!   or the process exits. So there is at most one such thread per process in steady state, and
//!   the clipboard empties when the process exits (unlike `wl-copy`, which forks a process that
//!   outlives the shell command).
//! - [`watch`](Clipboard::watch): one thread blocked on the compositor connection, plus one that
//!   checks every [`STOP_CHECK_INTERVAL`] whether the [`ClipboardWatch`] was dropped and, if so,
//!   wakes the first one up. Both end shortly after the watch is dropped, even if the clipboard
//!   never changes again.
//!
//! # Known compositor gaps
//!
//! Hyprland (checked on 0.56.2) sends no data-control event when the clipboard is cleared or when
//! the app that owned it exits, so no empty-formats change is reported there. The backend reports
//! one whenever the compositor does send it.

use std::error::Error;
use std::io::{self, Read};
use std::os::fd::AsFd;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec};
use wl_clipboard_rs::watch::{ClipboardEvent, Watcher};
use wl_clipboard_rs::{copy, paste, watch};

use crate::{ChangeSender, Clipboard, ClipboardChange, ClipboardError, ClipboardWatch};

/// The MIME type we read first and write as the main offer.
const TEXT_UTF8: &str = "text/plain;charset=utf-8";

/// Longest wait for the app that owns the clipboard to hand over its content.
const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// How often a watch checks whether its consumer is gone.
const STOP_CHECK_INTERVAL: Duration = Duration::from_millis(200);

/// The regular Wayland clipboard (not the primary selection).
#[derive(Debug)]
pub struct WaylandClipboard {
    _private: (),
}

impl WaylandClipboard {
    /// Creates the backend for the current Wayland session.
    ///
    /// Only checks that there is a session; problems with the compositor itself (e.g. no
    /// data-control protocol) show up as [`ClipboardError::Unavailable`] on the first call.
    ///
    /// # Errors
    ///
    /// [`ClipboardError::Unavailable`] if `WAYLAND_DISPLAY` is not set (no Wayland session, e.g.
    /// X11 or a plain TTY).
    pub fn new() -> Result<Self, ClipboardError> {
        if std::env::var_os("WAYLAND_DISPLAY").is_none_or(|display| display.is_empty()) {
            return Err(ClipboardError::Unavailable {
                reason: "no Wayland session (WAYLAND_DISPLAY is not set); X11 is not supported yet"
                    .to_owned(),
            });
        }
        Ok(Self { _private: () })
    }
}

impl Clipboard for WaylandClipboard {
    fn read_text(&self) -> Result<Option<String>, ClipboardError> {
        // `MimeType::Text` prefers `text/plain;charset=utf-8`, then `UTF8_STRING`, then any other
        // text type.
        let pipe = match paste::get_contents(
            paste::ClipboardType::Regular,
            paste::Seat::Unspecified,
            paste::MimeType::Text,
        ) {
            Ok((pipe, _mime_type)) => pipe,
            Err(paste::Error::ClipboardEmpty | paste::Error::NoMimeType) => return Ok(None),
            Err(error) => return Err(paste_error("requesting clipboard text", error)),
        };
        let bytes = read_to_end_within(pipe, READ_TIMEOUT)?;
        // The contract maps empty text to `None`, the same as no text.
        Ok(String::from_utf8(bytes)
            .ok()
            .filter(|text| !text.is_empty()))
    }

    fn write_text(&self, text: &str) -> Result<(), ClipboardError> {
        // Not `foreground`: the library serves the content from its own thread (see module docs).
        // It also offers `text/plain`, `UTF8_STRING`, `STRING` and `TEXT` with the same bytes.
        copy::Options::new()
            .copy(
                copy::Source::Bytes(text.as_bytes().into()),
                copy::MimeType::Specific(TEXT_UTF8.to_owned()),
            )
            .map_err(copy_error)
    }

    fn watch(&self) -> Result<ClipboardWatch, ClipboardError> {
        let watcher = Watcher::new(watch::ClipboardType::Regular, paste::Seat::Unspecified)
            .map_err(|error| paste_error("starting the clipboard watcher", error))?;
        let cancel = watcher.cancel_handle();
        let (changes, clipboard_watch) = ClipboardWatch::channel();
        let consumer = changes.clone();
        // Disconnects when the watcher thread ends, which also ends the stop checker.
        let (watcher_alive, watcher_done) = mpsc::channel::<()>();

        // The watcher thread blocks until the next selection change, so on its own it would only
        // notice a dropped watch then. This thread wakes it up as soon as the consumer is gone.
        thread::Builder::new()
            .name("wl-watch-stop".to_owned())
            .spawn(move || {
                while let Err(RecvTimeoutError::Timeout) =
                    watcher_done.recv_timeout(STOP_CHECK_INTERVAL)
                {
                    if consumer.is_closed() {
                        cancel.cancel();
                        return;
                    }
                }
            })
            .map_err(|error| ClipboardError::backend("spawning the watcher stop thread", error))?;

        // If this spawn fails, the closure (and `watcher_alive` in it) is dropped, which ends the
        // stop checker too.
        thread::Builder::new()
            .name("wl-watch".to_owned())
            .spawn(move || {
                let _alive = watcher_alive;
                forward_events(watcher, changes);
            })
            .map_err(|error| ClipboardError::backend("spawning the watcher thread", error))?;

        Ok(clipboard_watch)
    }
}

/// Runs on the watcher thread: forwards selection changes until cancelled or the consumer is gone.
fn forward_events(mut watcher: Watcher, changes: ChangeSender) {
    // `wl_clipboard_rs` reports the selection that existed before watching started as the first
    // event; the contract says not to report it.
    let mut initial = true;
    loop {
        let formats = match watcher.next_event() {
            Ok(Some(ClipboardEvent::Changed { mime_types, .. })) => mime_types,
            // Cleared, or the app that owned the selection exited.
            Ok(Some(ClipboardEvent::Cleared { .. })) => Vec::new(),
            // Cancelled by the stop checker.
            Ok(None) => return,
            Err(error) => {
                changes.fail(paste_error("watching the clipboard", error));
                return;
            }
        };
        if std::mem::take(&mut initial) {
            continue;
        }
        if changes.send(ClipboardChange::new(formats)).is_err() {
            return;
        }
    }
}

/// Reads `pipe` to the end, giving up once `timeout` has passed. A frozen source app would
/// otherwise block the read forever.
fn read_to_end_within(
    mut pipe: impl Read + AsFd,
    timeout: Duration,
) -> Result<Vec<u8>, ClipboardError> {
    let deadline = Instant::now() + timeout;
    let mut data = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ClipboardError::Backend {
                message: format!(
                    "the app that owns the clipboard did not send its content within {}s",
                    timeout.as_secs()
                ),
                source: None,
            });
        }
        let remaining = Timespec::try_from(remaining)
            .map_err(|error| ClipboardError::backend("computing the read timeout", error))?;
        match rustix::event::poll(&mut [PollFd::new(&pipe, PollFlags::IN)], Some(&remaining)) {
            // Timed out or interrupted: back to the deadline check.
            Ok(0) | Err(rustix::io::Errno::INTR) => continue,
            // Readable or hung up: `read` won't block.
            Ok(_) => {}
            Err(error) => {
                return Err(ClipboardError::backend(
                    "waiting for clipboard data",
                    io::Error::from(error),
                ));
            }
        }
        match pipe.read(&mut chunk) {
            Ok(0) => return Ok(data),
            Ok(read) => data.extend_from_slice(&chunk[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(ClipboardError::backend("reading clipboard data", error)),
        }
    }
}

fn paste_error(context: &str, error: paste::Error) -> ClipboardError {
    match error {
        paste::Error::NoSeats
        | paste::Error::SocketOpenError(_)
        | paste::Error::WaylandConnection(_)
        | paste::Error::MissingProtocol { .. } => unavailable(&error),
        error => ClipboardError::backend(context, error),
    }
}

fn copy_error(error: copy::Error) -> ClipboardError {
    match error {
        copy::Error::NoSeats
        | copy::Error::SocketOpenError(_)
        | copy::Error::WaylandConnection(_)
        | copy::Error::MissingProtocol { .. } => unavailable(&error),
        error => ClipboardError::backend("setting the clipboard", error),
    }
}

/// [`ClipboardError::Unavailable`] with the whole error chain as the reason, e.g. "Couldn't connect
/// to the Wayland compositor: ...".
fn unavailable(error: &dyn Error) -> ClipboardError {
    let mut reason = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        reason.push_str(": ");
        reason.push_str(&cause.to_string());
        source = cause.source();
    }
    ClipboardError::Unavailable { reason }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_everything_the_writer_sends() {
        let (reader, mut writer) = io::pipe().expect("pipe");
        let payload = "x".repeat(20_000);
        let sent = payload.clone();
        let writer = thread::spawn(move || io::Write::write_all(&mut writer, sent.as_bytes()));

        let data = read_to_end_within(reader, READ_TIMEOUT).expect("read");

        writer.join().expect("writer thread").expect("write");
        assert_eq!(data, payload.as_bytes());
    }

    #[test]
    fn gives_up_when_the_writer_stalls() {
        // Keeping the write end open without writing is what a frozen source app looks like.
        let (reader, _writer) = io::pipe().expect("pipe");
        let started = Instant::now();

        let result = read_to_end_within(reader, Duration::from_millis(100));

        assert!(matches!(result, Err(ClipboardError::Backend { .. })));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn unavailable_reason_includes_the_cause() {
        let error = unavailable(&paste::Error::SocketOpenError(io::Error::other("boom")));
        assert!(
            matches!(&error, ClipboardError::Unavailable { reason } if reason.ends_with(": boom")),
            "{error}"
        );
    }
}
