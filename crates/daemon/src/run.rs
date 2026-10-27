//! `clipbroad run`: the sync daemon. Wires the clipboard, the sync engine and the network together.

use std::future::{self, Future};
use std::io;
use std::path::PathBuf;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use clipbroad_clipboard::fake::FakeClipboard;
use clipbroad_clipboard::{Clipboard, ClipboardError};
use clipbroad_config::{Config, IdentityKey, Paths};
use clipbroad_core::protocol::{ContentKind, NodeId, Payload};
use clipbroad_core::sync::{LocalAction, RemoteAction, SyncEngine};
use clipbroad_core::{ClipContent, ContentHash, Png};
use clipbroad_net::{FetchError, FileCache, Node, Offer, Received};
use iroh::EndpointId;
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
    sync(clipboard, &config, &identity, &paths, shutdown_signal()).await
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

/// Received files on their way to the cache. The main loop polls it next to everything else, so
/// a big download blocks no clip or local copy; dropping it cancels the download.
type Download<'a> = Pin<Box<dyn Future<Output = Downloaded> + Send + 'a>>;

/// The end of a [`Download`]: the cache paths of the files, or why they could not be fetched.
struct Downloaded {
    from: EndpointId,
    size: u64,
    hash: String,
    paths: Result<Vec<PathBuf>, FetchError>,
}

/// Syncs `clipboard` with the peers in `config` until `shutdown` completes, keeping the status
/// file up to date for `clipbroad status` (see [`status`]) and received files in the file cache.
///
/// Errors from a single read, write or clip are logged and the loop keeps going; only a failed
/// setup or a failed clipboard watcher end it.
pub async fn sync(
    clipboard: Arc<dyn Clipboard>,
    config: &Config,
    identity: &IdentityKey,
    paths: &Paths,
    shutdown: impl Future<Output = io::Result<()>>,
) -> Result<()> {
    let mut changes = clipboard.watch().context("cannot watch the clipboard")?;
    let cache = FileCache::open(paths.file_cache_dir.clone(), config).with_context(|| {
        format!(
            "cannot open the file cache at {}",
            paths.file_cache_dir.display()
        )
    })?;
    let (node, mut inbox) = Node::spawn(identity, config)
        .await
        .context("cannot start the network node")?;
    info!(
        node_id = %node.id(),
        port = %config.listen_port,
        "node started; other machines list this id in their config's [[peers]]"
    );
    let mut engine = SyncEngine::new(NodeId::from_bytes(*node.id().as_bytes()));
    let status_file = paths.status_file();
    // At most one: a newer clip, from here or a peer, replaces what is being downloaded.
    let mut download: Option<Download<'_>> = None;
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
                match local_change(&clipboard, &mut engine, &node, config).await {
                    Ok(Some(sent)) => {
                        // The local copy is newer than whatever was downloading.
                        download = None;
                        status.last_sent = Some(sent);
                    }
                    Ok(None) => {}
                    Err(error) => warn!("{error:#}"),
                }
            }
            // `None` once no peer task is left to send clips, as with no peers configured.
            Some(received) = inbox.recv() => {
                let files = match &received.clip.payload {
                    Payload::Files(entries) => Some(entries.len()),
                    Payload::Inline(_) => None,
                };
                status.last_received = Some(ClipEvent {
                    at_ms: now_ms(),
                    kind: received.clip.kind,
                    size: received.clip.size,
                    files,
                    hash: short_hash(received.clip.hash),
                    from: Some(received.from),
                });
                let remote = remote_clip(&clipboard, &mut engine, &node, &cache, received, &mut download);
                if let Err(error) = remote.await {
                    warn!("{error:#}");
                }
            }
            downloaded = finished(&mut download) => {
                download = None;
                if let Err(error) = apply_files(&clipboard, downloaded).await {
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
        status.save(&status_file);
    };
    // It borrows the node.
    drop(download);
    node.shutdown().await;
    status::remove(&status_file);
    result
}

