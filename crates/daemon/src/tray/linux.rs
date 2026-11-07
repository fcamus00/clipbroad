//! Linux: `tray-icon` serves the icon over D-Bus (`StatusNotifierItem`) from its own thread, so
//! there are no GUI events to handle here. [`EventLoop::run`] sleeps until the next view.
//!
//! The icon shows only while a tray host (a bar such as waybar, or KDE's panel) owns
//! `org.kde.StatusNotifierWatcher` on the session bus. `tray-icon` registers the icon whenever
//! a watcher appears, even after the daemon started, but it says nothing while there is none. So
//! [`check_host`] looks once at startup and logs it.

use std::sync::mpsc::Receiver;

use anyhow::Result;
use tracing::{debug, info, warn};
use zbus::blocking::Connection;
use zbus::blocking::fdo::DBusProxy;
use zbus::names::BusName;

use crate::menu::View;

const WATCHER: &str = "org.kde.StatusNotifierWatcher";

pub struct EventLoop;

impl EventLoop {
    // It cannot fail here, but can on macOS.
    #[allow(clippy::unnecessary_wraps)]
    pub fn new() -> Result<Self> {
        Ok(Self)
    }

    /// Draws each view as it arrives, and returns once the daemon has ended (`views` is
    /// disconnected). It blocks in between, so the main thread does not wake up while idle.
    // It keeps `self` like on the other platforms.
    #[allow(clippy::unused_self)]
    pub fn run(&self, views: &Receiver<View>, mut draw: impl FnMut(&View)) {
        for view in views {
            draw(&view);
        }
    }
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

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::*;

    fn view(status: &str) -> View {
        View {
            status: status.to_owned(),
            ..View::starting()
        }
    }

    #[test]
    fn draws_every_view_and_ends_with_the_daemon() {
        let (send, views) = mpsc::channel();
        let daemon = thread::spawn(move || {
            send.send(view("one")).expect("loop is listening");
            // The loop waits for this one instead of returning.
            thread::sleep(Duration::from_millis(50));
            send.send(view("two")).expect("loop is listening");
            // Dropping `send` is the daemon ending.
        });
        let mut drawn = Vec::new();
        EventLoop.run(&views, |view| drawn.push(view.status.clone()));
        daemon.join().expect("daemon thread panicked");
        assert_eq!(drawn, ["one", "two"]);
    }
}
