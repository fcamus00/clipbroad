//! [`Clipboard`] backend for Windows, on the Win32 clipboard API.
//!
//! Built directly on the [`windows_sys`] bindings: higher-level crates (`arboard`,
//! `clipboard-master`) do not expose the list of offered formats, which the daemon needs to spot
//! sensitive-content markers. The type holds no state, so it is trivially `Send + Sync`.
//!
//! # Busy clipboard
//!
//! Only one window at a time can have the Windows clipboard open. When another app holds it, every
//! operation retries for up to [`OPEN_TIMEOUT`] and then gives up with [`ClipboardError::Busy`]
//! (reads and writes) or reports the change with no formats (the watcher).
//!
//! # Threads
//!
//! - [`read_text`](Clipboard::read_text) and [`write_text`](Clipboard::write_text): run on a
//!   short-lived thread so the caller can give up after [`OPERATION_TIMEOUT`]. Besides the busy
//!   retries, reading text that the source app renders on demand (delayed rendering) waits for that
//!   app, and a frozen app would otherwise block the caller. An operation that times out leaves its
//!   thread to finish on its own, so a timed-out write may still land later.
//! - [`watch`](Clipboard::watch): one thread with a message-only window registered with
//!   `AddClipboardFormatListener`, which receives a `WM_CLIPBOARDUPDATE` per change. A timer wakes
//!   it every [`STOP_CHECK_INTERVAL`] to check whether the [`ClipboardWatch`] was dropped; if so it
//!   unregisters and destroys the window and ends, even if the clipboard never changes again.
//!   Windows may notify several times for one copy; each notification is reported.

use std::ffi::c_void;
use std::io;
use std::marker::PhantomData;
use std::ptr;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{GlobalFree, HGLOBAL, HWND};
use windows_sys::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, EnumClipboardFormats,
    GetClipboardData, GetClipboardFormatNameW, IsClipboardFormatAvailable, OpenClipboard,
    RemoveClipboardFormatListener, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows_sys::Win32::System::Ole::{
    CF_BITMAP, CF_DIB, CF_DIBV5, CF_DIF, CF_DSPBITMAP, CF_DSPENHMETAFILE, CF_DSPMETAFILEPICT,
    CF_DSPTEXT, CF_ENHMETAFILE, CF_HDROP, CF_LOCALE, CF_METAFILEPICT, CF_OEMTEXT, CF_OWNERDISPLAY,
    CF_PALETTE, CF_PENDATA, CF_RIFF, CF_SYLK, CF_TEXT, CF_TIFF, CF_UNICODETEXT, CF_WAVE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_MESSAGE, MSG, SetTimer,
    WM_CLIPBOARDUPDATE, WM_TIMER,
};
use windows_sys::w;

use crate::{ChangeSender, Clipboard, ClipboardChange, ClipboardError, ClipboardWatch};

/// Longest a read or write may block its caller.
const OPERATION_TIMEOUT: Duration = Duration::from_secs(2);

/// How long to keep retrying while another app has the clipboard open.
const OPEN_TIMEOUT: Duration = Duration::from_secs(1);

/// Wait between two attempts to open a busy clipboard.
const OPEN_RETRY_INTERVAL: Duration = Duration::from_millis(10);

/// How often a watch checks whether its consumer is gone.
const STOP_CHECK_INTERVAL: Duration = Duration::from_millis(200);

/// Id of the watcher's stop-check timer (any non-zero value; the window has only this one).
const STOP_TIMER_ID: usize = 1;

/// Longest registered format name Windows stores, plus the terminating NUL.
const FORMAT_NAME_CAPACITY: usize = 256;

/// The Windows clipboard.
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsClipboard;

impl Clipboard for WindowsClipboard {
    fn read_text(&self) -> Result<Option<String>, ClipboardError> {
        run_with_timeout("win-read", read_unicode_text)
    }

