//! `clipbroad run`: the sync daemon. Wires the clipboard, the sync engine and the network together.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use clipbroad_clipboard::fake::FakeClipboard;
use clipbroad_clipboard::{Clipboard, ClipboardError};
use clipbroad_config::{Config, IdentityKey, Paths};
use clipbroad_core::protocol::{ContentKind, NodeId};
use clipbroad_core::sync::{LocalAction, RemoteAction, SyncEngine};
use clipbroad_core::{ClipContent, ContentHash};
use clipbroad_net::{Node, Received};
use tokio::time::{self, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::backend;
use crate::status::{self, ClipEvent, DaemonStatus};

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Config file to use instead of the default one. The identity is then read from (or created
    /// as) `secret.key` in the same directory, so give each instance its own directory.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Use an in-memory clipboard that nobody copies to: it only receives clips. For tests.
    #[arg(long, hide = true)]
    fake: bool,
}

pub async fn run(args: Args) -> Result<()> {
    let paths = match args.config {
        Some(config_file) => Paths::beside_config(config_file),
        None => Paths::discover()?,
    };
    let config = Config::load_or_default(&paths.config_file)?;
    let identity = IdentityKey::load_or_generate(&paths.secret_key_file)?;
    let clipboard: Arc<dyn Clipboard> = if args.fake {
        Arc::new(FakeClipboard::new())
    } else {
        backend::system_clipboard()?
    };
    info!(config = %paths.config_file.display(), peers = config.peers.len(), "starting");
    let status_file = paths.status_file();
    sync(
        clipboard,
        &config,
        &identity,
        &status_file,
        shutdown_signal(),
    )
    .await
}

/// Ctrl+C, or SIGTERM on Unix (what systemd and `kill` send).
async fn shutdown_signal() -> io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}

/// Syncs `clipboard` with the peers in `config` until `shutdown` completes, keeping
/// `status_file` up to date for `clipbroad status` (see [`status`]).
///
/// Errors from a single read, write or clip are logged and the loop keeps going; only a failed
/// setup or a failed clipboard watcher end it.
pub async fn sync(
    clipboard: Arc<dyn Clipboard>,
    config: &Config,
    identity: &IdentityKey,
    status_file: &Path,
    shutdown: impl Future<Output = io::Result<()>>,
) -> Result<()> {
    let mut changes = clipboard.watch().context("cannot watch the clipboard")?;
    let (node, mut inbox) = Node::spawn(identity, config)
        .await
        .context("cannot start the network node")?;
    info!(
        node_id = %node.id(),
        port = %config.listen_port,
        "node started; other machines list this id in their config's [[peers]]"
    );
    let mut engine = SyncEngine::new(NodeId::from_bytes(*node.id().as_bytes()));
    let inline_max_bytes = config.inline_max_bytes.get();
    // Written only once the node is up: a second daemon on the same config fails before this,
    // and must not take over the first one's file.
    let mut status = DaemonStatus::new(&node, config);
    let mut links = node.links();
    let mut heartbeat = time::interval(status::HEARTBEAT);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut shutdown = pin!(shutdown);
    let result = loop {
        tokio::select! {
            change = changes.recv() => {
                let change = match change {
                    Some(Ok(change)) => change,
                    Some(Err(error)) => break Err(error).context("the clipboard watcher failed"),
                    None => break Err(anyhow!("the clipboard watcher stopped")),
                };
                // No formats means nothing to sync (empty clipboard, or formats unknown).
                if change.formats.is_empty() {
                    continue;
                }
                let sent = &mut status.last_sent;
                if let Err(error) =
                    local_change(&clipboard, &mut engine, &node, inline_max_bytes, sent).await
                {
                    warn!("{error:#}");
                }
            }
            // `None` once no peer task is left to send clips, as with no peers configured.
            Some(received) = inbox.recv() => {
                status.last_received = Some(ClipEvent {
                    at_ms: now_ms(),
                    kind: received.clip.kind,
                    size: received.clip.size,
                    hash: short_hash(received.clip.hash),
                    from: Some(received.from),
                });
                if let Err(error) = remote_clip(&clipboard, &mut engine, received).await {
                    warn!("{error:#}");
                }
            }
            // The node holds the sender, so this never ends while it runs.
            Ok(()) = links.changed() => status.update_links(&links.borrow_and_update()),
            _ = heartbeat.tick() => {}
            result = &mut shutdown => {
                info!("shutting down");
                break result.context("cannot listen for the shutdown signal");
            }
        }
        status.save(status_file);
    };
    node.shutdown().await;
    status::remove(status_file);
    result
}

