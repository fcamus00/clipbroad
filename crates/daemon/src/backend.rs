//! Picks the clipboard backend for this platform. This is the only place where backends are wired
//! into the binary.

use std::sync::Arc;

use anyhow::Result;
use clipbroad_clipboard::Clipboard;

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
