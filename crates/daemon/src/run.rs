//! `clipbroad run`: the sync daemon. Wires the clipboard, the sync engine and the network together.

use std::fs;
use std::future::{self, Future};
use std::io;
use std::path::{Path, PathBuf};
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use clipbroad_clipboard::fake::FakeClipboard;
use clipbroad_clipboard::{Clipboard, ClipboardContent, ClipboardError};
use clipbroad_config::{Config, IdentityKey, Paths, Peer};
use clipbroad_core::protocol::{ContentKind, NodeId, Payload};
use clipbroad_core::sync::{LocalAction, RemoteAction, SyncEngine};
use clipbroad_core::{ClipContent, ContentHash};
use clipbroad_net::pair::Ticket;
use clipbroad_net::{FetchError, FileCache, Node, Offer, Received};
use iroh::{EndpointId, SecretKey};
use tokio::time::{self, Instant, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::control::Served;
use crate::menu::{self, Action, Menu};
use crate::mesh::{self, Mesh};
use crate::status::{self, ClipEvent, DaemonStatus};
use crate::update::{self, Installation, Updater, Updates};
use crate::{backend, control, pair, reset, tray};

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

/// How the daemon ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Quit,
    /// The binary changed: it should run again.
    Restart,
}

/// Runs the daemon with the tray icon, or without it for `--fake` (tests).
pub fn start(args: Args) -> Result<()> {
    // Here, because an update changes what the OS reports. An error matters only for updates.
    let installation = Installation::current()
        .inspect_err(|error| warn!("updates are off: {error:#}"))
        .ok();
    if let Some(installation) = &installation {
        installation.remove_replaced();
    }
    let exit = if args.fake {
        let (menu, _no_icon) = menu::channel();
        crate::runtime()?.block_on(run(args, menu, None))?
    } else {
        let updater = installation.clone().and_then(|installation| {
            Updater::new(installation)
                .inspect_err(|error| warn!("updates are off: {error:#}"))
                .ok()
        });
        tray::run(|menu| crate::runtime()?.block_on(run(args, menu, updater)))?
    };
    match exit {
        Exit::Quit => Ok(()),
        Exit::Restart => {
            update::restart(&installation.context("cannot find this executable to run it again")?)
        }
    }
}

