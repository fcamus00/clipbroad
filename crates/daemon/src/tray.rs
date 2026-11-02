//! The tray icon of `clipbroad run`: the same icon and menu on every OS.
//!
//! The icon lives in the daemon's process. The main thread runs the OS's GUI event loop, which
//! macOS requires there, and the daemon runs on its own thread with its own tokio runtime. Each
//! platform module provides that loop:
//! - Linux: no GUI events. The icon is a D-Bus `StatusNotifierItem` that `tray-icon` serves from
//!   its own thread.
//! - macOS: an `NSApplication` without a Dock icon, pumped by hand on the main thread.
//! - Windows: a message loop on the thread that owns the icon's hidden window.
//!
//! Those loops only draw. What the menu does is the daemon's job (see [`crate::menu`]): each click
//! becomes an [`Action`], and a [`View`] comes back whenever the menu should change. GUI objects
//! must stay on the thread that made them, so each loop looks for new views every
//! [`POLL_INTERVAL`] on that thread, and stops when the daemon is gone.
//!
//! If the icon cannot be shown, the daemon keeps running without it and the log says why.

use std::cell::Cell;
use std::panic;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc::UnboundedSender;
use tracing::warn;
use tray_icon::menu::{Menu as NativeMenu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::menu::{self, Action, Gui, Menu, View};

#[cfg_attr(target_os = "linux", path = "tray/linux.rs")]
#[cfg_attr(target_os = "macos", path = "tray/macos.rs")]
#[cfg_attr(windows, path = "tray/windows.rs")]
mod platform;

/// How long a change waits at most to reach the menu, and an ended daemon to end the process.
const POLL_INTERVAL: Duration = Duration::from_millis(300);

/// Shows the tray icon and runs `daemon` on a new thread until it returns.
///
/// Call it from the main thread. It returns the daemon's result once the icon is gone.
pub fn run<D>(daemon: D) -> Result<()>
where
    D: FnOnce(Menu) -> Result<()> + Send + 'static,
{
    let event_loop = platform::EventLoop::new()?;
    let (menu, Gui { actions, views }) = menu::channel();
    let tray = match Tray::show(actions) {
        Ok(tray) => Some(tray),
        Err(error) => {
            warn!("cannot show the tray icon, syncing without it: {error:#}");
            None
        }
    };
    let daemon = thread::Builder::new()
        .name("daemon".to_owned())
        .spawn(move || daemon(menu))
        .context("cannot start the daemon thread")?;
    if let Some(tray) = &tray {
        event_loop.run(|| tray.update(&views));
    }
    let result = daemon.join();
    // Removes the icon before the process ends: on Windows, an icon left behind stays in the
    // notification area until the mouse goes over it.
    drop(tray);
    result.unwrap_or_else(|payload| panic::resume_unwind(payload))
}

/// The icon and the menu items that change.
struct Tray {
    icon: TrayIcon,
    status: MenuItem,
    activity: MenuItem,
    pair: MenuItem,
    pair_copied: MenuItem,
    pause: MenuItem,
    syncing: Cell<bool>,
}

impl Tray {
    /// Adds the icon and its menu. Each click sends its action to `actions`.
    fn show(actions: UnboundedSender<Action>) -> Result<Self> {
        platform::check_host();
        let view = View::starting();
        let status = MenuItem::new(&view.status, false, None);
        let activity = MenuItem::new(&view.activity, false, None);
        let pair = MenuItem::new("Pair a machine", view.can_pair, None);
        let pair_copied = MenuItem::new("Pair with the copied ticket", view.can_pair, None);
        let pause = MenuItem::new(pause_label(view.paused), true, None);
        let quit = MenuItem::new("Quit clipbroad", true, None);
        let menu = NativeMenu::with_items(&[
            &status,
            &activity,
            &PredefinedMenuItem::separator(),
            &pair,
            &pair_copied,
            &pause,
            &PredefinedMenuItem::separator(),
            &quit,
        ])
        .context("cannot build the tray menu")?;
        let choices = [
            (pair.id().clone(), Action::Pair),
            (pair_copied.id().clone(), Action::PairCopied),
            (pause.id().clone(), Action::TogglePause),
            (quit.id().clone(), Action::Quit),
        ];
        // Called on the main thread (macOS, Windows) or on the D-Bus thread (Linux).
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            if let Some((_, action)) = choices.iter().find(|(id, _)| *id == event.id) {
                // Fails if the daemon ended, and then there is nothing to do.
                let _ = actions.send(*action);
            }
        }));
        let icon = TrayIconBuilder::new()
            .with_id("clipbroad")
            .with_tooltip(&view.status)
            .with_icon(image(view.syncing)?)
            .with_menu(Box::new(menu))
            .build()
            .context("cannot add the icon")?;
        Ok(Self {
            icon,
            status,
            activity,
            pair,
            pair_copied,
            pause,
            syncing: Cell::new(view.syncing),
        })
    }

    /// Draws the views that arrived. Returns false once the daemon has ended.
    fn update(&self, views: &Receiver<View>) -> bool {
        loop {
            match views.try_recv() {
                Ok(view) => self.draw(&view),
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => return false,
            }
        }
    }

    fn draw(&self, view: &View) {
        self.status.set_text(&view.status);
        self.activity.set_text(&view.activity);
        self.pair.set_enabled(view.can_pair);
        self.pair_copied.set_enabled(view.can_pair);
        self.pause.set_text(pause_label(view.paused));
        if let Err(error) = self.icon.set_tooltip(Some(&view.status)) {
            warn!("cannot update the tray tooltip: {error}");
        }
        if self.syncing.replace(view.syncing) != view.syncing {
            let icon = image(view.syncing).map(Some);
            if let Err(error) = icon.and_then(|icon| Ok(self.icon.set_icon(icon)?)) {
                warn!("cannot update the tray icon: {error:#}");
            }
        }
    }
}

fn pause_label(paused: bool) -> &'static str {
    if paused { "Resume sync" } else { "Pause sync" }
}

/// A filled blue circle while clips flow, or else a grey ring. Drawn here so the binary needs no
/// image file, and different in shape so that it does not depend on telling colors apart.
fn image(syncing: bool) -> Result<Icon> {
    const SIZE: u32 = 64;
    const RING: u32 = 12;
    let color = if syncing {
        [0x2f, 0x7d, 0xe1]
    } else {
        [0x8a, 0x8a, 0x8a]
    };
    // Twice the distance from the center, so the center (31.5, 31.5) is a whole number.
    let distance = |x: u32, y: u32| {
        let (dx, dy) = ((2 * x).abs_diff(SIZE - 1), (2 * y).abs_diff(SIZE - 1));
        dx * dx + dy * dy
    };
    let inner = if syncing { 0 } else { SIZE - 4 - 2 * RING };
    let rgba = (0..SIZE * SIZE)
        .flat_map(|pixel| {
            let distance = distance(pixel % SIZE, pixel / SIZE);
            let drawn = (inner * inner..=(SIZE - 4) * (SIZE - 4)).contains(&distance);
            [color[0], color[1], color[2], if drawn { 0xff } else { 0 }]
        })
        .collect();
    Icon::from_rgba(rgba, SIZE, SIZE).context("cannot build the icon image")
}
