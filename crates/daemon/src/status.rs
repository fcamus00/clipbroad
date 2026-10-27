//! `clipbroad status`: what the running daemon is doing, read from the status file it keeps.
//!
//! `clipbroad run` writes a small TOML file (`status.toml` beside the identity, see
//! [`Paths::status_file`]) whenever something changes and every [`HEARTBEAT`], and removes it when
//! it stops. A file instead of a local socket or named pipe: the code is the same on every OS, the
//! daemon runs no server, and a daemon that died shows up as a file that stopped being updated.
//! The file is replaced atomically (written aside, then renamed), so a reader never sees half of
//! it.

use std::fmt::Write as _;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};
use std::{fs, process};

use anyhow::{Context, Result};
use clipbroad_config::{Config, Paths};
use clipbroad_core::protocol::ContentKind;
use clipbroad_net::{Link, Links, Node};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::backend;
use crate::run::now_ms;
use crate::watch::utc_time;

/// How often the daemon rewrites the file when nothing changes.
pub const HEARTBEAT: Duration = Duration::from_secs(5);
/// A file older than this was left by a daemon that did not stop cleanly (killed, crashed, or
/// the machine lost power).
const STALE_AFTER: Duration = Duration::from_secs(15);
/// Width of the label column in the output.
const LABEL_WIDTH: usize = 15;

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Config file of the daemon to check, as given to `run --config`.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
}

pub fn run(args: Args) -> Result<()> {
    let paths = match args.config {
        Some(config_file) => Paths::beside_config(config_file),
        None => Paths::discover()?,
    };
    let status = read(&paths.status_file())?;
    print!("{}", render(status.as_ref(), now_ms()));
    Ok(())
}

/// What the daemon reports about itself. Never holds clipboard content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub pid: u32,
    /// When the daemon last wrote the file, in milliseconds since the Unix epoch.
    pub updated_ms: u64,
    pub node_id: EndpointId,
    pub port: u16,
    pub mdns: Mdns,
    /// Only on macOS 15.4 and later.
    pub paste_access: Option<PasteAccess>,
    pub last_sent: Option<ClipEvent>,
    pub last_received: Option<ClipEvent>,
    /// In config order.
    pub peers: Vec<PeerStatus>,
}

/// Whether LAN discovery runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mdns {
    On,
    /// `mdns = false` in the config.
    Off,
    /// On in the config, but it could not start.
    Failed,
}

/// The pasteboard access macOS grants the daemon (System Settings > Privacy & Security > Paste
/// from Other Apps). Mirrors `clipbroad_clipboard::macos::AccessBehavior`, which only exists on
/// macOS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PasteAccess {
    Default,
    Ask,
    AlwaysAllow,
    AlwaysDeny,
    Other(isize),
}

/// A clip that was sent or received: when, what kind, how big, and its short hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipEvent {
    pub at_ms: u64,
    pub kind: ContentKind,
    pub size: u64,
    pub hash: String,
    /// Who sent it, for a received clip.
    pub from: Option<EndpointId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerStatus {
    pub id: EndpointId,
    pub name: Option<String>,
    pub connected: bool,
    /// The address the connection uses, if known.
    pub addr: Option<SocketAddr>,
}

impl DaemonStatus {
    /// The status of a daemon that just started `node` from `config`: no clips, no peer
    /// connected yet.
    pub fn new(node: &Node, config: &Config) -> Self {
        let mdns = match (config.mdns, node.lan_discovery()) {
            (true, true) => Mdns::On,
            (true, false) => Mdns::Failed,
            (false, _) => Mdns::Off,
        };
        let peers = config.peers.iter().map(|peer| PeerStatus {
            id: peer.id,
            name: peer.name.clone(),
            connected: false,
            addr: None,
        });
        Self {
            pid: process::id(),
            updated_ms: 0,
            node_id: node.id(),
            port: config.listen_port.get(),
            mdns,
            paste_access: None,
            last_sent: None,
            last_received: None,
            peers: peers.collect(),
        }
    }

    pub fn update_links(&mut self, links: &Links) {
        for peer in &mut self.peers {
            (peer.connected, peer.addr) = match links.get(&peer.id) {
                Some(Link::Connected { addr }) => (true, *addr),
                Some(Link::Disconnected) | None => (false, None),
            };
        }
    }

    /// Refreshes the time and the macOS pasteboard access, then replaces the file at `path`. A
    /// failure is logged: the daemon keeps syncing without it.
    pub fn save(&mut self, path: &Path) {
        self.updated_ms = now_ms();
        self.paste_access = backend::paste_access();
        if let Err(error) = self.try_save(path) {
            warn!("cannot update the status file: {error:#}");
        }
    }

    fn try_save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string(self)?;
        let temp = path.with_extension("toml.tmp");
        fs::write(&temp, text).with_context(|| format!("cannot write {}", temp.display()))?;
        fs::rename(&temp, path).with_context(|| format!("cannot replace {}", path.display()))
    }
}

