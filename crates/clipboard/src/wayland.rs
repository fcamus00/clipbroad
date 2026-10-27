//! [`Clipboard`] backend for Wayland compositors with a data-control protocol
//! (`ext-data-control-v1` or `wlr-data-control-unstable-v1`), such as Hyprland, Sway or KDE.
//!
//! Built on [`wl_clipboard_rs`]. Every call opens its own short-lived compositor connection, so
//! the type holds no state and is trivially `Send + Sync`.
//!
//! # Threads
//!
//! - [`write_text`](Clipboard::write_text), [`write_image`](Clipboard::write_image) and
//!   [`write_files`](Clipboard::write_files): on Wayland the app that copied must keep serving the content to whoever pastes.
//!   `wl_clipboard_rs` does this on a background thread that lives until another copy replaces
//!   ours (the compositor then cancels our offer and the thread ends) or the process exits. So
//!   there is at most one such thread per process in steady state, and the clipboard empties when
//!   the process exits (unlike `wl-copy`, which forks a process that outlives the shell command).
//! - [`watch`](Clipboard::watch): one thread blocked on the compositor connection, plus one that
//!   checks every [`STOP_CHECK_INTERVAL`] whether the [`ClipboardWatch`] was dropped and, if so,
//!   wakes the first one up. Both end shortly after the watch is dropped, even if the clipboard
//!   never changes again.
//!
//! # Sensitive content
//!
//! [`read_text`](Clipboard::read_text), [`read_image`](Clipboard::read_image) and
//! [`read_files`](Clipboard::read_files) ask the compositor for the current selection and get one data offer with its fixed list of MIME types. They check
//! that list for sensitive markers and, if there are none, read the content from **that same
//! offer**. A newer copy brings a new offer, so the checked list and the content always belong to
//! the same copy.
//!
//! # Images
//!
//! Only `image/png` is read and written, byte for byte, so an image we wrote reads back with the
//! same hash. A copy that offers only other image types (`image/jpeg`, `image/bmp`, ...) counts as
//! no image: converting would need an image codec. Screenshot tools and browsers offer
//! `image/png`.
//!
//! When an image comes with `text/html` but no plain text (a browser's "Copy image"),
//! [`read_text`](Clipboard::read_text) returns `None`, so the image is what gets synced and not
//! the `<img>` markup.
//!
//! # Files
//!
//! Copied files are read from `text/uri-list` (RFC 2483), which file managers and
//! `wl-copy --type text/uri-list` offer. Only `file://` URIs with an empty host, `localhost` or
//! this machine's host name count, percent-decoded into paths. One URI of any other kind (a copied
//! web link) means the copy is not files, so it stays text. Lines starting with `#` are comments.
//!
//! Writing offers three types for the same paths: `text/uri-list`, `x-special/gnome-copied-files`
//! (`copy` and then one URI per line; what GNOME Files pastes from) and the plain paths, one per
//! line, as text, so pasting into a terminal or an editor gives the paths, as with a copy from
//! GNOME Files. Reading back prefers `text/uri-list`, so the daemon finds the same paths.
//!
//! # Known compositor gaps
//!
//! Hyprland (checked on 0.56.2) sends no data-control event when the clipboard is cleared or when
//! the app that owned it exits, so no empty-formats change is reported there. The backend reports
//! one whenever the compositor does send it.

use std::error::Error;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{self, Read};
use std::os::fd::AsFd;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use clipbroad_core::Png;
use clipbroad_core::sensitive::sensitive_marker;
use rustix::event::{PollFd, PollFlags, Timespec};
use wl_clipboard_rs::watch::{ClipboardEvent, Offer, Watcher};
use wl_clipboard_rs::{copy, paste, watch};

use crate::{ChangeSender, Clipboard, ClipboardChange, ClipboardError, ClipboardWatch};

/// The MIME type we read first and write as the main offer.
const TEXT_UTF8: &str = "text/plain;charset=utf-8";

/// Plain-text MIME types, in the order `read_text` prefers them when an image is also offered.
const PLAIN_TEXT: [&str; 5] = [TEXT_UTF8, "UTF8_STRING", "text/plain", "STRING", "TEXT"];

/// The only image type read and written.
const IMAGE_PNG: &str = "image/png";

/// Copied files, as `file://` URIs (RFC 2483).
const URI_LIST: &str = "text/uri-list";