/// The daemon, until a shutdown signal, `menu`'s "Quit" or an update.
async fn run(args: Args, menu: Menu, updater: Option<Updater>) -> Result<Exit> {
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
    sync(
        clipboard,
        &config,
        &identity,
        &paths,
        shutdown_signal(),
        menu,
        updater.as_ref(),
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

/// Received files on their way to the cache. The main loop polls it next to everything else, so
/// a big download blocks no clip or local copy; dropping it cancels the download.
type Download<'a> = Pin<Box<dyn Future<Output = Downloaded> + Send + 'a>>;

/// A control client being served (see [`control::serve`]), or a pairing from the tray menu.
type Pairing<'a> = Pin<Box<dyn Future<Output = Served> + Send + 'a>>;

/// The end of a [`Download`]: the cache paths of the files, or why they could not be fetched.
struct Downloaded {
    from: EndpointId,
    size: u64,
    hash: String,
    paths: Result<Vec<PathBuf>, FetchError>,
}

/// Syncs `clipboard` with the members of the mesh (see [`mesh`]) until `shutdown` completes,
/// keeping the status file up to date for `clipbroad status` (see [`status`]) and received files
/// in the file cache. It also pairs when `clipbroad pair` asks over the control channel (see
/// [`control`]) or the tray menu does, and syncs with the new peer right away, and removes a
/// member when `clipbroad remove` asks. `menu` also pauses sync and quits.
/// With an `updater`, it looks for new versions and installs one when `menu` says so.
///
/// Errors from a single read, write or clip are logged and the loop keeps going; only a failed
/// setup or a failed clipboard watcher end it.
// One `select!` with a branch per event source. The branches share most of these locals, so
// moving them out takes functions with all of those as arguments.
#[allow(clippy::too_many_lines)]
pub async fn sync(
    clipboard: Arc<dyn Clipboard>,
    config: &Config,
    identity: &IdentityKey,
    paths: &Paths,
    shutdown: impl Future<Output = io::Result<()>>,
    mut menu: Menu,
    updater: Option<&Updater>,
) -> Result<Exit> {
    let mut changes = clipboard.watch().context("cannot watch the clipboard")?;
    let cache = FileCache::open(paths.file_cache_dir.clone(), config).with_context(|| {
        format!(
            "cannot open the file cache at {}",
            paths.file_cache_dir.display()
        )
    })?;
    let me = SecretKey::from_bytes(&identity.to_bytes()).public();
    let mesh = Mesh::load(paths, config, me)?;
    let node_config = Config {
        peers: mesh.peers(),
        ..config.clone()
    };
    let (node, mut inbox) = Node::spawn(identity, &node_config)
        .await
        .context("cannot start the network node")?;
    mesh.announce(&node);
    info!(
        node_id = %node.id(),
        port = %config.listen_port,
        peers = node_config.peers.len(),
        "node started"
    );
    let mut engine = SyncEngine::new(NodeId::from_bytes(*node.id().as_bytes()));
    let status_file = paths.status_file();
    // At most one: a newer clip, from here or a peer, replaces what is being downloaded.
    let mut download: Option<Download<'_>> = None;
    // The files of the newest applied clip, to recognize their echo (see [`AppliedFiles`]).
    let mut applied: Option<AppliedFiles> = None;
    // Written only once the node is up: a second daemon on the same config fails before this,
    // and must not take over the first one's file.
    let mut status = DaemonStatus::new(&node, config);
    // Same as the status file: a second daemon on the same config fails before this.
    let mut control = control::listen(paths);
    // At most one: `clipbroad pair` asks one thing at a time, and waits for it.
    let mut pairing: Option<Pairing<'_>> = None;
    let mut links = node.links();
    let mut heartbeat = time::interval(status::HEARTBEAT);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut self_update = Updates::new(updater);
    let mut update_timer =
        time::interval_at(Instant::now() + update::FIRST_CHECK, update::CHECK_EVERY);
    update_timer.set_missed_tick_behavior(MissedTickBehavior::Delay);

    // Set when the daemon stops to restart with a new identity.
    let mut new_identity = false;
    let mut shutdown = pin!(shutdown);
    let result = loop {
        tokio::select! {
            change = changes.recv() => {
                let change = match change {
                    Some(Ok(change)) => change,
                    Some(Err(error)) => break Err(error).context("the clipboard watcher failed"),
                    None => break Err(anyhow!("the clipboard watcher stopped")),
                };
                // No formats means nothing to sync (empty clipboard, or formats unknown). What
                // is copied during a pause is not sent later either.
                if change.formats.is_empty() || status.paused {
                    continue;
                }
                match local_change(&clipboard, &mut engine, &node, config, &mut applied).await {
                    Ok(Some(sent)) => {
                        // The local copy is newer than whatever was downloading.
                        download = None;
                        status.last_sent = Some(sent);
                    }
                    Ok(None) => {}
                    Err(error) => warn!("{error:#}"),
                }
            }
            // The node holds a sender, so this never ends while it runs.
            Some(received) = inbox.clips.recv() => {
                if status.paused {
                    debug!(from = %received.from.fmt_short(), "sync is paused, clip ignored");
                    continue;
                }
                status.last_received = Some(received_event(&received));
                let remote = remote_clip(
                    &clipboard, &mut engine, &node, &cache, received, &mut download, &mut applied,
                );
                if let Err(error) = remote.await {
                    warn!("{error:#}");
                }
            }
            // Same: the node holds a sender.
            Some(received) = inbox.members.recv() => mesh.merge(received, &node),
            client = accept(control.as_mut()) => match client {
                Ok(client) => serve(client, &mut pairing, &node, &mesh),
                Err(error) => warn!(%error, "cannot accept a control connection"),
            },
            served = finished(&mut pairing) => {
                pairing = None;
                match served {
                    Served::Nothing => {}
                    Served::Quit => {
                        info!("quit requested over the control channel");
                        break Ok(Exit::Quit);
                    }
                    Served::Restart => {
                        info!("restart requested over the control channel");
                        break Ok(Exit::Restart);
                    }
                    Served::Reset => {
                        info!("new identity requested over the control channel");
                        new_identity = true;
                        break Ok(Exit::Restart);
                    }
                    Served::Paired(outcome) => menu.note(pairing_result(&outcome), now_ms()),
                    Served::Removed(outcome) => menu.note(removal_result(&outcome), now_ms()),
                }
            }
            Some(action) = menu.next_action() => match action {
                Action::Quit => {
                    info!("quit chosen in the tray menu");
                    break Ok(Exit::Quit);
                }
                Action::TogglePause => {
                    status.paused = !status.paused;
                    info!(paused = status.paused, "sync toggled in the tray menu");
                    if status.paused {
                        // It would land on the clipboard during the pause.
                        download = None;
                        applied = None;
                    }
                }
                Action::Pair | Action::PairCopied => {
                    let copied = action == Action::PairCopied;
                    let text = start_pairing(copied, &mut pairing, &clipboard, &node, &mesh).await;
                    menu.note(text, now_ms());
                }
                Action::Reset if pairing.is_none() => {
                    info!("new identity chosen in the tray menu");
                    new_identity = true;
                    break Ok(Exit::Restart);
                }
                Action::Reset => menu.note("Wait for the pairing to end", now_ms()),
                Action::Remove(id) => {
                    let removed = mesh.remove_id(id, Some(&node));
                    menu.note(removal_result(&removed), now_ms());
                }
                Action::Update => {
                    if let Some(note) = self_update.choose(status.paused || pairing.is_some()) {
                        menu.note(note, now_ms());
                    }
                }
            },
            finished_update = finished(&mut self_update.task) => {
                let outcome = self_update.finish(finished_update);
                if let Some(note) = outcome.note {
                    menu.note(note, now_ms());
                }
                if outcome.restart {
                    break Ok(Exit::Restart);
                }
            }
            _ = update_timer.tick(), if config.update_check => self_update.check_on_schedule(),
            downloaded = finished(&mut download) => {
                download = None;
                match apply_files(&clipboard, downloaded).await {
                    Ok(files) => applied = files,
                    Err(error) => warn!("{error:#}"),
                }
            }
            // The node holds the sender, so this never ends while it runs.
            Ok(()) = links.changed() => {}
            _ = heartbeat.tick() => {}
            result = &mut shutdown => {
                info!("shutting down");
                break result
                    .map(|()| Exit::Quit)
                    .context("cannot listen for the shutdown signal");
            }
        }
        status.set_peers(&mesh, &links.borrow());
        status.update = self_update.available().map(|version| version.to_string());
        status.save(&status_file);
        menu.show(&status, pairing.is_some(), self_update.state(), now_ms());
    };
    // They borrow the node.
    drop((download, pairing));
    // Removes the socket, so `clipbroad pair` no longer finds this daemon.
    drop(control);
    node.shutdown().await;
    status::remove(&status_file);
    if new_identity {
        // Only now: the node no longer uses the key, and nothing writes the members file.
        let peers = reset::wipe(paths)?;
        info!(
            config_peers = peers,
            "deleted the identity, the members of the mesh and the peers in the config file; \
             restarting with a new identity"
        );
    }
    result
}

/// Completes with the end of `task`, or never if there is none.
async fn finished<F: Future + Unpin>(task: &mut Option<F>) -> F::Output {
    match task {
        Some(task) => task.await,
        None => future::pending().await,
    }
}

/// Logs how a pairing ended and returns it as a note for the menu.
fn pairing_result(outcome: &Result<(Peer, bool)>) -> String {
    match outcome {
        Ok((peer, added)) => {
            let name = peer.name.as_deref().unwrap_or("(no name)");
            info!(peer = %peer.id.fmt_short(), name, added, "paired");
            format!("Paired with {name}")
        }
        Err(error) => {
            warn!("pairing failed: {error:#}");
            format!("Pairing failed: {error:#}")
        }
    }
}

/// Logs how a removal ended and returns it as a note for the menu.
fn removal_result(outcome: &Result<clipbroad_core::members::Member>) -> String {
    match outcome {
        Ok(member) => {
            let label = mesh::label(member);
            info!(member = %label, "removed from the mesh");
            format!("Removed {label}")
        }
        Err(error) => {
            warn!("removal failed: {error:#}");
            format!("Removal failed: {error:#}")
        }
    }
}

/// Starts pairing from the menu, unless one is running: with the ticket on the clipboard if
/// `copied`, or else by copying a new one there and waiting. Returns the note to show meanwhile.
async fn start_pairing<'a>(
    copied: bool,
    pairing: &mut Option<Pairing<'a>>,
    clipboard: &Arc<dyn Clipboard>,
    node: &'a Node,
    mesh: &'a Mesh,
) -> String {
    if pairing.is_some() {
        return "Another pairing is running".to_owned();
    }
    let begun = if copied {
        join_copied(clipboard, node, mesh).await
    } else {
        host_copied(clipboard, node, mesh).await
    };
    match begun {
        Ok((task, text)) => {
            *pairing = Some(task);
            text.to_owned()
        }
        Err(error) => pairing_result(&Err(error)),
    }
}

async fn host_copied<'a>(
    clipboard: &Arc<dyn Clipboard>,
    node: &'a Node,
    mesh: &'a Mesh,
) -> Result<(Pairing<'a>, &'static str)> {
    let host = pair::host_on(node)?;
    let ticket = host.ticket().to_string();
    let clipboard = Arc::clone(clipboard);
    tokio::task::spawn_blocking(move || clipboard.write_text(&ticket))
        .await?
        .context("cannot copy the ticket")?;
    info!("waiting for the other machine to use the pairing ticket");
    let task = async move {
        Served::Paired(pair::finish_host(host, mesh, pair::TICKET_TTL, Some(node)).await)
    };
    let text = "Ticket copied. Send it to the other machine, copy it there and choose \
                \"Pair with the copied ticket\"";
    Ok((Box::pin(task), text))
}

