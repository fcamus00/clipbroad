//! Daemons on `FakeClipboard`s talking over localhost, plus an observer: a plain network node that
//! never copies and records every clip that reaches it, which shows what each daemon sends.

use std::collections::HashSet;
use std::net::{Ipv4Addr, UdpSocket};
use std::num::{NonZeroU16, NonZeroU64};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use clipbroad_clipboard::fake::{TEXT_FORMAT, tiny_png};
use clipbroad_config::Peer;
use clipbroad_core::Png;
use iroh::{EndpointId, SecretKey};
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{self, Instant};

use super::*;

/// Upper bound for anything a test waits on. Localhost takes milliseconds; this only stops a
/// broken test from hanging CI.
const TIMEOUT: Duration = Duration::from_secs(10);
/// How long nothing more may arrive after a clip has reached everyone, to count as "no echo".
const QUIET: Duration = Duration::from_millis(500);

/// A node before it starts: its identity and the port it will listen on.
struct Spec {
    key: IdentityKey,
    port: NonZeroU16,
}

impl Spec {
    fn new() -> Self {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("bind a free port");
        let port = socket.local_addr().expect("local address").port();
        Self {
            key: IdentityKey::generate().expect("random key"),
            port: NonZeroU16::new(port).expect("the OS never assigns port 0"),
        }
    }

    fn id(&self) -> EndpointId {
        SecretKey::from_bytes(&self.key.to_bytes()).public()
    }

    fn config<'a>(
        &self,
        peers: impl IntoIterator<Item = &'a Self>,
        inline_max_bytes: u64,
    ) -> Config {
        Config {
            listen_port: self.port,
            inline_max_bytes: NonZeroU64::new(inline_max_bytes).expect("non-zero"),
            mdns: false,
            peers: peers.into_iter().map(Self::peer).collect(),
        }
    }

    fn peer(&self) -> Peer {
        Peer {
            id: self.id(),
            name: None,
            addrs: vec![(Ipv4Addr::LOCALHOST, self.port.get()).into()],
        }
    }
}

/// [`sync`] running in a task on its own fake clipboard, with its status file in its own
/// directory.
struct Daemon {
    id: EndpointId,
    clipboard: FakeClipboard,
    status_file: PathBuf,
    stop: oneshot::Sender<()>,
    task: JoinHandle<Result<()>>,
    _dir: TempDir,
}

impl Daemon {
    fn start(spec: &Spec, config: Config) -> Self {
        let clipboard = FakeClipboard::new();
        let (stop, stopped) = oneshot::channel::<()>();
        let shared: Arc<dyn Clipboard> = Arc::new(clipboard.clone());
        let key = spec.key.clone();
        let dir = tempfile::tempdir().expect("temp dir");
        let status_file = dir.path().join("status.toml");
        let path = status_file.clone();
        let task = tokio::spawn(async move {
            let shutdown = async {
                // A dropped sender also stops the daemon.
                let _ = stopped.await;
                Ok(())
            };
            sync(shared, &config, &key, &path, shutdown).await
        });
        Self {
            id: spec.id(),
            clipboard,
            status_file,
            stop,
            task,
            _dir: dir,
        }
    }

    /// What `clipbroad status` would read. `None` also while the file cannot be read, as on
    /// Windows if the daemon is replacing it right now.
    fn status(&self) -> Option<DaemonStatus> {
        status::read(&self.status_file).ok().flatten()
    }

    fn text(&self) -> Option<String> {
        self.clipboard.read_text().expect("the fake never fails")
    }

    fn image(&self) -> Option<Png> {
        self.clipboard.read_image().expect("the fake never fails")
    }

    async fn stop(self) {
        assert!(!self.task.is_finished(), "daemon ended early: {self:?}");
        let _ = self.stop.send(());
        time::timeout(TIMEOUT, self.task)
            .await
            .expect("daemon stops")
            .expect("daemon does not panic")
            .expect("daemon stops cleanly");
    }
}

impl std::fmt::Debug for Daemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Daemon({})", self.id.fmt_short())
    }
}

/// Records `(sender, content hash)` for every clip that reaches it. Duplicates collapse, so a
/// clip delivered twice over a duplicate connection (see `clipbroad-net`) counts once.
struct Observer {
    seen: Arc<Mutex<HashSet<(EndpointId, ContentHash)>>>,
    _node: Node,
}

impl Observer {
    async fn start(spec: &Spec, daemons: &[Spec]) -> Self {
        let config = spec.config(daemons, Config::MAX_INLINE_MAX_BYTES);
        let (node, mut inbox) = Node::spawn(&spec.key, &config)
            .await
            .expect("observer starts");
        let seen = Arc::new(Mutex::new(HashSet::new()));
        let record = Arc::clone(&seen);
        tokio::spawn(async move {
            while let Some(received) = inbox.recv().await {
                lock(&record).insert((received.from, received.clip.hash));
            }
        });
        Self { seen, _node: node }
    }