/// Reads the local clipboard after a change and sends it to the peers if it is new.
async fn local_change(
    clipboard: &Arc<dyn Clipboard>,
    engine: &mut SyncEngine,
    node: &Node,
    inline_max_bytes: u64,
    last_sent: &mut Option<ClipEvent>,
) -> Result<()> {
    let clipboard = Arc::clone(clipboard);
    // Reads may block (up to 2 s each by contract), so keep them off the async thread.
    let content = match tokio::task::spawn_blocking(move || read_content(&*clipboard)).await? {
        Ok(content) => content,
        // Expected for every password manager copy: not a problem worth a warning.
        Err(ClipboardError::Sensitive { marker }) => {
            debug!(
                marker,
                "clipboard content is marked as sensitive, not synced"
            );
            return Ok(());
        }
        Err(error) => return Err(error).context("cannot read the clipboard"),
    };
    let Some(content) = content else {
        debug!("clipboard holds no text or PNG image, nothing to send");
        return Ok(());
    };
    let (kind, size) = match &content {
        ClipContent::Text(text) => (ContentKind::Text, text.len() as u64),
        ClipContent::Image(png) => (ContentKind::Image, png.as_bytes().len() as u64),
        // `read_content` does not read file lists yet.
        ClipContent::Files(files) => (ContentKind::Files, files.total_size()),
    };
    if size > inline_max_bytes {
        warn!(
            ?kind,
            size, inline_max_bytes, "copied content is larger than inline_max_bytes, not synced"
        );
        return Ok(());
    }
    match engine.local_changed(content, now_ms()) {
        LocalAction::Send(clip) => {
            let hash = short_hash(clip.hash);
            node.broadcast(clip).context("cannot send the clip")?;
            info!(?kind, size, %hash, "sent clip");
            *last_sent = Some(ClipEvent {
                at_ms: now_ms(),
                kind,
                size,
                hash,
                from: None,
            });
        }
        LocalAction::Unchanged => debug!(size, "clipboard already synced, nothing to send"),
    }
    Ok(())
}

/// Applies a clip from a peer to the local clipboard if it is newer than what this node has.
async fn remote_clip(
    clipboard: &Arc<dyn Clipboard>,
    engine: &mut SyncEngine,
    received: Received,
) -> Result<()> {
    let from = received.from.fmt_short();
    let (kind, size) = (received.clip.kind, received.clip.size);
    let hash = short_hash(received.clip.hash);
    info!(%from, ?kind, size, %hash, "received clip");
    let action = engine
        .remote_clip(received.clip)
        .with_context(|| format!("invalid clip from {from}"))?;
    match action {
        RemoteAction::Apply(content) => {
            let clipboard = Arc::clone(clipboard);
            let written = match content {
                ClipContent::Text(text) => {
                    tokio::task::spawn_blocking(move || clipboard.write_text(&text)).await?
                }
                ClipContent::Image(png) => {
                    tokio::task::spawn_blocking(move || clipboard.write_image(&png)).await?
                }
                // Fetching the files and putting their paths on the clipboard comes with the
                // clipboard backends for files.
                ClipContent::Files(_) => {
                    warn!(%from, ?kind, size, %hash, "file clips are not applied yet, clip not applied");
                    return Ok(());
                }
            };
            match written {
                Ok(()) => info!(%from, ?kind, size, %hash, "applied clip"),
                // macOS and Windows do not write images yet.
                Err(ClipboardError::Unsupported { content }) => warn!(
                    %from, ?kind, size, %hash,
                    "this machine's clipboard does not support {content} yet, clip not applied"
                ),
                Err(error) => return Err(error).context("cannot write the clip to the clipboard"),
            }
        }
        RemoteAction::AlreadyHave => debug!(%hash, "clipboard already holds this clip"),
        RemoteAction::Stale => debug!(%hash, "clip is older than the current one, ignored"),
    }
    Ok(())
}

/// What a local copy holds: its text, or its image if it has no text.
///
/// Text wins when a copy offers both: apps that offer both for one copy (a spreadsheet's cells,
/// an office document) mostly do it so the text can also be pasted as a picture, and the text is
/// what the person meant and is much smaller. A browser's "Copy image" offers the image with
/// `<img>` markup but no plain text; backends do not report that markup as text, so the image
/// wins there.
pub fn read_content(clipboard: &dyn Clipboard) -> Result<Option<ClipContent>, ClipboardError> {
    if let Some(text) = clipboard.read_text()? {
        return Ok(Some(ClipContent::Text(text)));
    }
    Ok(clipboard.read_image()?.map(ClipContent::Image))
}

/// The first 8 hex characters: enough to match clips across machines' logs.
fn short_hash(hash: ContentHash) -> String {
    let mut hex = hash.to_string();
    hex.truncate(8);
    hex
}

/// Milliseconds since the Unix epoch on the local clock (0 if the clock is before 1970).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests;