async fn join_copied<'a>(
    clipboard: &Arc<dyn Clipboard>,
    node: &'a Node,
    mesh: &'a Mesh,
) -> Result<(Pairing<'a>, &'static str)> {
    let clipboard = Arc::clone(clipboard);
    let text = tokio::task::spawn_blocking(move || clipboard.read_text())
        .await?
        .context("cannot read the clipboard")?;
    let ticket: Ticket = text
        .and_then(|text| text.parse().ok())
        .context("the clipboard holds no pairing ticket")?;
    let task = async move { Served::Paired(pair::join_on(node, mesh, &ticket).await) };
    Ok((Box::pin(task), "Pairing..."))
}

/// Serves `client`, or refuses it while another `clipbroad pair` is being served.
fn serve<'a>(
    client: control::ServerStream,
    pairing: &mut Option<Pairing<'a>>,
    node: &'a Node,
    mesh: &'a Mesh,
) {
    if pairing.is_some() {
        tokio::spawn(control::refuse(client));
    } else {
        *pairing = Some(Box::pin(control::serve(client, node, mesh)));
    }
}

/// The next client of the control channel, or never if it could not open.
async fn accept(control: Option<&mut control::Listener>) -> io::Result<control::ServerStream> {
    match control {
        Some(control) => control.accept().await,
        None => future::pending().await,
    }
}

