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
//! # Sensitive content
//!
//! [`read_text`](Clipboard::read_text), [`read_image`](Clipboard::read_image) and
//! [`read_files`](Clipboard::read_files) open the clipboard once, list the formats, read the DWORD
//! of the `CanUploadToCloudClipboard` / `CanIncludeInClipboardHistory` flags, check them for
//! sensitive markers and read the content, all before closing it. While the clipboard is open no
//! other app can change it, so the check and the content always belong to the same copy. A flag
//! whose value cannot be read fails the read.
//!
//! # Images
//!
//! [`read_image`](Clipboard::read_image) prefers the registered `PNG` format (browsers, the
//! Snipping Tool) and returns its bytes as they are. Otherwise it converts the `CF_DIB` or
//! `CF_DIBV5` the app put on the clipboard to PNG (`PrintScreen`, Paint, most other apps; Windows
//! derives each from the other and from `CF_BITMAP`). A bitmap the converter does not read (see
//! the `dib` module) counts as no image. [`write_image`](Clipboard::write_image) puts the PNG
//! under `PNG` first and a `CF_DIB` copy after it for apps that cannot paste a PNG. Since reads
//! prefer `PNG`, our own write reads back byte for byte, which anti-echo needs.
//!
//! [`read_text`](Clipboard::read_text) only reads `CF_UNICODETEXT` (Windows derives it from
//! `CF_TEXT` and `CF_OEMTEXT`), so markup such as `HTML Format` next to an image never counts as
//! text.
//!
//! # Files
//!
//! [`read_files`](Clipboard::read_files) reads `CF_HDROP`, the list of paths Explorer puts on the
//! clipboard when files are copied (see the `hdrop` module).
//! [`write_files`](Clipboard::write_files) writes the same format with UTF-16 paths, plus
//! `Preferred DropEffect` set to `DROPEFFECT_COPY`, so Explorer pastes a copy and never moves the
//! files out of the cache.
//!
//! # Threads
//!
//! - Reads and writes ([`read_text`](Clipboard::read_text), [`write_text`](Clipboard::write_text)
//!   and their image and file versions): run on a short-lived thread so the caller can give up
//!   after [`OPERATION_TIMEOUT`]. Besides the busy retries, reading text that the source app
//!   renders on demand (delayed rendering) waits for that app, and a frozen app would otherwise
//!   block the caller. An operation that times out leaves its
//!   thread to finish on its own, so a timed-out write may still land later.
//! - [`watch`](Clipboard::watch): one thread with a message-only window registered with
//!   `AddClipboardFormatListener`, which receives a `WM_CLIPBOARDUPDATE` per change. A timer wakes
//!   it every [`STOP_CHECK_INTERVAL`] to check whether the [`ClipboardWatch`] was dropped; if so it
//!   unregisters and destroys the window and ends, even if the clipboard never changes again.
//!   Windows may notify several times for one copy; each notification is reported.

use std::ffi::{OsString, c_void};
use std::io;
use std::marker::PhantomData;
use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::PathBuf;
use std::ptr;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use clipbroad_core::Png;
use clipbroad_core::sensitive::{WINDOWS_FLAG_FORMATS, sensitive_marker};
use windows_sys::Win32::Foundation::{GlobalFree, HGLOBAL, HWND, SetLastError};
use windows_sys::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, EnumClipboardFormats,
    GetClipboardData, GetClipboardFormatNameW, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, RemoveClipboardFormatListener, SetClipboardData,
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

use crate::dib::{self, DibError};
use crate::hdrop;
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

/// `DROPEFFECT_COPY`, the value of `Preferred DropEffect` that makes a paste copy the files.
const DROPEFFECT_COPY: u32 = 1;

/// The Windows clipboard.
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsClipboard;

impl Clipboard for WindowsClipboard {
    fn read_text(&self) -> Result<Option<String>, ClipboardError> {
        run_with_timeout("win-read", || read_unless_sensitive(read_unicode_text))
    }