/// Removes the status file when the daemon stops.
pub fn remove(path: &Path) {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            warn!("cannot remove {}: {error}", path.display());
        }
        _ => {}
    }
}

/// The status in the file at `path`, or `None` if there is no file (the daemon is not running).
pub fn read(path: &Path) -> Result<Option<DaemonStatus>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", path.display())),
    };
    let status = toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
    Ok(Some(status))
}

/// The text `clipbroad status` prints, one aligned line per item.
pub fn render(status: Option<&DaemonStatus>, now_ms: u64) -> String {
    let mut out = String::new();
    let mut line = |label: &str, value: &str| {
        // Writing to a String never fails.
        let _ = writeln!(out, "{label:<LABEL_WIDTH$}{value}");
    };
    let stale_after_ms = u64::try_from(STALE_AFTER.as_millis()).unwrap_or(u64::MAX);
    let Some(status) = status.filter(|s| now_ms.saturating_sub(s.updated_ms) <= stale_after_ms)
    else {
        let detail = status.map_or(String::new(), |s| {
            format!(
                ": it did not stop cleanly, last seen at {}",
                time(s.updated_ms, now_ms)
            )
        });
        line("daemon", &format!("not running{detail}"));
        line(
            "",
            "Start it with `clipbroad run`, or check `clipbroad service status`.",
        );
        return out;
    };

    line("daemon", &format!("running (pid {})", status.pid));
    line("node id", &status.node_id.to_string());
    line("port", &format!("UDP {}", status.port));
    line(
        "LAN discovery",
        match status.mdns {
            Mdns::On => "on (mDNS)",
            Mdns::Off => "off (mdns = false in the config)",
            Mdns::Failed => "off: mDNS could not start, see the daemon's logs",
        },
    );
    if status.peers.is_empty() {
        line("peers", "none yet; add one with `clipbroad pair`");
    } else {
        let connected = status.peers.iter().filter(|p| p.connected).count();
        line(
            "peers",
            &format!("{connected} of {} connected", status.peers.len()),
        );
        let names: Vec<&str> = status.peers.iter().map(peer_name).collect();
        let width = names.iter().map(|name| name.chars().count()).max();
        for (peer, name) in status.peers.iter().zip(names) {
            let state = match (peer.connected, peer.addr) {
                (true, Some(addr)) => format!("connected via {addr}"),
                (true, None) => "connected".to_owned(),
                (false, _) => "not connected".to_owned(),
            };
            let id = peer.id.fmt_short();
            line(
                "",
                &format!("{name:<w$}  {id}  {state}", w = width.unwrap_or(0)),
            );
        }
    }
    let clip = |event: Option<&ClipEvent>| {
        let Some(event) = event else {
            return "nothing yet".to_owned();
        };
        let from = event.from.map_or(String::new(), |id| {
            let peer = status.peers.iter().find(|p| p.id == id);
            format!(", from {}", peer.map_or("an unknown peer", peer_name))
        });
        let kind = match event.kind {
            ContentKind::Text => "text",
            ContentKind::Image => "image",
        };
        format!(
            "{}, {kind}, {} bytes, hash {}{from}",
            time(event.at_ms, now_ms),
            event.size,
            event.hash
        )
    };
    line("last sent", &clip(status.last_sent.as_ref()));
    line("last received", &clip(status.last_received.as_ref()));
    if let Some(access) = status.paste_access {
        let (state, needs_fix) = match access {
            PasteAccess::AlwaysAllow => ("allowed".to_owned(), false),
            PasteAccess::Ask => ("macOS asks before each read".to_owned(), true),
            PasteAccess::Default => ("macOS will ask on the first read".to_owned(), true),
            PasteAccess::AlwaysDeny => ("denied: copies here are not synced".to_owned(), true),
            PasteAccess::Other(value) => (format!("unknown macOS setting ({value})"), true),
        };
        line("paste access", &state);
        if needs_fix {
            line(
                "",
                "To sync without prompts, open System Settings > Privacy & Security > Paste \
                 from Other Apps and set clipbroad to Allow.",
            );
        }
    }
    out
}

fn peer_name(peer: &PeerStatus) -> &str {
    peer.name.as_deref().unwrap_or("(no name)")
}

/// `HH:MM:SS UTC (5 min ago)`.
fn time(at_ms: u64, now_ms: u64) -> String {
    let secs = now_ms.saturating_sub(at_ms) / 1000;
    let ago = match secs {
        0..60 => format!("{secs} s"),
        60..3600 => format!("{} min", secs / 60),
        3600..86_400 => format!("{} h", secs / 3600),
        _ => format!("{} days", secs / 86_400),
    };
    let at = UNIX_EPOCH + Duration::from_millis(at_ms);
    format!("{} ({ago} ago)", utc_time(at))
}

#[cfg(test)]
mod tests;