fn received_event(received: &Received) -> ClipEvent {
    let files = match &received.clip.payload {
        Payload::Files(entries) => Some(entries.len()),
        Payload::Inline(_) => None,
    };
    ClipEvent {
        at_ms: now_ms(),
        kind: received.clip.kind,
        size: received.clip.size,
        files,
        hash: short_hash(received.clip.hash),
        from: Some(received.from),
    }
}

/// Reads the local clipboard after a change and sends it to the peers if it is new. Returns what
/// was sent, if anything. `applied` is forgotten once something is sent.
async fn local_change(
    clipboard: &Arc<dyn Clipboard>,
    engine: &mut SyncEngine,
    node: &Node,
    config: &Config,
    applied: &mut Option<AppliedFiles>,
) -> Result<Option<ClipEvent>> {
    let clipboard = Arc::clone(clipboard);
    // Reads may block (up to 2 s by contract), so keep them off the async thread.
    let copied = match tokio::task::spawn_blocking(move || clipboard.read_content()).await? {
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
        ClipboardContent::Files(paths) => {
            // One `stat` per file, in the local cache: cheap enough for the async thread.
            if applied
                .as_ref()
                .is_some_and(|applied| applied.matches(&paths))
            {
                debug!(
                    files = paths.len(),
                    "clipboard holds the files just applied, not hashed again"
                );
                return Ok(None);
            }
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
        // A ticket is meant for a machine that is not paired yet, and it is a secret for 5 minutes.
        ClipboardContent::Text(text) if text.parse::<Ticket>().is_ok() => {
            debug!("clipboard holds a pairing ticket, not synced");
            return Ok(None);
        }
        ClipboardContent::Text(text) => (ClipContent::Text(text), None),
        ClipboardContent::Image(png) => (ClipContent::Image(png), None),
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
    // The engine moved on to this clip, so the applied files are no longer its current content.
    *applied = None;
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
/// `download`, and [`apply_files`] writes their paths once it ends. Applying forgets `applied`.
async fn remote_clip<'d>(
    clipboard: &Arc<dyn Clipboard>,
    engine: &mut SyncEngine,
    node: &'d Node,
    cache: &'d FileCache,
    received: Received,
    download: &mut Option<Download<'d>>,
    applied: &mut Option<AppliedFiles>,
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
    // Whatever was downloading or applied before is older than this clip.
    *download = None;
    *applied = None;
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
/// newer one drops the download before it ends. Returns the files written, to recognize their
/// echo, or `None` if they were not written.
async fn apply_files(
    clipboard: &Arc<dyn Clipboard>,
    downloaded: Downloaded,
) -> Result<Option<AppliedFiles>> {
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
    let (written, applied) = tokio::task::spawn_blocking(move || {
        let written = clipboard.write_files(&paths);
        // Stamped after the fetch, which refreshed the mtimes of files that were cached already.
        let applied = written.is_ok().then(|| AppliedFiles::stamp(paths));
        (written, applied)
    })
    .await?;
    report_write(written, from, ContentKind::Files, size, &hash)?;
    // Without stamps, the echo is hashed as before.
    Ok(applied.and_then(|stamped| {
        stamped
            .inspect_err(|error| debug!(%error, "cannot stamp the applied files"))
            .ok()
    }))
}

/// The cache paths of the newest applied files clip, as written to the clipboard, with each
/// file's size and modification time read right after the write.
///
/// Writing them fires a local event, and reading them back would hash every file again only to
/// learn that the engine already has this clip. While the same paths still have the same size and
/// mtime, the daemon skips that. It gives the same result as hashing, because the daemon forgets
/// these files as soon as the engine moves on (any clip applied or sent, or a pause), so until
/// then the engine's current clip is this one and would answer "already synced":
/// - a cached file edited in place changes its size or mtime, so it is hashed and sent as usual;
/// - a person copying the same cache files again while nothing else happened gets "already
///   synced" today too.
///
/// Only a rewrite that keeps the size and sets the old mtime back goes unnoticed.
#[derive(Debug)]
struct AppliedFiles(Vec<(PathBuf, FileStamp)>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    size: u64,
    modified: SystemTime,
}

impl FileStamp {
    fn of(path: &Path) -> io::Result<Self> {
        let meta = fs::metadata(path)?;
        Ok(Self {
            size: meta.len(),
            modified: meta.modified()?,
        })
    }
}

impl AppliedFiles {
    fn stamp(paths: Vec<PathBuf>) -> io::Result<Self> {
        paths
            .into_iter()
            .map(|path| FileStamp::of(&path).map(|stamp| (path, stamp)))
            .collect::<io::Result<_>>()
            .map(Self)
    }

    /// Whether `paths` are these files, in the same order, unchanged since they were applied.
    fn matches(&self, paths: &[PathBuf]) -> bool {
        self.0.len() == paths.len()
            && self.0.iter().zip(paths).all(|((applied, stamp), path)| {
                applied == path && FileStamp::of(path).is_ok_and(|now| now == *stamp)
            })
    }
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