/// Completes with the end of `download`, or never if there is none.
async fn finished(download: &mut Option<Download<'_>>) -> Downloaded {
    match download {
        Some(download) => download.await,
        None => future::pending().await,
    }
}

/// Reads the local clipboard after a change and sends it to the peers if it is new. Returns what
/// was sent, if anything.
async fn local_change(
    clipboard: &Arc<dyn Clipboard>,
    engine: &mut SyncEngine,
    node: &Node,
    config: &Config,
) -> Result<Option<ClipEvent>> {
    let clipboard = Arc::clone(clipboard);
    // Reads may block (up to 2 s each by contract), so keep them off the async thread.
    let copied = match tokio::task::spawn_blocking(move || read_content(&*clipboard)).await? {
        Ok(Some(copied)) => copied,
        Ok(None) => {
            debug!("clipboard holds no files, text or PNG image, nothing to send");
            return Ok(None);
        }
        // Expected for every password manager copy: not a problem worth a warning.
        Err(ClipboardError::Sensitive { marker }) => {
            debug!(
                marker,
                "clipboard content is marked as sensitive, not synced"
            );
            return Ok(None);
        }
        Err(error) => return Err(error).context("cannot read the clipboard"),
    };
    let (content, offer) = match copied {
        Copied::Files(paths) => {
            let config = config.clone();
            // Hashes every file, so it takes a while for big ones.
            let offer = tokio::task::spawn_blocking(move || Offer::read(&paths, &config))
                .await?
                .context("cannot read the copied files")?;
            // `Offer::read` logged why each file was skipped.
            let Some(offer) = offer else {
                return Ok(None);
            };
            (ClipContent::Files(offer.list().clone()), Some(offer))
        }
        Copied::Text(text) => (ClipContent::Text(text), None),
        Copied::Image(png) => (ClipContent::Image(png), None),
    };
    let (kind, size, files) = match &content {
        ClipContent::Text(text) => (ContentKind::Text, text.len() as u64, None),
        ClipContent::Image(png) => (ContentKind::Image, png.as_bytes().len() as u64, None),
        ClipContent::Files(list) => (
            ContentKind::Files,
            list.total_size(),
            Some(list.entries().len()),
        ),
    };
    // Files travel apart from the clip, with their own limits (checked by `Offer::read`).
    let inline_max_bytes = config.inline_max_bytes.get();
    if offer.is_none() && size > inline_max_bytes {
        warn!(
            ?kind,
            size, inline_max_bytes, "copied content is larger than inline_max_bytes, not synced"
        );
        return Ok(None);
    }
    let LocalAction::Send(clip) = engine.local_changed(content, now_ms()) else {
        debug!(size, "clipboard already synced, nothing to send");
        return Ok(None);
    };
    let hash = short_hash(clip.hash);
    match offer {
        Some(offer) => node.broadcast_files(clip, offer),
        None => node.broadcast(clip),
    }
    .context("cannot send the clip")?;
    info!(?kind, size, files, %hash, "sent clip");
    Ok(Some(ClipEvent {
        at_ms: now_ms(),
        kind,
        size,
        files,
        hash,
        from: None,
    }))
}