    fn write_text(&self, text: &str) -> Result<(), ClipboardError> {
        // `CF_UNICODETEXT` is UTF-16 ending in a NUL. Windows derives `CF_TEXT`, `CF_OEMTEXT` and
        // `CF_LOCALE` from it for apps that ask for those.
        let units: Vec<u16> = text.encode_utf16().chain([0]).collect();
        run_with_timeout("win-write", move || write_unicode_text(&units))
    }

    fn watch(&self) -> Result<ClipboardWatch, ClipboardError> {
        let (changes, clipboard_watch) = ClipboardWatch::channel();
        // The thread ends on its own once the watch is dropped; nobody needs to join it.
        spawn_watcher(changes)?;
        Ok(clipboard_watch)
    }
}

/// Runs `operation` on its own thread and waits at most [`OPERATION_TIMEOUT`] for its result.
fn run_with_timeout<T: Send + 'static>(
    thread_name: &str,
    operation: impl FnOnce() -> Result<T, ClipboardError> + Send + 'static,
) -> Result<T, ClipboardError> {
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name(thread_name.to_owned())
        .spawn(move || {
            // After a timeout nobody is waiting for the result anymore; dropping it is fine.
            let _ = result_tx.send(operation());
        })
        .map_err(|error| ClipboardError::backend("spawning the clipboard thread", error))?;
    match result_rx.recv_timeout(OPERATION_TIMEOUT) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err(ClipboardError::Backend {
            message: format!(
                "the clipboard did not respond within {}s (another app is holding it or \
                 rendering its content)",
                OPERATION_TIMEOUT.as_secs()
            ),
            source: None,
        }),
        Err(RecvTimeoutError::Disconnected) => Err(ClipboardError::Backend {
            message: "the clipboard thread ended without a result".to_owned(),
            source: None,
        }),
    }
}

#[allow(unsafe_code)] // Win32 FFI.
fn read_unicode_text() -> Result<Option<String>, ClipboardError> {
    let _open = OpenedClipboard::open()?;
    // SAFETY: plain query; the clipboard is open on this thread.
    if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT.into()) } == 0 {
        return Ok(None);
    }
    // SAFETY: the clipboard is open on this thread. The handle stays owned by the clipboard and
    // valid until `_open` closes it, which happens after we are done with it.
    let handle = unsafe { GetClipboardData(CF_UNICODETEXT.into()) };
    if handle.is_null() {
        return Err(ClipboardError::backend(
            "reading CF_UNICODETEXT",
            io::Error::last_os_error(),
        ));
    }
    let locked = LockedGlobal::lock(handle)?;
    // The text ends at the first NUL; the block may be larger than the text.
    let text = locked
        .units()
        .split(|&unit| unit == 0)
        .next()
        .unwrap_or(&[]);
    // Invalid UTF-16 (a lone surrogate) cannot be converted without loss: no text.
    Ok(String::from_utf16(text).ok())
}

#[allow(unsafe_code)] // Win32 FFI.
fn write_unicode_text(units: &[u16]) -> Result<(), ClipboardError> {
    let memory = GlobalMemory::with_units(units)?;
    let _open = OpenedClipboard::open()?;
    // SAFETY: the clipboard is open on this thread.
    if unsafe { EmptyClipboard() } == 0 {
        return Err(ClipboardError::backend(
            "emptying the clipboard",
            io::Error::last_os_error(),
        ));
    }
    memory.set_as(CF_UNICODETEXT.into())
}

/// The clipboard, open on the current thread until dropped. Not `Send`: Windows ties the open
/// clipboard to the thread that opened it.
struct OpenedClipboard(PhantomData<*const ()>);