    /// Who sent `content` (text, or an image as `ClipContent::Image`).
    fn senders(&self, content: impl Into<ClipContent>) -> HashSet<EndpointId> {
        let hash = content.into().hash();
        lock(&self.seen)
            .iter()
            .filter(|(_, seen)| *seen == hash)
            .map(|(from, _)| *from)
            .collect()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Polls `condition` until it holds or `within` passes; returns its last value.
async fn eventually(within: Duration, condition: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        time::sleep(Duration::from_millis(10)).await;
    }
    condition()
}

/// Copies on each daemon until its copy reaches every other daemon and the observer. A clip sent
/// before a peer connects is lost, so the tests start only once every connection is up.
async fn connect_all(daemons: &[Daemon], observer: &Observer) {
    let deadline = Instant::now() + TIMEOUT;
    for (i, daemon) in daemons.iter().enumerate() {
        for attempt in 0.. {
            assert!(
                Instant::now() < deadline,
                "{daemon:?} did not reach its peers"
            );
            let text = format!("warm-up {i}.{attempt}");
            daemon.clipboard.simulate_copy_text(&text);
            let reached = || {
                daemons
                    .iter()
                    .all(|other| other.text().as_deref() == Some(&text))
                    && observer.senders(text.as_str()).contains(&daemon.id)
            };
            if eventually(Duration::from_millis(500), reached).await {
                break;
            }
        }
    }
}

#[tokio::test]
async fn copy_reaches_every_peer_once_and_is_not_echoed() {
    let specs = [Spec::new(), Spec::new(), Spec::new()];
    let observer_spec = Spec::new();
    let observer = Observer::start(&observer_spec, &specs).await;
    let daemons: Vec<Daemon> = specs
        .iter()
        .map(|spec| {
            let others = specs.iter().filter(|other| other.port != spec.port);
            let peers = others.chain([&observer_spec]);
            Daemon::start(
                spec,
                spec.config(peers, Config::DEFAULT_INLINE_MAX_BYTES.get()),
            )
        })
        .collect();
    connect_all(&daemons, &observer).await;
    let [a, _, c] = &daemons[..] else {
        unreachable!("three daemons")
    };
    let all_hold = |text: &str| daemons.iter().all(|d| d.text().as_deref() == Some(text));

    a.clipboard.simulate_copy_text("hello");
    assert!(eventually(TIMEOUT, || all_hold("hello")).await);
    // B and C wrote "hello" to their clipboards, which fired change events. Had they treated those
    // as new copies, the observer would get "hello" from them too.
    time::sleep(QUIET).await;
    assert_eq!(observer.senders("hello"), HashSet::from([a.id]));
    assert!(all_hold("hello"));

    // Any machine can copy next, and its copy wins everywhere.
    c.clipboard.simulate_copy_text("world");
    assert!(eventually(TIMEOUT, || all_hold("world")).await);
    time::sleep(QUIET).await;
    assert_eq!(observer.senders("world"), HashSet::from([c.id]));
    assert!(all_hold("world"));

    for daemon in daemons {
        daemon.stop().await;
    }
}

#[tokio::test]
async fn image_reaches_every_peer_once_and_text_wins_over_it() {
    let specs = [Spec::new(), Spec::new()];
    let observer_spec = Spec::new();
    let observer = Observer::start(&observer_spec, &specs).await;
    let daemons: Vec<Daemon> = specs
        .iter()
        .map(|spec| {
            let other = specs.iter().filter(|other| other.port != spec.port);
            let peers = other.chain([&observer_spec]);
            Daemon::start(
                spec,
                spec.config(peers, Config::DEFAULT_INLINE_MAX_BYTES.get()),
            )
        })
        .collect();
    connect_all(&daemons, &observer).await;
    let [a, b] = &daemons[..] else {
        unreachable!("two daemons")
    };

    a.clipboard.simulate_copy_image(tiny_png());
    assert!(eventually(TIMEOUT, || b.image() == Some(tiny_png())).await);
    // B wrote the image, which fired a change event; it must not send it back.
    time::sleep(QUIET).await;
    let image = ClipContent::Image(tiny_png());
    assert_eq!(observer.senders(image.clone()), HashSet::from([a.id]));
    assert_eq!(a.image(), Some(tiny_png()));

    // A copy that offers both, like spreadsheet cells: only the text is sent.
    b.clipboard
        .simulate_copy_text_and_image("cells", tiny_png());
    assert!(eventually(TIMEOUT, || a.text().as_deref() == Some("cells")).await);
    assert_eq!(a.image(), None);
    assert_eq!(observer.senders(image), HashSet::from([a.id]));

    for daemon in daemons {
        daemon.stop().await;
    }
}

#[tokio::test]
async fn skips_what_it_cannot_send_and_keeps_running() {
    let spec = Spec::new();
    let observer_spec = Spec::new();
    let observer = Observer::start(&observer_spec, std::slice::from_ref(&spec)).await;
    let daemon = Daemon::start(&spec, spec.config([&observer_spec], 16));
    connect_all(std::slice::from_ref(&daemon), &observer).await;

    let too_large = "x".repeat(17);
    // A non-PNG image is not read at all.
    daemon
        .clipboard
        .simulate_copy(None, vec!["image/jpeg".to_owned()]);
    daemon.clipboard.simulate_copy_text(&too_large);
    // The PNG is 70 bytes.
    daemon.clipboard.simulate_copy_image(tiny_png());
    daemon.clipboard.simulate_clear();
    daemon
        .clipboard
        .simulate_copy(Some("no formats".to_owned()), Vec::new());
    daemon.clipboard.simulate_copy_text("small");

    // The daemon handles changes in order, so once "small" arrives the others were handled.
    assert!(eventually(TIMEOUT, || observer.senders("small").contains(&daemon.id)).await);
    assert_eq!(observer.senders(too_large.as_str()), HashSet::new());
    assert_eq!(
        observer.senders(ClipContent::Image(tiny_png())),
        HashSet::new()
    );
    assert_eq!(observer.senders("no formats"), HashSet::new());
    daemon.stop().await;
}

#[tokio::test]
async fn never_sends_sensitive_content() {
    let spec = Spec::new();
    let observer_spec = Spec::new();
    let observer = Observer::start(&observer_spec, std::slice::from_ref(&spec)).await;
    let daemon = Daemon::start(
        &spec,
        spec.config([&observer_spec], Config::DEFAULT_INLINE_MAX_BYTES.get()),
    );
    connect_all(std::slice::from_ref(&daemon), &observer).await;

    // What KeePassXC offers on Linux: the text plus the password manager hint.
    daemon.clipboard.simulate_copy(
        Some("hunter2".to_owned()),
        vec![
            TEXT_FORMAT.to_owned(),
            "x-kde-passwordManagerHint".to_owned(),
        ],
    );
    daemon.clipboard.simulate_copy_text("after the password");

    // The daemon handles changes in order, so once the next copy arrives the secret was handled.
    let next_arrived = || observer.senders("after the password").contains(&daemon.id);
    assert!(eventually(TIMEOUT, next_arrived).await);
    assert_eq!(observer.senders("hunter2"), HashSet::new());
    daemon.stop().await;
}

#[tokio::test]
async fn runs_without_peers() {
    let spec = Spec::new();
    let daemon = Daemon::start(
        &spec,
        spec.config([], Config::DEFAULT_INLINE_MAX_BYTES.get()),
    );
    daemon.clipboard.simulate_copy_text("nobody listens");
    time::sleep(QUIET).await;
    daemon.stop().await;
}

#[tokio::test]
async fn status_file_follows_peers_and_clips() {
    let (a, b) = (Spec::new(), Spec::new());
    let inline_max_bytes = Config::DEFAULT_INLINE_MAX_BYTES.get();
    let daemon_a = Daemon::start(&a, a.config([&b], inline_max_bytes));
    let daemon_b = Daemon::start(&b, b.config([&a], inline_max_bytes));
    let peer_connected = |daemon: &Daemon| {
        daemon
            .status()
            .is_some_and(|status| status.peers.iter().all(|peer| peer.connected))
    };
    assert!(
        eventually(TIMEOUT, || peer_connected(&daemon_a)
            && peer_connected(&daemon_b))
        .await
    );

    daemon_a.clipboard.simulate_copy_text("hello");
    let hash = short_hash(ContentHash::of(b"hello"));
    let sent = || daemon_a.status().and_then(|s| s.last_sent);
    let received = || daemon_b.status().and_then(|s| s.last_received);
    assert!(eventually(TIMEOUT, || sent().is_some() && received().is_some()).await);
    let (sent, received) = (sent().expect("sent"), received().expect("received"));
    assert_eq!((sent.size, &sent.hash, sent.from), (5, &hash, None));
    assert_eq!(
        (received.size, &received.hash, received.from),
        (5, &hash, Some(a.id()))
    );

    let shown = status::render(daemon_a.status().as_ref(), now_ms());
    let expected = [
        "daemon         running (pid ".to_owned(),
        format!("node id        {}\n", a.id()),
        "peers          1 of 1 connected\n".to_owned(),
        format!("connected via 127.0.0.1:{}\n", b.port),
        format!(", text, 5 bytes, hash {hash}\n"),
    ];
    for line in expected {
        assert!(shown.contains(&line), "{line:?} missing in:\n{shown}");
    }

    daemon_b.stop().await;
    assert!(eventually(TIMEOUT, || !peer_connected(&daemon_a)).await);
    let status_file = daemon_a.status_file.clone();
    daemon_a.stop().await;
    // A daemon that stops removes its file.
    assert!(!status_file.exists());
}