/// Applies a clip from a peer to the local clipboard if it is newer than what this node has.
/// Text and images are written right away. Files are fetched first: the clip replaces
/// `download`, and [`apply_files`] writes their paths once it ends.
async fn remote_clip<'d>(
    clipboard: &Arc<dyn Clipboard>,
    engine: &mut SyncEngine,
    node: &'d Node,
    cache: &'d FileCache,
    received: Received,
    download: &mut Option<Download<'d>>,
) -> Result<()> {
    let from = received.from;
    let (kind, size) = (received.clip.kind, received.clip.size);
    let hash = short_hash(received.clip.hash);
    info!(from = %from.fmt_short(), ?kind, size, %hash, "received clip");
    let action = engine
        .remote_clip(received.clip)
        .with_context(|| format!("invalid clip from {}", from.fmt_short()))?;
    let content = match action {
        RemoteAction::Apply(content) => content,
        RemoteAction::AlreadyHave => {
            debug!(%hash, "clipboard already holds this clip");
            return Ok(());
        }
        RemoteAction::Stale => {
            debug!(%hash, "clip is older than the current one, ignored");
            return Ok(());
        }
    };
    // Whatever was downloading is older than this clip.
    *download = None;
    let clipboard = Arc::clone(clipboard);
    let written = match content {
        ClipContent::Text(text) => {
            tokio::task::spawn_blocking(move || clipboard.write_text(&text)).await?
        }
        ClipContent::Image(png) => {
            tokio::task::spawn_blocking(move || clipboard.write_image(&png)).await?
        }
        ClipContent::Files(files) => {
            debug!(from = %from.fmt_short(), count = files.entries().len(), size, %hash, "downloading files");
            *download = Some(Box::pin(async move {
                let paths = node.fetch_files(from, &files, cache).await;
                Downloaded {
                    from,
                    size,
                    hash,
                    paths,
                }
            }));
            return Ok(());
        }
    };
    report_write(written, from, kind, size, &hash)
}

/// Puts the files of a finished download on the clipboard. Only called for the newest clip: a
/// newer one drops the download before it ends.
async fn apply_files(clipboard: &Arc<dyn Clipboard>, downloaded: Downloaded) -> Result<()> {
    let Downloaded {
        from,
        size,
        hash,
        paths,
    } = downloaded;
    let paths = paths.with_context(|| {
        format!(
            "cannot fetch the files of clip {hash} from {}",
            from.fmt_short()
        )
    })?;
    let clipboard = Arc::clone(clipboard);
    let written = tokio::task::spawn_blocking(move || clipboard.write_files(&paths)).await?;
    report_write(written, from, ContentKind::Files, size, &hash)
}

/// Logs the outcome of writing a clip from `from` to the clipboard.
fn report_write(
    written: Result<(), ClipboardError>,
    from: EndpointId,
    kind: ContentKind,
    size: u64,
    hash: &str,
) -> Result<()> {
    let from = from.fmt_short();
    match written {
        Ok(()) => info!(%from, ?kind, size, %hash, "applied clip"),
        // A backend without support for this kind of content (the trait's default).
        Err(ClipboardError::Unsupported { content }) => warn!(
            %from, ?kind, size, %hash,
            "this machine's clipboard does not support {content} yet, clip not applied"
        ),
        Err(error) => return Err(error).context("cannot write the clip to the clipboard"),
    }
    Ok(())
}

/// What a local copy holds. Not `Debug`, so its content cannot end up in the logs.
pub enum Copied {
    /// Local paths, as the clipboard lists them. They may be folders or missing files.
    Files(Vec<PathBuf>),
    Text(String),
    Image(Png),
}

/// What a local copy holds: its files, else its text, else its image.
///
/// Files win: a file manager's copy also offers the paths as text, but the files are what the
/// person copied. Backends only report files when every reference is a local file, so a copied
/// web link stays text.
///
/// Text wins over an image when a copy offers both: apps that offer both for one copy (a
/// spreadsheet's cells, an office document) mostly do it so the text can also be pasted as a
/// picture, and the text is what the person meant and is much smaller. A browser's "Copy image"
/// offers the image with `<img>` markup but no plain text; backends do not report that markup as
/// text, so the image wins there.
pub fn read_content(clipboard: &dyn Clipboard) -> Result<Option<Copied>, ClipboardError> {
    if let Some(paths) = clipboard.read_files()? {
        return Ok(Some(Copied::Files(paths)));
    }
    if let Some(text) = clipboard.read_text()? {
        return Ok(Some(Copied::Text(text)));
    }
    Ok(clipboard.read_image()?.map(Copied::Image))
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