impl OpenedClipboard {
    /// Opens the clipboard, retrying for up to [`OPEN_TIMEOUT`] while another app has it open.
    #[allow(unsafe_code)] // Win32 FFI.
    fn open() -> Result<Self, ClipboardError> {
        let deadline = Instant::now() + OPEN_TIMEOUT;
        loop {
            // SAFETY: plain call. With no owner window, the clipboard is associated with this
            // thread until `CloseClipboard`.
            if unsafe { OpenClipboard(ptr::null_mut()) } != 0 {
                return Ok(Self(PhantomData));
            }
            if Instant::now() >= deadline {
                return Err(ClipboardError::Busy);
            }
            thread::sleep(OPEN_RETRY_INTERVAL);
        }
    }
}

impl Drop for OpenedClipboard {
    #[allow(unsafe_code)] // Win32 FFI.
    fn drop(&mut self) {
        // SAFETY: this thread opened the clipboard in `open`. Nothing useful to do on failure.
        unsafe { CloseClipboard() };
    }
}

/// A movable global memory block we own (and free on drop) until it is handed to the clipboard.
struct GlobalMemory(HGLOBAL);

impl GlobalMemory {
    /// A block holding exactly `units`.
    #[allow(unsafe_code)] // Win32 FFI.
    fn with_units(units: &[u16]) -> Result<Self, ClipboardError> {
        // SAFETY: plain allocation; a null result is handled below.
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, size_of_val(units)) };
        if handle.is_null() {
            return Err(ClipboardError::backend(
                "allocating clipboard memory",
                io::Error::last_os_error(),
            ));
        }
        let memory = Self(handle);
        let locked = LockedGlobal::lock(memory.0)?;
        // SAFETY: the block was allocated with room for exactly `units.len()` units, global
        // memory is aligned for `u16`, and a fresh allocation cannot overlap `units`.
        unsafe { ptr::copy_nonoverlapping(units.as_ptr(), locked.data.cast(), units.len()) };
        drop(locked);
        Ok(memory)
    }

    /// Puts the block on the clipboard as `format`, which takes ownership of it. The clipboard
    /// must be open on this thread and already emptied by us.
    #[allow(unsafe_code)] // Win32 FFI.
    fn set_as(self, format: u32) -> Result<(), ClipboardError> {
        // SAFETY: `self.0` is a valid, unlocked movable block, as `SetClipboardData` requires.
        if unsafe { SetClipboardData(format, self.0) }.is_null() {
            // Still ours: dropping `self` frees it.
            return Err(ClipboardError::backend(
                "setting the clipboard data",
                io::Error::last_os_error(),
            ));
        }
        // The clipboard owns the block now and frees it itself.
        std::mem::forget(self);
        Ok(())
    }
}

impl Drop for GlobalMemory {
    #[allow(unsafe_code)] // Win32 FFI.
    fn drop(&mut self) {
        // SAFETY: we own the block and nothing else references it.
        unsafe { GlobalFree(self.0) };
    }
}

/// A global memory block, locked (its contents mapped at `data`) until dropped.
struct LockedGlobal {
    handle: HGLOBAL,
    data: *mut c_void,
}

impl LockedGlobal {
    #[allow(unsafe_code)] // Win32 FFI.
    fn lock(handle: HGLOBAL) -> Result<Self, ClipboardError> {
        // SAFETY: the callers pass a valid global memory handle.
        let data = unsafe { GlobalLock(handle) };
        if data.is_null() {
            return Err(ClipboardError::backend(
                "locking clipboard memory",
                io::Error::last_os_error(),
            ));
        }
        Ok(Self { handle, data })
    }

    /// The whole block as UTF-16 units.
    #[allow(unsafe_code)] // Win32 FFI.
    fn units(&self) -> &[u16] {
        // SAFETY: the handle is valid (we locked it).
        let len = unsafe { GlobalSize(self.handle) } / size_of::<u16>();
        // SAFETY: while locked, `data` points to the block's `GlobalSize` bytes, aligned for
        // `u16`. The returned slice borrows `self`, so it cannot outlive the lock.
        unsafe { std::slice::from_raw_parts(self.data.cast(), len) }
    }
}