    fn write_text(&self, text: &str) -> Result<(), ClipboardError> {
        // `CF_UNICODETEXT` is UTF-16 ending in a NUL. Windows derives `CF_TEXT`, `CF_OEMTEXT` and
        // `CF_LOCALE` from it for apps that ask for those.
        let bytes: Vec<u8> = text
            .encode_utf16()
            .chain([0])
            .flat_map(u16::to_ne_bytes)
            .collect();
        run_with_timeout("win-write", move || write_unicode_text(&bytes))
    }

    fn read_image(&self) -> Result<Option<Png>, ClipboardError> {
        run_with_timeout("win-read", read_png)
    }

    fn write_image(&self, png: &Png) -> Result<(), ClipboardError> {
        let png = png.clone();
        run_with_timeout("win-write", move || write_png(png.as_bytes()))
    }

    fn read_files(&self) -> Result<Option<Vec<PathBuf>>, ClipboardError> {
        run_with_timeout("win-read", || read_unless_sensitive(read_file_list))
    }

    fn write_files(&self, paths: &[PathBuf]) -> Result<(), ClipboardError> {
        if paths.iter().any(|path| !path.is_absolute()) {
            return Err(ClipboardError::Backend {
                message: "a file to put on the clipboard has a relative path".to_owned(),
                source: None,
            });
        }
        let wide: Vec<Vec<u16>> = paths
            .iter()
            .map(|path| path.as_os_str().encode_wide().collect())
            .collect();
        let list = hdrop::encode(&wide).ok_or_else(|| ClipboardError::Backend {
            message: "a file to put on the clipboard has a NUL in its path".to_owned(),
            source: None,
        })?;
        run_with_timeout("win-write", move || write_file_list(&list))
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

/// Opens the clipboard and runs `read` on it, unless its formats hold a sensitive marker (see
/// module docs).
fn read_unless_sensitive<T>(
    read: impl FnOnce(&OpenedClipboard) -> Result<T, ClipboardError>,
) -> Result<T, ClipboardError> {
    let open = OpenedClipboard::open()?;
    let formats = format_ids(&open)?
        .into_iter()
        .map(|format| format_name_with_flag(&open, format))
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(marker) = sensitive_marker(&formats) {
        return Err(ClipboardError::Sensitive { marker });
    }
    read(&open)
}

#[allow(unsafe_code)] // Win32 FFI.
fn read_unicode_text(open: &OpenedClipboard) -> Result<Option<String>, ClipboardError> {
    if !is_offered(open, CF_UNICODETEXT.into()) {
        return Ok(None);
    }
    // SAFETY: the clipboard is open on this thread. The handle stays owned by the clipboard and
    // valid until `open` closes it, which happens after we are done with it.
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

fn write_unicode_text(bytes: &[u8]) -> Result<(), ClipboardError> {
    let memory = GlobalMemory::with_bytes(bytes)?;
    let open = OpenedClipboard::open()?;
    empty_clipboard(&open)?;
    memory.set_as(CF_UNICODETEXT.into())
}

/// Image data as found on the clipboard.
enum ClipboardImage {
    Png(Vec<u8>),
    Dib(Vec<u8>),
}

fn read_png() -> Result<Option<Png>, ClipboardError> {
    let png_format = registered_format("PNG")?;
    let image = read_unless_sensitive(|open| {
        if is_offered(open, png_format) {
            return Ok(Some(ClipboardImage::Png(read_bytes(open, png_format)?)));
        }
        // Windows lists the formats an app put before the ones it derives from them, so the
        // first bitmap format listed is the original and needs no conversion by Windows.
        let bitmaps = [CF_DIB, CF_DIBV5].map(u32::from);
        let Some(bitmap) = format_ids(open)?
            .into_iter()
            .find(|format| bitmaps.contains(format))
        else {
            return Ok(None);
        };
        Ok(Some(ClipboardImage::Dib(read_bytes(open, bitmap)?)))
    })?;
    // Converted after closing the clipboard, so other apps are not kept waiting.
    match image {
        None => Ok(None),
        Some(ClipboardImage::Png(mut bytes)) => {
            bytes.truncate(dib::png_len(&bytes).unwrap_or(bytes.len()));
            Png::try_from(bytes)
                .map(Some)
                .map_err(|error| ClipboardError::backend("reading the PNG format", error))
        }
        Some(ClipboardImage::Dib(bitmap)) => match dib::dib_to_png(&bitmap) {
            Ok(bytes) => Ok(Some(
                Png::try_from(bytes).expect("the PNG encoder writes a valid PNG header"),
            )),
            Err(DibError::Unsupported(reason)) => {
                tracing::debug!(reason, "the clipboard bitmap is not converted to PNG");
                Ok(None)
            }
            Err(error) => Err(ClipboardError::backend(
                "converting the clipboard bitmap to PNG",
                error,
            )),
        },
    }
}

fn write_png(png: &[u8]) -> Result<(), ClipboardError> {
    let png_format = registered_format("PNG")?;
    // For apps that cannot paste a PNG (Paint and many older apps). Windows derives `CF_DIBV5`
    // and `CF_BITMAP` from it.
    let bitmap = match dib::png_to_dib(png) {
        Ok(bitmap) => Some(GlobalMemory::with_bytes(&bitmap)?),
        Err(error) => {
            tracing::warn!(
                ?error,
                "cannot convert the image to a bitmap; writing only the PNG"
            );
            None
        }
    };
    let png_memory = GlobalMemory::with_bytes(png)?;
    let open = OpenedClipboard::open()?;
    empty_clipboard(&open)?;
    // `PNG` first: apps that take the first format they know get the lossless one.
    png_memory.set_as(png_format)?;
    bitmap.map_or(Ok(()), |bitmap| bitmap.set_as(CF_DIB.into()))
}

fn read_file_list(open: &OpenedClipboard) -> Result<Option<Vec<PathBuf>>, ClipboardError> {
    if !is_offered(open, CF_HDROP.into()) {
        return Ok(None);
    }
    let Some(paths) = hdrop::decode(&read_bytes(open, CF_HDROP.into())?) else {
        tracing::debug!("the clipboard CF_HDROP holds no readable paths; treating it as no files");
        return Ok(None);
    };
    Ok(Some(
        paths
            .iter()
            .map(|path| PathBuf::from(OsString::from_wide(path)))
            .collect(),
    ))
}

fn write_file_list(list: &[u8]) -> Result<(), ClipboardError> {
    let drop_effect_format = registered_format("Preferred DropEffect")?;
    let list_memory = GlobalMemory::with_bytes(list)?;
    let drop_effect = GlobalMemory::with_bytes(&DROPEFFECT_COPY.to_le_bytes())?;
    let open = OpenedClipboard::open()?;
    empty_clipboard(&open)?;
    list_memory.set_as(CF_HDROP.into())?;
    drop_effect.set_as(drop_effect_format)
}

/// The id of the registered clipboard format `name`.
#[allow(unsafe_code)] // Win32 FFI.
fn registered_format(name: &str) -> Result<u32, ClipboardError> {
    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    // SAFETY: plain call with a valid NUL-terminated string. Registering an existing name returns
    // its id.
    match unsafe { RegisterClipboardFormatW(wide.as_ptr()) } {
        0 => Err(ClipboardError::backend(
            format!("registering the {name} clipboard format"),
            io::Error::last_os_error(),
        )),
        format => Ok(format),
    }
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
    /// A block holding exactly `bytes`.
    #[allow(unsafe_code)] // Win32 FFI.
    fn with_bytes(bytes: &[u8]) -> Result<Self, ClipboardError> {
        // SAFETY: plain allocation; a null result is handled below.
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) };
        if handle.is_null() {
            return Err(ClipboardError::backend(
                "allocating clipboard memory",
                io::Error::last_os_error(),
            ));
        }
        let memory = Self(handle);
        let locked = LockedGlobal::lock(memory.0)?;
        // SAFETY: the block was allocated with room for exactly `bytes.len()` bytes, and a fresh
        // allocation cannot overlap `bytes`.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), locked.data.cast(), bytes.len()) };
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

    /// The whole block as bytes.
    #[allow(unsafe_code)] // Win32 FFI.
    fn bytes(&self) -> &[u8] {
        // SAFETY: the handle is valid (we locked it).
        let len = unsafe { GlobalSize(self.handle) };
        // SAFETY: while locked, `data` points to the block's `GlobalSize` bytes. The returned
        // slice borrows `self`, so it cannot outlive the lock.
        unsafe { std::slice::from_raw_parts(self.data.cast(), len) }
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

/// The names of the formats on the clipboard, in the order Windows reports them.
fn list_formats() -> Result<Vec<String>, ClipboardError> {
    let open = OpenedClipboard::open()?;
    Ok(format_ids(&open)?.into_iter().map(format_name).collect())
}

/// The ids of the formats on the open clipboard, in the order Windows reports them. Fails if the
/// list cannot be read completely, so a missing sensitive-content marker never goes unnoticed.
#[allow(unsafe_code)] // Win32 FFI.
fn format_ids(_open: &OpenedClipboard) -> Result<Vec<u32>, ClipboardError> {
    let mut formats = Vec::new();
    let mut format = 0;
    loop {
        // A 0 result is only told apart from a failure by the last error, and `format_name` may
        // have left one behind, so start each call from a clean slate.
        // SAFETY: plain calls; the clipboard is open on this thread.
        format = unsafe {
            SetLastError(0);
            EnumClipboardFormats(format)
        };
        if format == 0 {
            break;
        }
        formats.push(format);
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

/// [`format_name`], plus `=<value>` for the flag formats in [`WINDOWS_FLAG_FORMATS`], e.g.
/// `CanUploadToCloudClipboard=0`.
fn format_name_with_flag(open: &OpenedClipboard, format: u32) -> Result<String, ClipboardError> {
    let name = format_name(format);
    if !WINDOWS_FLAG_FORMATS.contains(&name.as_str()) {
        return Ok(name);
    }
    let value = read_dword(open, format)
        .map_err(|error| ClipboardError::backend(format!("reading the {name} flag"), error))?;
    Ok(format!("{name}={value}"))
}

/// Whether the open clipboard offers `format`, directly or derived by Windows.
#[allow(unsafe_code)] // Win32 FFI.
fn is_offered(_open: &OpenedClipboard, format: u32) -> bool {
    // SAFETY: plain query; the clipboard is open on this thread.
    unsafe { IsClipboardFormatAvailable(format) != 0 }
}

/// Removes all content from the open clipboard and makes us its owner, as required before
/// setting any.
#[allow(unsafe_code)] // Win32 FFI.
fn empty_clipboard(_open: &OpenedClipboard) -> Result<(), ClipboardError> {
    // SAFETY: the clipboard is open on this thread.
    if unsafe { EmptyClipboard() } == 0 {
        return Err(ClipboardError::backend(
            "emptying the clipboard",
            io::Error::last_os_error(),
        ));
    }
    Ok(())
}

/// The DWORD held by `format` on the open clipboard.
fn read_dword(open: &OpenedClipboard, format: u32) -> Result<u32, ClipboardError> {
    let bytes = read_bytes(open, format)?;
    let dword = bytes
        .first_chunk::<4>()
        .ok_or_else(|| ClipboardError::Backend {
            message: "the data is smaller than a DWORD".to_owned(),
            source: None,
        })?;
    Ok(u32::from_le_bytes(*dword))
}

/// A copy of the data held by `format` on the open clipboard (the whole memory block, which may
/// be bigger than the data put in it).
#[allow(unsafe_code)] // Win32 FFI.
fn read_bytes(_open: &OpenedClipboard, format: u32) -> Result<Vec<u8>, ClipboardError> {
    // SAFETY: the clipboard is open on this thread. The handle stays owned by the clipboard and
    // valid until `_open` closes it, after we are done with it.
    let handle = unsafe { GetClipboardData(format) };
    if handle.is_null() {
        return Err(ClipboardError::backend(
            "getting the clipboard data",
            io::Error::last_os_error(),
        ));
    }
    Ok(LockedGlobal::lock(handle)?.bytes().to_vec())
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
    use super::*;

    #[test]
    fn names_predefined_formats_by_their_constant() {
        assert_eq!(format_name(CF_UNICODETEXT.into()), "CF_UNICODETEXT");
        assert_eq!(format_name(CF_HDROP.into()), "CF_HDROP");
        // Private formats have no name.
        assert_eq!(format_name(0x200), "#512");
    }

    fn registered(name: &str) -> u32 {
        registered_format(name).expect("register the format")
    }

    /// `CF_UNICODETEXT` data for `text`.
    fn utf16(text: &str) -> Vec<u8> {
        text.encode_utf16()
            .chain([0])
            .flat_map(u16::to_ne_bytes)
            .collect()
    }

    /// Replaces the clipboard content with these formats, as one copy.
    fn write_formats(formats: &[(u32, &[u8])]) {
        let memory: Vec<_> = formats
            .iter()
            .map(|&(format, data)| (format, GlobalMemory::with_bytes(data).expect("allocate")))
            .collect();
        let open = OpenedClipboard::open().expect("open clipboard");
        empty_clipboard(&open).expect("empty clipboard");
        for (format, memory) in memory {
            memory.set_as(format).expect("set format");
        }
    }

    /// Writes `text` plus a registered format `marker` holding the DWORD `value`, the way a
    /// password manager adds its markers.
    fn write_with_marker(text: &str, marker: &str, value: u32) {
        write_formats(&[
            (CF_UNICODETEXT.into(), &utf16(text)),
            (registered(marker), &value.to_le_bytes()),
        ]);
    }

    /// A 2x1 24-bit bottom-up `CF_DIB`: a red and a green pixel.
    fn small_dib() -> Vec<u8> {
        let mut dib = Vec::new();
        // Header size, width, height, planes (1) and bits per pixel (24), then BI_RGB and zeros.
        for field in [40_u32, 2, 1, (24 << 16) | 1, 0, 0, 0, 0, 0, 0] {
            dib.extend(field.to_le_bytes());
        }
        dib.extend([0, 0, 255, 0, 255, 0, 0, 0]);
        dib
    }

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

    /// The image part of the smoke test.
    async fn check_images(clipboard: &WindowsClipboard, watch: &mut ClipboardWatch) {
        let exclude = "ExcludeClipboardContentFromMonitorProcessing";
        // An image we write reads back byte for byte, and is offered as a bitmap too.
        let png = Png::try_from(dib::dib_to_png(&small_dib()).expect("to PNG")).expect("PNG");
        clipboard.write_image(&png).expect("write image");
        let change = change_with(watch, "PNG").await;
        assert!(change.has_format("CF_DIB"), "{change:?}");
        assert_eq!(
            clipboard.read_image().expect("read image"),
            Some(png.clone())
        );
        assert_eq!(clipboard.read_text().expect("read text"), None);

        // A bitmap with no PNG (PrintScreen, Paint) is converted.
        write_formats(&[(CF_DIB.into(), &small_dib())]);
        change_with(watch, "CF_DIB").await;
        assert_eq!(
            clipboard.read_image().expect("read bitmap"),
            Some(png.clone())
        );

        // A browser's "Copy image": the image plus `<img>` markup. The image wins.
        let html = registered("HTML Format");
        write_formats(&[
            (html, b"<img src=\"x.png\">\0"),
            (registered("PNG"), png.as_bytes()),
        ]);
        change_with(watch, "HTML Format").await;
        assert_eq!(clipboard.read_text().expect("read text"), None);
        assert_eq!(
            clipboard.read_image().expect("read image"),
            Some(png.clone())
        );

        // A sensitive marker hides the image too.
        write_formats(&[
            (registered("PNG"), png.as_bytes()),
            (registered(exclude), &0_u32.to_le_bytes()),
        ]);
        change_with(watch, exclude).await;
        assert!(matches!(
            clipboard.read_image(),
            Err(ClipboardError::Sensitive { marker }) if marker == exclude
        ));
    }

    /// The file part of the smoke test.
    async fn check_files(clipboard: &WindowsClipboard, watch: &mut ClipboardWatch) {
        let dir = tempfile::tempdir().expect("temp dir");
        let files: Vec<PathBuf> = ["a b.txt", "ñandú 🦀.txt"]
            .into_iter()
            .map(|name| dir.path().join(name))
            .collect();
        for file in &files {
            std::fs::write(file, "contents").expect("create file");
        }

        // Files we write read back as the same paths, and paste as a copy.
        clipboard.write_files(&files).expect("write files");
        let change = change_with(watch, "CF_HDROP").await;
        assert!(change.has_format("Preferred DropEffect"), "{change:?}");
        assert_eq!(
            clipboard.read_files().expect("read files"),
            Some(files.clone())
        );

        // A copied web link is text, not files.
        let url = "https://example.com/file.txt";
        write_formats(&[
            (CF_UNICODETEXT.into(), &utf16(url)),
            (registered("UniformResourceLocatorW"), &utf16(url)),
        ]);
        change_with(watch, "UniformResourceLocatorW").await;
        assert_eq!(clipboard.read_files().expect("read files"), None);
        assert_eq!(
            clipboard.read_text().expect("read text").as_deref(),
            Some(url)
        );

        // A sensitive marker hides files too.
        let wide: Vec<Vec<u16>> = files
            .iter()
            .map(|file| file.as_os_str().encode_wide().collect())
            .collect();
        let exclude = "ExcludeClipboardContentFromMonitorProcessing";
        write_formats(&[
            (CF_HDROP.into(), &hdrop::encode(&wide).expect("valid paths")),
            (registered(exclude), &0_u32.to_le_bytes()),
        ]);
        change_with(watch, exclude).await;
        assert!(matches!(
            clipboard.read_files(),
            Err(ClipboardError::Sensitive { marker }) if marker == exclude
        ));
    }

    /// Smoke test against the real clipboard. GitHub's Windows runners allow clipboard access,
    /// so CI runs it. It overwrites the clipboard. One test on purpose: the clipboard is global,
    /// and nextest runs tests in parallel processes that would see each other's changes.
    #[tokio::test]
    async fn real_clipboard_round_trip_and_watch() {
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

        // The exclusion marker counts by presence, whatever its value; a DWORD 0 is customary.
        let exclude = "ExcludeClipboardContentFromMonitorProcessing";
        write_with_marker("secret", exclude, 0);
        let change = change_with(&mut watch, exclude).await;
        assert!(change.has_format("CF_UNICODETEXT"), "{change:?}");
        assert!(matches!(
            clipboard.read_text(),
            Err(ClipboardError::Sensitive { marker }) if marker == exclude
        ));

        // The flags count only when 0.
        for flag in WINDOWS_FLAG_FORMATS {
            write_with_marker("secret", flag, 0);
            change_with(&mut watch, flag).await;
            let zero = format!("{flag}=0");
            assert!(matches!(
                clipboard.read_text(),
                Err(ClipboardError::Sensitive { marker }) if marker == zero
            ));

            write_with_marker("shareable", flag, 1);
            change_with(&mut watch, flag).await;
            assert_eq!(
                clipboard.read_text().expect("read").as_deref(),
                Some("shareable")
            );
        }

        check_images(&clipboard, &mut watch).await;
        check_files(&clipboard, &mut watch).await;

        drop(watch);
        let deadline = Instant::now() + STOP_CHECK_INTERVAL * 5;
        while !watcher.is_finished() {
            assert!(Instant::now() < deadline, "watcher thread still running");
            thread::sleep(Duration::from_millis(20));
        }
        watcher.join().expect("watcher thread panicked");
    }
}
