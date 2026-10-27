//! Picks the clipboard backend for this platform. This is the only place where backends are wired
//! into the binary.

use std::sync::Arc;

use anyhow::Result;
use clipbroad_clipboard::Clipboard;

use crate::status::PasteAccess;

/// The system clipboard of this machine.
#[cfg(target_os = "linux")]
pub fn system_clipboard() -> Result<Arc<dyn Clipboard>> {
    Ok(Arc::new(
        clipbroad_clipboard::wayland::WaylandClipboard::new()?,
    ))
}

/// The system clipboard of this machine.
#[cfg(target_os = "macos")]
// Never fails on macOS, but keeps the signature every platform shares.
#[allow(clippy::unnecessary_wraps)]
pub fn system_clipboard() -> Result<Arc<dyn Clipboard>> {
    Ok(Arc::new(clipbroad_clipboard::macos::MacosClipboard))
}

/// The system clipboard of this machine.
#[cfg(windows)]
// Never fails on Windows, but keeps the signature every platform shares.
#[allow(clippy::unnecessary_wraps)]
pub fn system_clipboard() -> Result<Arc<dyn Clipboard>> {
    Ok(Arc::new(clipbroad_clipboard::windows::WindowsClipboard))
}

/// The system clipboard of this machine.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn system_clipboard() -> Result<Arc<dyn Clipboard>> {
    anyhow::bail!(
        "{} is not supported; run `clipbroad watch --fake` to try it with a simulated clipboard",
        std::env::consts::OS
    )
}

/// What macOS lets this app do with the pasteboard, for `clipbroad status`. `None` before macOS
/// 15.4, which has no such setting.
#[cfg(target_os = "macos")]
pub fn paste_access() -> Option<PasteAccess> {
    use clipbroad_clipboard::macos::{AccessBehavior, MacosClipboard};
    Some(match MacosClipboard.access_behavior()? {
        AccessBehavior::Default => PasteAccess::Default,
        AccessBehavior::Ask => PasteAccess::Ask,
        AccessBehavior::AlwaysAllow => PasteAccess::AlwaysAllow,
        AccessBehavior::AlwaysDeny => PasteAccess::AlwaysDeny,
        AccessBehavior::Other(value) => PasteAccess::Other(value),
    })
}

/// Only macOS restricts reading the clipboard.
#[cfg(not(target_os = "macos"))]
pub fn paste_access() -> Option<PasteAccess> {
    None
}
