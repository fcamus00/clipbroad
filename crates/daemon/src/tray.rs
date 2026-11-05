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

use std::cell::{Cell, RefCell};
use std::panic;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use iroh::EndpointId;
use tokio::sync::mpsc::UnboundedSender;
use tracing::warn;
use tray_icon::menu::{
    Menu as NativeMenu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::menu::{self, Action, Gui, MemberItem, Menu, View};

#[cfg_attr(target_os = "linux", path = "tray/linux.rs")]
#[cfg_attr(target_os = "macos", path = "tray/macos.rs")]
#[cfg_attr(windows, path = "tray/windows.rs")]
mod platform;

/// How long a change waits at most to reach the menu, and an ended daemon to end the process.
const POLL_INTERVAL: Duration = Duration::from_millis(300);

/// Shows the tray icon and runs `daemon` on a new thread until it returns.
///
/// Call it from the main thread. It returns the daemon's result once the icon is gone.
pub fn run<D, T>(daemon: D) -> Result<T>
where
    D: FnOnce(Menu) -> Result<T> + Send + 'static,
    T: Send + 'static,
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
    update: MenuItem,
    /// "Remove a machine", with a submenu per member.
    remove: Submenu,
    /// "Reset identity", with its confirmation.
    reset: Submenu,
    /// The members `remove` lists now.
    members: RefCell<Vec<MemberItem>>,
    /// The confirmation item of each member in `remove`, shared with the click handler.
    removals: Arc<Mutex<Vec<(MenuId, EndpointId)>>>,
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
        let update = MenuItem::new(&view.update.text, view.update.enabled, None);
        let remove = Submenu::new("Remove a machine", false);
        let reset_warning = MenuItem::new("Leaves the mesh. Pair again to rejoin", false, None);
        let reset_confirm = MenuItem::new("Reset identity and restart", true, None);
        let reset = Submenu::with_items(
            "Reset identity",
            view.can_pair,
            &[&reset_warning, &reset_confirm],
        )
        .context("cannot build the tray menu")?;
        let quit = MenuItem::new("Quit clipbroad", true, None);
        let menu = NativeMenu::with_items(&[
            &status,
            &activity,
            &PredefinedMenuItem::separator(),
            &pair,
            &pair_copied,
            &pause,
            &remove,
            &reset,
            &PredefinedMenuItem::separator(),
            &update,
            &quit,
        ])
        .context("cannot build the tray menu")?;
        let choices = [
            (pair.id().clone(), Action::Pair),
            (pair_copied.id().clone(), Action::PairCopied),
            (pause.id().clone(), Action::TogglePause),
            (update.id().clone(), Action::Update),
            (reset_confirm.id().clone(), Action::Reset),
            (quit.id().clone(), Action::Quit),
        ];
        let removals: Arc<Mutex<Vec<(MenuId, EndpointId)>>> = Arc::default();
        let clicked_removals = Arc::clone(&removals);
        // Called on the main thread (macOS, Windows) or on the D-Bus thread (Linux).
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let fixed = choices.iter().find(|(id, _)| *id == event.id);
            let action = fixed.map(|(_, action)| *action).or_else(|| {
                let removals = lock(&clicked_removals);
                let removal = removals.iter().find(|(id, _)| *id == event.id);
                removal.map(|(_, member)| Action::Remove(*member))
            });
            if let Some(action) = action {
                // Fails if the daemon ended, and then there is nothing to do.
                let _ = actions.send(action);
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
            update,
            remove,
            reset,
            members: RefCell::new(Vec::new()),
            removals,
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
        self.reset.set_enabled(view.can_pair);
        self.pause.set_text(pause_label(view.paused));
        self.update.set_text(&view.update.text);
        self.update.set_enabled(view.update.enabled);
        if *self.members.borrow() != view.members {
            self.draw_members(&view.members);
        }
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

impl Tray {
    /// Rebuilds "Remove a machine": a submenu per member that says what removing means and
    /// holds the item that does it, so a removal takes two deliberate clicks.
    fn draw_members(&self, members: &[MemberItem]) {
        while self.remove.remove_at(0).is_some() {}
        let mut removals = Vec::new();
        for member in members {
            let warning = MenuItem::new("It cannot join again with this identity", false, None);
            let confirm = MenuItem::new(format!("Remove {} for good", member.label), true, None);
            let built = Submenu::with_items(&member.label, true, &[&warning, &confirm])
                .and_then(|submenu| self.remove.append(&submenu));
            match built {
                Ok(()) => removals.push((confirm.id().clone(), member.id)),
                Err(error) => warn!("cannot add {} to the tray menu: {error}", member.label),
            }
        }
        self.remove.set_enabled(!removals.is_empty());
        *lock(&self.removals) = removals;
        *self.members.borrow_mut() = members.to_vec();
    }
}

/// Nothing panics while holding it, and each change is one assignment.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
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