impl Drop for LockedGlobal {
    #[allow(unsafe_code)] // Win32 FFI.
    fn drop(&mut self) {
        // SAFETY: we locked this handle in `lock`.
        unsafe { GlobalUnlock(self.handle) };
    }
}

/// Starts the watcher thread and returns once it is listening, so a change made right after
/// `watch` returns is reported.
fn spawn_watcher(changes: ChangeSender) -> Result<JoinHandle<()>, ClipboardError> {
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let thread = thread::Builder::new()
        .name("win-watch".to_owned())
        .spawn(move || match ListenerWindow::create() {
            // The window lives until the end of this arm, i.e. until watching stops.
            Ok(_window) => {
                let _ = ready_tx.send(Ok(()));
                forward_changes(changes);
            }
            Err(error) => {
                let _ = ready_tx.send(Err(error));
            }
        })
        .map_err(|error| ClipboardError::backend("spawning the watcher thread", error))?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(thread),
        Ok(Err(error)) => Err(error),
        Err(mpsc::RecvError) => Err(ClipboardError::Backend {
            message: "the watcher thread ended while starting".to_owned(),
            source: None,
        }),
    }
}

/// A message-only window that receives `WM_CLIPBOARDUPDATE` and the stop-check `WM_TIMER`.
/// Dropping it unregisters the listener and destroys the window (which also kills the timer).
struct ListenerWindow(HWND);

impl ListenerWindow {
    #[allow(unsafe_code)] // Win32 FFI.
    fn create() -> Result<Self, ClipboardError> {
        // SAFETY: plain call with valid or null pointers. `STATIC` is a system window class, so
        // nothing has to be registered. Our messages never reach its window procedure: the loop in
        // `forward_changes` handles them without dispatching.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                w!("STATIC"),
                ptr::null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if hwnd.is_null() {
            return Err(ClipboardError::backend(
                "creating the clipboard listener window",
                io::Error::last_os_error(),
            ));
        }
        let window = Self(hwnd);
        // SAFETY: `hwnd` is a live window owned by this thread.
        if unsafe { AddClipboardFormatListener(hwnd) } == 0 {
            return Err(ClipboardError::backend(
                "registering the clipboard listener",
                io::Error::last_os_error(),
            ));
        }
        let interval = u32::try_from(STOP_CHECK_INTERVAL.as_millis())
            .expect("STOP_CHECK_INTERVAL fits in u32 milliseconds");
        // SAFETY: `hwnd` is a live window owned by this thread; no timer callback, so the timer
        // posts `WM_TIMER` to the queue.
        if unsafe { SetTimer(hwnd, STOP_TIMER_ID, interval, None) } == 0 {
            return Err(ClipboardError::backend(
                "starting the watcher timer",
                io::Error::last_os_error(),
            ));
        }
        Ok(window)
    }
}

impl Drop for ListenerWindow {
    #[allow(unsafe_code)] // Win32 FFI.
    fn drop(&mut self) {
        // SAFETY: the window is live and owned by this thread (the type is not `Send`, `HWND`
        // being a raw pointer). Unregistering a window that never registered just fails, which
        // is harmless.
        unsafe {
            RemoveClipboardFormatListener(self.0);
            DestroyWindow(self.0);
        }
    }
}

