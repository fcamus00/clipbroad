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
// TODO(phase 1): a `#[cfg(target_os = ...)]` function per backend, like the Linux one:
// - macOS: polling `NSPasteboard.changeCount`.
// - Windows: `AddClipboardFormatListener`.
#[cfg(not(target_os = "linux"))]
pub fn system_clipboard() -> Result<Arc<dyn Clipboard>> {
    anyhow::bail!(
        "the clipboard backend for {} is not implemented yet; \
         run `clipbroad watch --fake` to try it with a simulated clipboard",
        std::env::consts::OS
    )
}
