//! [`Clipboard`] backend for macOS, on the general `NSPasteboard`.
//!
//! Built on the [`objc2_app_kit`] bindings. `NSPasteboard` is not tied to the main thread (the
//! bindings do not require a `MainThreadMarker` for it), so every call fetches the general
//! pasteboard on whatever thread it runs on. Each call runs inside its own autorelease pool,
//! because the threads here have no run loop that would drain one.
//!
//! # Threads
//!
//! - [`read_text`](Clipboard::read_text), [`read_image`](Clipboard::read_image) and
//!   [`read_files`](Clipboard::read_files): run on a short-lived thread so the caller can give up
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
//! that, the watcher only reads metadata (`changeCount` and the offered `types`). The content
//! itself is read only in [`read_text`](Clipboard::read_text),
//! [`read_image`](Clipboard::read_image) and [`read_files`](Clipboard::read_files), and only after
//! checking that the pasteboard offers that kind of content. [`MacosClipboard::access_behavior`]
//! reports what the user allowed.
//!
//! # Sensitive content
//!
//! `NSPasteboard` has no way to list the types and read the content in one step. So
//! [`read_text`](Clipboard::read_text), [`read_image`](Clipboard::read_image) and
//! [`read_files`](Clipboard::read_files) read `changeCount`, list the types, check them for
//! sensitive markers, read the content, and then check `changeCount` and the types again. If the
//! count moved, another copy happened in between: it starts over, up to [`READ_ATTEMPTS`] times,
//! and then gives up with an error. The second look at the types catches an app that adds its
//! marker type after the content within the same copy (writing types one by one does not bump
//! `changeCount`). An app that adds the marker even later, after that second look, is not caught;
//! password managers that follow nspasteboard.org declare all types before writing any data.
//!
//! # Images
//!
//! [`write_image`](Clipboard::write_image) offers the PNG as `public.png`, byte for byte.
//! [`read_image`](Clipboard::read_image) prefers `public.png` and returns its bytes as they are,
//! so our own write reads back with the same hash (anti-echo needs it). If only `public.tiff` is
//! offered (some apps, e.g. Preview), `AppKit` converts it to PNG.
//!
//! [`read_text`](Clipboard::read_text) only reads `public.utf8-plain-text`, so markup such as
//! `public.html` next to an image never counts as text.
//!
//! # Files
//!
//! Finder puts one pasteboard item per copied file, each with a `public.file-url`.
//! [`read_files`](Clipboard::read_files) turns them into paths with `NSURL`, which also resolves
//! file reference URLs (`file:///.file/id=...`) to the file's current path. Only `file:` URLs with
//! no host or `localhost` count; if any item has no such URL (a copied web link has only
//! `public.url`), the copy is not files and stays text. [`write_files`](Clipboard::write_files)
//! writes one `NSURL` per path, the way Finder pastes them.

use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use clipbroad_core::Png;
use clipbroad_core::sensitive::sensitive_marker;
use objc2::available;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSPasteboard, NSPasteboardAccessBehavior,
    NSPasteboardWriting,
};
use objc2_foundation::{NSArray, NSData, NSDictionary, NSInteger, NSString, NSURL, ns_string};

use crate::{ChangeSender, Clipboard, ClipboardChange, ClipboardError, ClipboardWatch};

/// The text type we read and write: `NSPasteboardTypeString`.
const TEXT_UTF8: &str = "public.utf8-plain-text";

/// The image type we read first and write: `NSPasteboardTypePNG`.
const IMAGE_PNG: &str = "public.png";

/// The image type read when there is no PNG, converted to PNG: `NSPasteboardTypeTIFF`.
const IMAGE_TIFF: &str = "public.tiff";

/// A file URL, one per pasteboard item: `NSPasteboardTypeFileURL`.
const FILE_URL: &str = "public.file-url";