/// Runs on the watcher thread: reports clipboard changes until the consumer is gone.
#[allow(unsafe_code)] // Win32 FFI.
fn forward_changes(changes: ChangeSender) {
    let mut message = MSG::default();
    loop {
        // SAFETY: `message` is a valid out-pointer; a null window means any message of this
        // thread, which only has our window.
        match unsafe { GetMessageW(&raw mut message, ptr::null_mut(), 0, 0) } {
            // `WM_QUIT`: nothing posts it, but end cleanly if something does.
            0 => return,
            -1 => {
                changes.fail(ClipboardError::backend(
                    "waiting for clipboard notifications",
                    io::Error::last_os_error(),
                ));
                return;
            }
            _ => {}
        }
        match message.message {
            WM_CLIPBOARDUPDATE => {
                let formats = list_formats().unwrap_or_else(|error| {
                    tracing::warn!(%error, "could not list the clipboard formats; reporting the change with no formats");
                    Vec::new()
                });
                if changes.send(ClipboardChange::new(formats)).is_err() {
                    return;
                }
            }
            WM_TIMER if changes.is_closed() => return,
            WM_TIMER => {}
            // SAFETY: `message` was filled in by `GetMessageW`.
            _ => unsafe {
                DispatchMessageW(&raw const message);
            },
        }
    }
}

/// The names of the formats on the clipboard, in the order Windows reports them. Fails if the
/// list cannot be read completely, so a missing sensitive-content marker never goes unnoticed.
#[allow(unsafe_code)] // Win32 FFI.
fn list_formats() -> Result<Vec<String>, ClipboardError> {
    let _open = OpenedClipboard::open()?;
    let mut formats = Vec::new();
    let mut format = 0;
    loop {
        // SAFETY: plain query; the clipboard is open on this thread.
        format = unsafe { EnumClipboardFormats(format) };
        if format == 0 {
            break;
        }
        formats.push(format_name(format));
    }
    // 0 means either "no more formats" (the last error is then 0) or a failure.
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(0) => Ok(formats),
        _ => Err(ClipboardError::backend(
            "listing the clipboard formats",
            error,
        )),
    }
}

/// The name of a clipboard format (see [`ClipboardChange::formats`]).
#[allow(unsafe_code)] // Win32 FFI.
fn format_name(format: u32) -> String {
    if let Some(name) = u16::try_from(format).ok().and_then(standard_format_name) {
        return name.to_owned();
    }
    let mut buffer = [0_u16; FORMAT_NAME_CAPACITY];
    let capacity = i32::try_from(buffer.len()).expect("FORMAT_NAME_CAPACITY fits in i32");
    // SAFETY: `buffer` is valid for `capacity` units; Windows writes at most that many.
    let len = unsafe { GetClipboardFormatNameW(format, buffer.as_mut_ptr(), capacity) };
    match usize::try_from(len) {
        Ok(len) if len > 0 => String::from_utf16_lossy(&buffer[..len]),
        // Not registered: a private or GDI object format.
        _ => format!("#{format}"),
    }
}

