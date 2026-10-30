//! Linux: `tray-icon` serves the icon over D-Bus (`StatusNotifierItem`) from its own thread, so
//! there is no event loop to run here.
//!
//! The icon shows only while a tray host (a bar such as waybar, or KDE's panel) owns
//! `org.kde.StatusNotifierWatcher` on the session bus. `tray-icon` registers the icon whenever
//! a watcher appears, even after the daemon started, but it says nothing while there is none. So
//! [`check_host`] looks once at startup and logs it.

use anyhow::Result;
use tracing::{debug, info, warn};
use zbus::blocking::Connection;
use zbus::blocking::fdo::DBusProxy;
use zbus::names::BusName;

const WATCHER: &str = "org.kde.StatusNotifierWatcher";

pub struct EventLoop;

// Does nothing here, but keeps the API every platform shares.
#[allow(clippy::unused_self, clippy::unnecessary_wraps)]
impl EventLoop {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }

    pub fn waker(&self) -> Waker {
        Waker
    }

    /// Nothing to run: the caller then waits for the daemon thread.
    pub fn run(&self) {}
}

pub struct Waker;

// Does nothing here, but keeps the API every platform shares.
#[allow(clippy::unused_self)]
impl Waker {
    pub fn wake(&self) {}
}

/// Logs whether a tray host is running, and what happens if there is none.
pub fn check_host() {
    match watcher_running() {
        Ok(true) => info!("tray host found, showing the icon"),
        Ok(false) => warn!(
            "no tray host on the session bus ({WATCHER}), so no icon is shown; syncing \
             continues, and the icon appears if a bar with a tray starts later"
        ),
        // `TrayIconBuilder::build` fails the same way next, and the caller warns about that.
        Err(error) => debug!("cannot look for a tray host on the session bus: {error}"),
    }
}

fn watcher_running() -> zbus::Result<bool> {
    let bus = Connection::session()?;
    let watcher = BusName::try_from(WATCHER)?;
    Ok(DBusProxy::new(&bus)?.name_has_owner(watcher)?)
}
