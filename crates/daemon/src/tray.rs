//! The tray icon of `clipbroad run`: the same icon and menu on every OS.
//!
//! The icon lives in the daemon's process. The main thread runs the OS's GUI event loop, which
//! macOS requires there, and the daemon runs on its own thread with its own tokio runtime. Each
//! platform module provides that loop:
//! - Linux: none. The icon is a D-Bus `StatusNotifierItem` that `tray-icon` serves from its own
//!   thread, so the main thread only waits for the daemon.
//! - macOS: an `NSApplication` without a Dock icon, pumped by hand on the main thread.
//! - Windows: a message loop on the thread that owns the icon's hidden window.
//!
//! If the icon cannot be shown, the daemon keeps running without it and the log says why.

use std::panic;
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result};
use tokio::sync::Notify;
use tracing::warn;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

#[cfg_attr(target_os = "linux", path = "tray/linux.rs")]
#[cfg_attr(target_os = "macos", path = "tray/macos.rs")]
#[cfg_attr(windows, path = "tray/windows.rs")]
mod platform;

/// The menu's "Quit". The daemon should shut down as on Ctrl+C once it is [chosen](Self::chosen).
pub struct Quit(Arc<Notify>);

impl Quit {
    /// Completes when "Quit" is chosen, even if that happened before the call.
    pub async fn chosen(self) {
        self.0.notified().await;
    }
}

/// Shows the tray icon and runs `daemon` on a new thread until it returns.
///
/// Call it from the main thread. It returns the daemon's result once the icon is gone.
pub fn run<D>(daemon: D) -> Result<()>
where
    D: FnOnce(Quit) -> Result<()> + Send + 'static,
{
    let event_loop = platform::EventLoop::new()?;
    let quit = Arc::new(Notify::new());
    let tray = match show(&quit) {
        Ok(tray) => Some(tray),
        Err(error) => {
            warn!("cannot show the tray icon, syncing without it: {error:#}");
            None
        }
    };
    let waker = event_loop.waker();
    let daemon = thread::Builder::new()
        .name("daemon".to_owned())
        .spawn(move || {
            let result = daemon(Quit(quit));
            waker.wake();
            result
        })
        .context("cannot start the daemon thread")?;
    if tray.is_some() {
        event_loop.run();
    }
    // On Linux there is no loop, so this is where the main thread waits.
    let result = daemon.join();
    // Removes the icon before the process ends: on Windows, an icon left behind stays in the
    // notification area until the mouse goes over it.
    drop(tray);
    result.unwrap_or_else(|payload| panic::resume_unwind(payload))
}

/// Adds the icon and its menu. Choosing "Quit" wakes whoever waits on `quit`.
fn show(quit: &Arc<Notify>) -> Result<TrayIcon> {
    platform::check_host();
    let quit_item = MenuItem::new("Quit clipbroad", true, None);
    let menu = Menu::with_items(&[&quit_item]).context("cannot build the tray menu")?;
    let quit_id = quit_item.id().clone();
    let quit = Arc::clone(quit);
    // Called on the main thread (macOS, Windows) or on the D-Bus thread (Linux).
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id == quit_id {
            // Kept as a permit if the daemon is not waiting yet, so the click is not lost.
            quit.notify_one();
        }
    }));
    TrayIconBuilder::new()
        .with_id("clipbroad")
        .with_tooltip("clipbroad")
        .with_icon(icon()?)
        .with_menu(Box::new(menu))
        .build()
        .context("cannot add the icon")
}

/// A filled circle, drawn here so the binary needs no image file. The real icon comes later.
fn icon() -> Result<Icon> {
    const SIZE: u32 = 64;
    const COLOR: [u8; 3] = [0x2f, 0x7d, 0xe1];
    // Twice the distance from the center, so the center (31.5, 31.5) is a whole number.
    let inside = |x: u32, y: u32| {
        let (dx, dy) = ((2 * x).abs_diff(SIZE - 1), (2 * y).abs_diff(SIZE - 1));
        dx * dx + dy * dy <= (SIZE - 4) * (SIZE - 4)
    };
    let rgba = (0..SIZE * SIZE)
        .flat_map(|pixel| {
            let alpha = if inside(pixel % SIZE, pixel / SIZE) {
                0xff
            } else {
                0
            };
            [COLOR[0], COLOR[1], COLOR[2], alpha]
        })
        .collect();
    Icon::from_rgba(rgba, SIZE, SIZE).context("cannot build the icon image")
}