/// The constant name of a predefined format, e.g. `CF_UNICODETEXT`.
fn standard_format_name(format: u16) -> Option<&'static str> {
    Some(match format {
        CF_TEXT => "CF_TEXT",
        CF_BITMAP => "CF_BITMAP",
        CF_METAFILEPICT => "CF_METAFILEPICT",
        CF_SYLK => "CF_SYLK",
        CF_DIF => "CF_DIF",
        CF_TIFF => "CF_TIFF",
        CF_OEMTEXT => "CF_OEMTEXT",
        CF_DIB => "CF_DIB",
        CF_PALETTE => "CF_PALETTE",
        CF_PENDATA => "CF_PENDATA",
        CF_RIFF => "CF_RIFF",
        CF_WAVE => "CF_WAVE",
        CF_UNICODETEXT => "CF_UNICODETEXT",
        CF_ENHMETAFILE => "CF_ENHMETAFILE",
        CF_HDROP => "CF_HDROP",
        CF_LOCALE => "CF_LOCALE",
        CF_DIBV5 => "CF_DIBV5",
        CF_OWNERDISPLAY => "CF_OWNERDISPLAY",
        CF_DSPTEXT => "CF_DSPTEXT",
        CF_DSPBITMAP => "CF_DSPBITMAP",
        CF_DSPMETAFILEPICT => "CF_DSPMETAFILEPICT",
        CF_DSPENHMETAFILE => "CF_DSPENHMETAFILE",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use windows_sys::Win32::System::DataExchange::RegisterClipboardFormatW;

    use super::*;

    #[test]
    fn names_predefined_formats_by_their_constant() {
        assert_eq!(format_name(CF_UNICODETEXT.into()), "CF_UNICODETEXT");
        assert_eq!(format_name(CF_HDROP.into()), "CF_HDROP");
        // Private formats have no name.
        assert_eq!(format_name(0x200), "#512");
    }

    /// Writes `text` plus the marker format a password manager would add, the way such an app
    /// does it.
    #[allow(unsafe_code)] // Win32 FFI.
    fn write_with_exclusion_marker(text: &str) {
        // SAFETY: plain call with a valid NUL-terminated string.
        let marker =
            unsafe { RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing")) };
        assert_ne!(marker, 0, "{}", io::Error::last_os_error());
        let units: Vec<u16> = text.encode_utf16().chain([0]).collect();
        let text_memory = GlobalMemory::with_units(&units).expect("allocate text");
        // The marker's content is irrelevant, only its presence; a DWORD 0 is customary.
        let flag = GlobalMemory::with_units(&[0, 0]).expect("allocate marker");
        let _open = OpenedClipboard::open().expect("open clipboard");
        // SAFETY: the clipboard is open on this thread.
        assert_ne!(unsafe { EmptyClipboard() }, 0);
        text_memory.set_as(CF_UNICODETEXT.into()).expect("set text");
        flag.set_as(marker).expect("set marker");
    }

    /// Smoke test against the real clipboard. GitHub's Windows runners allow clipboard access,
    /// so CI runs it. It overwrites the clipboard. One test on purpose: the clipboard is global,
    /// and nextest runs tests in parallel processes that would see each other's changes.
    #[tokio::test]
    async fn real_clipboard_round_trip_and_watch() {
        async fn next_change(watch: &mut ClipboardWatch) -> ClipboardChange {
            tokio::time::timeout(Duration::from_secs(2), watch.recv())
                .await
                .expect("no change event in time")
                .expect("watch ended")
                .expect("watch failed")
        }
        /// Skips the extra notifications Windows may send for one copy, until one with `format`.
        async fn change_with(watch: &mut ClipboardWatch, format: &str) -> ClipboardChange {
            loop {
                let change = next_change(watch).await;
                if change.has_format(format) {
                    return change;
                }
            }
        }

        let clipboard = WindowsClipboard;
        clipboard
            .write_text("before watching")
            .expect("write before watching");
        let (changes, mut watch) = ClipboardWatch::channel();
        let watcher = spawn_watcher(changes).expect("start watching");

        let early = tokio::time::timeout(Duration::from_millis(500), watch.recv()).await;
        assert!(
            early.is_err(),
            "content present before watching was reported: {early:?}"
        );

        let text = "línea 1\r\nlínea 2 \u{1F600}\n";
        clipboard.write_text(text).expect("write");
        let change = change_with(&mut watch, "CF_UNICODETEXT").await;
        assert!(change.has_format("CF_UNICODETEXT"), "{change:?}");
        assert_eq!(clipboard.read_text().expect("read").as_deref(), Some(text));

        clipboard.write_text("").expect("write empty");
        change_with(&mut watch, "CF_UNICODETEXT").await;
        assert_eq!(
            clipboard.read_text().expect("read empty").as_deref(),
            Some("")
        );

        write_with_exclusion_marker("secret");
        let change = change_with(&mut watch, "ExcludeClipboardContentFromMonitorProcessing").await;
        assert!(change.has_format("CF_UNICODETEXT"), "{change:?}");

        drop(watch);
        let deadline = Instant::now() + STOP_CHECK_INTERVAL * 5;
        while !watcher.is_finished() {
            assert!(Instant::now() < deadline, "watcher thread still running");
            thread::sleep(Duration::from_millis(20));
        }
        watcher.join().expect("watcher thread panicked");
    }
}