/// Longest wait for the app that owns the pasteboard to hand over its content.
const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// How many times a read starts over because another copy happened during it.
const READ_ATTEMPTS: usize = 3;

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
        read_with_timeout(read_string_once)
    }

    fn write_text(&self, text: &str) -> Result<(), ClipboardError> {
        replace_contents("text", |pasteboard| {
            pasteboard.setString_forType(&NSString::from_str(text), ns_string!(TEXT_UTF8))
        })
    }

    fn read_image(&self) -> Result<Option<Png>, ClipboardError> {
        read_with_timeout(read_png_once)
    }

    fn write_image(&self, png: &Png) -> Result<(), ClipboardError> {
        replace_contents("image", |pasteboard| {
            let data = NSData::with_bytes(png.as_bytes());
            pasteboard.setData_forType(Some(&data), ns_string!(IMAGE_PNG))
        })
    }

    fn read_files(&self) -> Result<Option<Vec<PathBuf>>, ClipboardError> {
        read_with_timeout(read_files_once)
    }

    fn write_files(&self, paths: &[PathBuf]) -> Result<(), ClipboardError> {
        // Checked before `replace_contents` clears the pasteboard.
        let paths = paths
            .iter()
            .map(|path| path.to_str().filter(|_| path.is_absolute()))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| ClipboardError::Backend {
                message: "a file to put on the clipboard has a relative or non-UTF-8 path"
                    .to_owned(),
                source: None,
            })?;
        replace_contents("files", |pasteboard| {
            let urls: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = paths
                .iter()
                .map(|path| {
                    ProtocolObject::from_retained(NSURL::fileURLWithPath(&NSString::from_str(path)))
                })
                .collect();
            pasteboard.writeObjects(&NSArray::from_retained_slice(&urls))
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

/// Runs `read_checked(read_once)` on a short-lived thread and waits at most [`READ_TIMEOUT`] for
/// its result.
fn read_with_timeout<T: Send + 'static>(
    read_once: fn(&NSPasteboard) -> Result<Option<T>, ClipboardError>,
) -> Result<Option<T>, ClipboardError> {
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("mac-read".to_owned())
        .spawn(move || {
            // After a timeout nobody is waiting for the result anymore; dropping it is fine.
            let _ = result_tx.send(read_checked(read_once));
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

/// Replaces the pasteboard content with what `set` writes (`what` names it in the error).
fn replace_contents(
    what: &str,
    set: impl FnOnce(&NSPasteboard) -> bool,
) -> Result<(), ClipboardError> {
    autoreleasepool(|_| {
        let pasteboard = NSPasteboard::generalPasteboard();
        // Taking ownership of the pasteboard is required before writing to it.
        pasteboard.clearContents();
        if set(&pasteboard) {
            Ok(())
        } else {
            Err(ClipboardError::Backend {
                message: format!("macOS refused to set the pasteboard {what}"),
                source: None,
            })
        }
    })
}

/// Runs on a read thread. Reads one pasteboard content with `read_once`, checking it for sensitive
/// markers before and after (see module docs).
fn read_checked<T>(
    read_once: fn(&NSPasteboard) -> Result<Option<T>, ClipboardError>,
) -> Result<Option<T>, ClipboardError> {
    autoreleasepool(|_| {
        let pasteboard = NSPasteboard::generalPasteboard();
        for _ in 0..READ_ATTEMPTS {
            let count = pasteboard.changeCount();
            check_not_sensitive(&pasteboard)?;
            let content = read_once(&pasteboard)?;
            check_not_sensitive(&pasteboard)?;
            if pasteboard.changeCount() == count {
                return Ok(content);
            }
        }
        Err(ClipboardError::Backend {
            message: format!(
                "the clipboard changed during each of {READ_ATTEMPTS} attempts to read it"
            ),
            source: None,
        })
    })
}

/// Fails if the pasteboard's types hold a sensitive marker or cannot be listed.
fn check_not_sensitive(pasteboard: &NSPasteboard) -> Result<(), ClipboardError> {
    let Some(formats) = type_names(pasteboard) else {
        return Err(ClipboardError::Backend {
            message: "could not list the pasteboard types to check for sensitive content"
                .to_owned(),
            source: None,
        });
    };
    match sensitive_marker(&formats) {
        Some(marker) => Err(ClipboardError::Sensitive { marker }),
        None => Ok(()),
    }
}

/// Requests the text only if the pasteboard offers it, so an image or a file copy is never read.
fn read_string_once(pasteboard: &NSPasteboard) -> Result<Option<String>, ClipboardError> {
    let text_type = ns_string!(TEXT_UTF8);
    if pasteboard
        .availableTypeFromArray(&NSArray::from_slice(&[text_type]))
        .is_none()
    {
        return Ok(None);
    }
    match pasteboard.stringForType(text_type) {
        Some(text) => Ok(Some(text.to_string())),
        // The pasteboard changed since the check, the text could not be converted, or access is
        // denied.
        None => nothing_read(),
    }
}

/// Requests an image only if the pasteboard offers PNG or TIFF (see module docs).
#[allow(unsafe_code)] // `representationUsingType:properties:`; see the SAFETY comment.
fn read_png_once(pasteboard: &NSPasteboard) -> Result<Option<Png>, ClipboardError> {
    let png_type = ns_string!(IMAGE_PNG);
    // The first of these that is offered, in this order.
    let Some(image_type) = pasteboard
        .availableTypeFromArray(&NSArray::from_slice(&[png_type, ns_string!(IMAGE_TIFF)]))
    else {
        return Ok(None);
    };
    let Some(data) = pasteboard.dataForType(&image_type) else {
        return nothing_read();
    };
    let bytes = if image_type.isEqualToString(png_type) {
        data.to_vec()
    } else {
        let Some(bitmap) = NSBitmapImageRep::imageRepWithData(&data) else {
            tracing::debug!("the pasteboard TIFF could not be decoded; treating it as no image");
            return Ok(None);
        };
        // SAFETY: an empty dictionary holds no value of the wrong type.
        let png = unsafe {
            bitmap.representationUsingType_properties(
                NSBitmapImageFileType::PNG,
                &NSDictionary::new(),
            )
        };
        let Some(png) = png else {
            return Err(ClipboardError::Backend {
                message: "macOS could not convert the pasteboard TIFF to PNG".to_owned(),
                source: None,
            });
        };
        png.to_vec()
    };
    Png::try_from(bytes)
        .map(Some)
        .map_err(|error| ClipboardError::backend("reading the pasteboard image", error))
}

/// Reads the file URL of every pasteboard item, only if the pasteboard offers one (see module
/// docs).
fn read_files_once(pasteboard: &NSPasteboard) -> Result<Option<Vec<PathBuf>>, ClipboardError> {
    let file_url = ns_string!(FILE_URL);
    if pasteboard
        .availableTypeFromArray(&NSArray::from_slice(&[file_url]))
        .is_none()
    {
        return Ok(None);
    }
    let Some(items) = pasteboard.pasteboardItems() else {
        return nothing_read();
    };
    let mut paths = Vec::new();
    for item in &items {
        // An item without a file URL (or one whose data cannot be read): not a file copy.
        let Some(url) = item.stringForType(file_url) else {
            return nothing_read();
        };
        let Some(path) = local_path(&url) else {
            return Ok(None);
        };
        paths.push(path);
    }
    Ok((!paths.is_empty()).then_some(paths))
}

/// The path of a local `file:` URL, resolving a file reference URL; `None` for any other URL or a
/// reference to a file that no longer exists.
fn local_path(url: &NSString) -> Option<PathBuf> {
    let url = NSURL::URLWithString(url)?;
    let local = url.isFileURL()
        && url.host().is_none_or(|host| {
            host.is_empty() || host.to_string().eq_ignore_ascii_case("localhost")
        });
    if !local {
        return None;
    }
    let path = url.filePathURL()?.path()?;
    // The file system hands back decomposed Unicode (NFD: "n" + combining tilde for "ñ"). Other
    // OSes use the composed form (NFC), and the files hash covers the names, so a name that
    // changed form would make an applied files clip look like a new copy. APFS and HFS+ open a
    // file by either form.
    Some(PathBuf::from(
        path.precomposedStringWithCanonicalMapping().to_string(),
    ))
}

/// The result when the pasteboard offered a type but handed over no data: an error if the user
/// denied access, otherwise nothing.
fn nothing_read<T>() -> Result<Option<T>, ClipboardError> {
    if MacosClipboard.access_behavior() == Some(AccessBehavior::AlwaysDeny) {
        return Err(ClipboardError::Unavailable {
            reason: "macOS denies clipboard access to this app; allow it in System Settings > \
                     Privacy & Security > Paste from Other Apps"
                .to_owned(),
        });
    }
    Ok(None)
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
    type_names(pasteboard).unwrap_or_else(|| {
        tracing::warn!("could not list the pasteboard types; reporting the change with no formats");
        Vec::new()
    })
}

/// The types offered by all pasteboard items, or `None` if macOS cannot list them.
fn type_names(pasteboard: &NSPasteboard) -> Option<Vec<String>> {
    let types = pasteboard.types()?;
    Some(types.iter().map(|format| format.to_string()).collect())
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