/// What GNOME Files pastes from: `copy` (or `cut`), then one URI per line.
const GNOME_COPIED_FILES: &str = "x-special/gnome-copied-files";

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
        read_current(|mime_types, offer| {
            let mime_type = if mime_types.iter().any(|offered| offered == IMAGE_PNG) {
                // Markup next to an image describes the image (a browser's "Copy image" offers
                // `text/html` with an `<img>` tag), so only plain text counts as text then.
                PLAIN_TEXT
                    .into_iter()
                    .find(|plain| mime_types.iter().any(|offered| offered == plain))
                    .map(str::to_owned)
            } else {
                // `MimeType::Text` prefers `text/plain;charset=utf-8`, then `UTF8_STRING`, then
                // any other text type.
                paste::select_mime_type(mime_types, paste::MimeType::Text)
            };
            let Some(mime_type) = mime_type else {
                return Ok(None);
            };
            let bytes = receive(offer, &mime_type)?;
            Ok(String::from_utf8(bytes).ok())
        })
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

    fn read_image(&self) -> Result<Option<Png>, ClipboardError> {
        read_current(|mime_types, offer| {
            // Other image types (`image/jpeg`, `image/bmp`, ...) would need transcoding; see
            // module docs.
            if !mime_types.iter().any(|offered| offered == IMAGE_PNG) {
                return Ok(None);
            }
            let bytes = receive(offer, IMAGE_PNG)?;
            Png::try_from(bytes)
                .map(Some)
                .map_err(|error| ClipboardError::backend("reading the image/png offer", error))
        })
    }

    fn write_image(&self, png: &Png) -> Result<(), ClipboardError> {
        // Served as is, so a read-back returns the same bytes (anti-echo depends on it). Only
        // `image/png` is offered: unlike text, the library adds no other types.
        copy::Options::new()
            .copy(
                copy::Source::Bytes(png.as_bytes().into()),
                copy::MimeType::Specific(IMAGE_PNG.to_owned()),
            )
            .map_err(copy_error)
    }

    fn read_files(&self) -> Result<Option<Vec<PathBuf>>, ClipboardError> {
        read_current(|mime_types, offer| {
            if !mime_types.iter().any(|offered| offered == URI_LIST) {
                return Ok(None);
            }
            let list = receive(offer, URI_LIST)?;
            let host = rustix::system::uname();
            Ok(parse_uri_list(&list, host.nodename().to_bytes()))
        })
    }

    fn write_files(&self, paths: &[PathBuf]) -> Result<(), ClipboardError> {
        let uris = paths
            .iter()
            .map(|path| file_uri(path))
            .collect::<Result<Vec<_>, _>>()?;
        let uri_list = format!("{}\r\n", uris.join("\r\n"));
        let gnome = format!("copy\n{}", uris.join("\n"));
        let text = paths
            .iter()
            .map(|path| path.to_string_lossy())
            .collect::<Vec<_>>()
            .join("\n");
        let source = |bytes: String, mime: &str| copy::MimeSource {
            source: copy::Source::Bytes(bytes.into_bytes().into()),
            mime_type: copy::MimeType::Specific(mime.to_owned()),
        };
        // The plain text goes first: the library also offers the first text type it gets as
        // `text/plain`, `UTF8_STRING`, `STRING` and `TEXT`, and `text/uri-list` counts as text.
        copy::Options::new()
            .copy_multi(vec![
                source(text, TEXT_UTF8),
                source(uri_list, URI_LIST),
                source(gnome, GNOME_COPIED_FILES),
            ])
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

/// Gets the current selection and hands its MIME types and offer to `read`, unless the selection
/// is empty or marked as sensitive. Both come from the same offer (see module docs).
fn read_current<T>(
    read: impl FnOnce(Vec<String>, &mut Offer<'_>) -> Result<Option<T>, ClipboardError>,
) -> Result<Option<T>, ClipboardError> {
    // A watcher, unlike `paste::get_contents`, hands out the offer together with its MIME types.
    // It reports the current selection first; cancelling up front makes `next_event` return that
    // queued event right away, or `None` instead of blocking if the compositor sent none.
    let mut watcher = Watcher::new(watch::ClipboardType::Regular, paste::Seat::Unspecified)
        .map_err(|error| paste_error("requesting the clipboard", error))?;
    watcher.cancel_handle().cancel();
    let (mime_types, mut offer) = match watcher.next_event() {
        Ok(Some(ClipboardEvent::Changed {
            mime_types, offer, ..
        })) => (mime_types, offer),
        Ok(Some(ClipboardEvent::Cleared { .. })) => return Ok(None),
        Ok(None) => {
            return Err(ClipboardError::Backend {
                message: "the compositor did not report the current clipboard".to_owned(),
                source: None,
            });
        }
        Err(error) => return Err(paste_error("requesting the clipboard", error)),
    };
    if let Some(marker) = sensitive_marker(&mime_types) {
        return Err(ClipboardError::Sensitive { marker });
    }
    // `offer` borrows `watcher`, which keeps the compositor connection open until `read` ends.
    read(mime_types, &mut offer)
}

/// Receives the content of `offer` as `mime_type`, within [`READ_TIMEOUT`].
fn receive(offer: &mut Offer<'_>, mime_type: &str) -> Result<Vec<u8>, ClipboardError> {
    let pipe = offer
        .receive(mime_type)
        .map_err(|error| paste_error(&format!("requesting the {mime_type} offer"), error))?;
    read_to_end_within(pipe, READ_TIMEOUT)
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

/// The local paths in a `text/uri-list`, or `None` if it holds no URI or any URI that is not a
/// local file (see module docs). `host` is this machine's host name.
fn parse_uri_list(list: &[u8], host: &[u8]) -> Option<Vec<PathBuf>> {
    let mut paths = Vec::new();
    // RFC 2483 separates lines with CRLF; some apps write a plain LF.
    for line in list.split(|&byte| byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() || line.starts_with(b"#") {
            continue;
        }
        paths.push(file_path(line, host)?);
    }
    (!paths.is_empty()).then_some(paths)
}

/// The path of a `file://` URI on this machine, or `None` for any other URI.
fn file_path(uri: &[u8], host: &[u8]) -> Option<PathBuf> {
    let (scheme, rest) = uri.split_at_checked(7)?;
    if !scheme.eq_ignore_ascii_case(b"file://") {
        return None;
    }
    let (uri_host, encoded) = rest.split_at(rest.iter().position(|&byte| byte == b'/')?);
    let local = uri_host.is_empty()
        || uri_host.eq_ignore_ascii_case(b"localhost")
        || uri_host.eq_ignore_ascii_case(host);
    // A query or fragment is not part of a file URI.
    if !local || encoded.iter().any(|&byte| byte == b'?' || byte == b'#') {
        return None;
    }
    let path = percent_decode(encoded)?;
    // No path can hold a NUL.
    if path.contains(&0) {
        return None;
    }
    Some(PathBuf::from(OsString::from_vec(path)))
}

/// `%XX` escapes decoded into bytes, or `None` if an escape is malformed.
fn percent_decode(encoded: &[u8]) -> Option<Vec<u8>> {
    let mut decoded = Vec::with_capacity(encoded.len());
    let mut bytes = encoded.iter();
    while let Some(&byte) = bytes.next() {
        if byte == b'%' {
            let hex = [*bytes.next()?, *bytes.next()?];
            decoded.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            decoded.push(byte);
        }
    }
    Some(decoded)
}

/// `file://` and the absolute `path`, with every byte outside RFC 3986's unreserved characters
/// and `/` percent-encoded (spaces, non-ASCII, `%`, `#`, ...).
fn file_uri(path: &Path) -> Result<String, ClipboardError> {
    if !path.is_absolute() {
        return Err(ClipboardError::Backend {
            message: "a file to put on the clipboard has a relative path".to_owned(),
            source: None,
        });
    }
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            // Writing to a String never fails.
            let _ = write!(uri, "%{byte:02X}");
        }
    }
    Ok(uri)
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

    fn paths(list: &str) -> Option<Vec<PathBuf>> {
        parse_uri_list(list.as_bytes(), b"devbox")
    }

    #[test]
    fn local_file_uris_are_files() {
        let list = "# copied\r\nfile:///tmp/a%20b/%C3%B1and%C3%BA.txt\r\nfile://localhost/x\r\n\
                    FILE://devbox/y\nfile:///z%2525";
        let expected = ["/tmp/a b/ñandú.txt", "/x", "/y", "/z%25"].map(PathBuf::from);
        assert_eq!(paths(list), Some(expected.to_vec()));
    }

    #[test]
    fn any_other_uri_means_no_files() {
        for list in [
            "https://example.com/file.txt",
            "file:///tmp/a\r\nhttps://example.com/",
            "file://otherhost/tmp/a",
            "file:///tmp/a%2",
            "file:///tmp/a%zz",
            "file:///tmp/a%00b",
            "file:///tmp/a?query",
            "file:relative",
            "/tmp/a",
            "",
            "# only a comment\r\n",
        ] {
            assert_eq!(paths(list), None, "{list:?}");
        }
    }

    #[test]
    fn written_uris_read_back_as_the_same_paths() {
        let written = [
            PathBuf::from("/tmp/a b/ñandú 🦀.txt"),
            PathBuf::from("/tmp/100% #1?.txt"),
            PathBuf::from(OsString::from_vec(b"/tmp/not utf-8 \xff".to_vec())),
        ];
        let uris: Vec<String> = written
            .iter()
            .map(|path| file_uri(path).expect("absolute"))
            .collect();
        assert_eq!(
            uris[0],
            "file:///tmp/a%20b/%C3%B1and%C3%BA%20%F0%9F%A6%80.txt"
        );
        assert_eq!(paths(&uris.join("\r\n")), Some(written.to_vec()));
        assert!(file_uri(Path::new("relative")).is_err());
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
