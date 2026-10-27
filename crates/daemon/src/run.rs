//! `clipbroad run`: the sync daemon. Wires the clipboard, the sync engine and the network together.

use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use clipbroad_clipboard::fake::FakeClipboard;
use clipbroad_clipboard::{Clipboard, ClipboardError};
use clipbroad_config::{Config, IdentityKey, Paths};
use clipbroad_core::protocol::NodeId;
use clipbroad_core::sync::{LocalAction, RemoteAction, SyncEngine};
use clipbroad_core::{ClipContent, ContentHash};
use clipbroad_net::{Node, Received};
use tracing::{debug, info, warn};

use crate::backend;

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
        Some(config_file) => Paths {
            secret_key_file: config_file.with_file_name("secret.key"),
            config_file,
        },
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
    sync(clipboard, &config, &identity, shutdown_signal()).await
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

/// Syncs `clipboard` with the peers in `config` until `shutdown` completes.
///
/// Errors from a single read, write or clip are logged and the loop keeps going; only a failed
/// setup or a failed clipboard watcher end it.
pub async fn sync(
    clipboard: Arc<dyn Clipboard>,
    config: &Config,
    identity: &IdentityKey,
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
                if let Err(error) =
                    local_change(&clipboard, &mut engine, &node, inline_max_bytes).await
                {
                    warn!("{error:#}");
                }
            }
            // `None` once no peer task is left to send clips, as with no peers configured.
            Some(received) = inbox.recv() => {
                if let Err(error) = remote_clip(&clipboard, &mut engine, received).await {
                    warn!("{error:#}");
                }
            }
            result = &mut shutdown => {
                info!("shutting down");
                break result.context("cannot listen for the shutdown signal");
            }
        }
    };
    node.shutdown().await;
    result
}

/// Reads the local clipboard after a change and sends it to the peers if it is new.
async fn local_change(
    clipboard: &Arc<dyn Clipboard>,
    engine: &mut SyncEngine,
    node: &Node,
    inline_max_bytes: u64,
) -> Result<()> {
    let clipboard = Arc::clone(clipboard);
    // `read_text` may block (up to 2 s by contract), so keep it off the async thread.
    let text = match tokio::task::spawn_blocking(move || clipboard.read_text()).await? {
        Ok(text) => text,
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
    let Some(text) = text else {
        debug!("clipboard holds no text, nothing to send");
        return Ok(());
    };
    let size = text.len() as u64;
    if size > inline_max_bytes {
        warn!(
            size,
            inline_max_bytes, "copied text is larger than inline_max_bytes, not synced"
        );
        return Ok(());
    }
    match engine.local_changed(ClipContent::from(text), now_ms()) {
        LocalAction::Send(clip) => {
            let hash = short_hash(clip.hash);
            node.broadcast(clip).context("cannot send the clip")?;
            info!(size, %hash, "sent clip");
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
    let (size, hash) = (received.clip.size, short_hash(received.clip.hash));
    info!(%from, size, %hash, "received clip");
    let action = engine
        .remote_clip(received.clip)
        .with_context(|| format!("invalid clip from {from}"))?;
    match action {
        RemoteAction::Apply(content) => {
            let ClipContent::Text(text) = content;
            let clipboard = Arc::clone(clipboard);
            tokio::task::spawn_blocking(move || clipboard.write_text(&text))
                .await?
                .context("cannot write the clip to the clipboard")?;
            info!(%from, size, %hash, "applied clip");
        }
        RemoteAction::AlreadyHave => debug!(%hash, "clipboard already holds this clip"),
        RemoteAction::Stale => debug!(%hash, "clip is older than the current one, ignored"),
    }
    Ok(())
}

/// The first 8 hex characters: enough to match clips across machines' logs.
fn short_hash(hash: ContentHash) -> String {
    let mut hex = hash.to_string();
    hex.truncate(8);
    hex
}

/// Milliseconds since the Unix epoch on the local clock (0 if the clock is before 1970).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests;
