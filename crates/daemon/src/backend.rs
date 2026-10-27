//! Picks the clipboard backend for this platform. This is the only place where backends are wired
//! into the binary.

use std::sync::Arc;

use anyhow::{Result, bail};
use clipbroad_clipboard::Clipboard;

/// The system clipboard of this machine.
pub fn system_clipboard() -> Result<Arc<dyn Clipboard>> {
    // TODO(phase 1): one `#[cfg(target_os = ...)]` branch per backend, each returning early:
    // - Linux: Wayland backend (`wl-clipboard-rs`).
    // - macOS: polling `NSPasteboard.changeCount`.
    // - Windows: `AddClipboardFormatListener`.
    // Platforms without a backend fall through to this error.
    bail!(
        "the clipboard backend for {} is not implemented yet; \
         run `clipbroad watch --fake` to try it with a simulated clipboard",
        std::env::consts::OS
    )
}
