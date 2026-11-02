//! What the tray menu shows and asks, without any GUI, so tests can drive it.
//!
//! The daemon owns a [`Menu`]: it receives each [`Action`] the person chooses and sends a [`View`]
//! whenever what the menu should show changes. [`crate::tray`] holds the other end, the [`Gui`],
//! and only draws. Without an icon, nobody holds that end: no actions arrive and views go nowhere.

use std::sync::mpsc::{self as std_mpsc, Receiver, Sender};

use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::pair::TICKET_TTL;
use crate::status::DaemonStatus;
use crate::update::{State, VERSION};

/// How long a note stays in the menu, so a result is still there when the person looks. A ticket
/// expires by then too, so the note about it never outlives it.
const NOTE_TTL_MS: u64 = TICKET_TTL.as_secs() * 1000;

/// What the person chose in the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Copy a new pairing ticket and wait for the other machine.
    Pair,
    /// Pair with the ticket on the clipboard.
    PairCopied,
    TogglePause,
    /// Check for updates, or install the one a check found.
    Update,
    Quit,
}

/// What the menu shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    /// Peers connected, and whether sync is paused. Also the icon's tooltip.
    pub status: String,
    /// A note about pairing, or else the last clip.
    pub activity: String,
    /// False while a pairing runs: there is one at a time.
    pub can_pair: bool,
    pub paused: bool,
    /// Whether clips are flowing, which picks the icon.
    pub syncing: bool,
    pub update: UpdateItem,
}

/// The menu item that checks for updates and installs them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateItem {
    pub text: String,
    pub enabled: bool,
}

impl View {
    /// What shows until the daemon reports for the first time.
    pub fn starting() -> Self {
        Self {
            status: "Starting...".to_owned(),
            activity: "No clips yet".to_owned(),
            can_pair: false,
            paused: false,
            syncing: false,
            update: UpdateItem {
                text: format!("Check for updates ({VERSION})"),
                enabled: false,
            },
        }
    }

    fn new(
        status: &DaemonStatus,
        note: Option<&str>,
        pairing: bool,
        update: State,
        now_ms: u64,
    ) -> Self {
        let connected = status.peers.iter().filter(|peer| peer.connected).count();
        let peers = match status.peers.len() {
            0 => "No machines paired yet".to_owned(),
            1 => format!("{connected} of 1 machine connected"),
            total => format!("{connected} of {total} machines connected"),
        };
        let (text, enabled) = match update {
            State::Idle => (format!("Check for updates ({VERSION})"), true),
            State::Checking => ("Checking for updates...".to_owned(), false),
            // A restart would resume sync, and pairing would be cut off.
            State::Available(_) if status.paused => ("Resume sync to update".to_owned(), false),
            State::Available(version) => (format!("Update to {version} and restart"), !pairing),
            State::Installing(version) => (format!("Updating to {version}..."), false),
        };
        Self {
            status: if status.paused {
                format!("Sync paused. {peers}")
            } else {
                peers
            },
            activity: note.map_or_else(
                || {
                    status
                        .last_clip(now_ms)
                        .unwrap_or_else(|| "No clips yet".to_owned())
                },
                ToOwned::to_owned,
            ),
            can_pair: !pairing,
            paused: status.paused,
            syncing: connected > 0 && !status.paused,
            update: UpdateItem { text, enabled },
        }
    }
}

/// The daemon's end of the menu.
#[derive(Debug)]
pub struct Menu {
    actions: UnboundedReceiver<Action>,
    views: Sender<View>,
    /// The text and when it expires, in milliseconds since the Unix epoch.
    note: Option<(String, u64)>,
    shown: Option<View>,
}

/// The icon's end of the menu.
#[derive(Debug)]
pub struct Gui {
    pub actions: UnboundedSender<Action>,
    pub views: Receiver<View>,
}

pub fn channel() -> (Menu, Gui) {
    let (action_tx, actions) = mpsc::unbounded_channel();
    let (views_tx, views) = std_mpsc::channel();
    let menu = Menu {
        actions,
        views: views_tx,
        note: None,
        shown: None,
    };
    let gui = Gui {
        actions: action_tx,
        views,
    };
    (menu, gui)
}

impl Menu {
    /// The next action, or `None` once there is no icon to choose one from.
    pub async fn next_action(&mut self) -> Option<Action> {
        self.actions.recv().await
    }

    /// Shows `text` in place of the last clip for a while. [`Self::show`] sends it.
    pub fn note(&mut self, text: impl Into<String>, now_ms: u64) {
        self.note = Some((text.into(), now_ms.saturating_add(NOTE_TTL_MS)));
    }

    /// Sends what the menu should show, if it changed.
    pub fn show(&mut self, status: &DaemonStatus, pairing: bool, update: State, now_ms: u64) {
        self.note.take_if(|(_, until_ms)| *until_ms <= now_ms);
        let note = self.note.as_ref().map(|(text, _)| text.as_str());
        let view = View::new(status, note, pairing, update, now_ms);
        if self.shown.as_ref() != Some(&view) {
            // Fails if there is no icon, and then nobody is looking.
            let _ = self.views.send(view.clone());
            self.shown = Some(view);
        }
    }
}

#[cfg(test)]
mod tests;
