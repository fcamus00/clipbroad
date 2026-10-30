//! Peer-to-peer transport: connects to every paired peer with iroh and exchanges clips.
//!
//! [`Node::spawn`] binds an iroh endpoint and keeps one connection per peer alive in the
//! background. [`Node::broadcast`] sends a clip to every connected peer, and the receiver returned
//! by `spawn` yields the clips that arrive.
//!
//! # Network
//!
//! The endpoint uses iroh's `Minimal` preset: no relay servers and no DNS or pkarr address
//! lookup, so no traffic goes to third-party services. Peers are dialed at the static addresses
//! in the config and, if `config.mdns` is on, at the addresses they announce on the LAN with mDNS
//! (see [`MDNS_SERVICE`]). Discovery only finds addresses: only peers in the config can connect,
//! and anyone else is rejected right after the TLS handshake.
//!
//! # Connections
//!
//! Each node dials every peer it can find (any peer when mDNS is on, otherwise only those with
//! static addresses), and accepts connections from them. Both sides may dial at once and end up
//! with two connections. Only the node with the smaller id decides which one stays: it keeps the
//! one that most recently finished the `Hello` exchange and closes the older one. The other node
//! keeps whatever is still open. "Newest wins" also handles a peer that restarted while its old
//! connection still looks alive. A lost connection is dialed again
//! with exponential backoff (0.5 s up to 30 s). [`Node::links`] tells which peers are connected.
//!
//! Inside a connection each side opens one unidirectional stream and writes frames on it: first
//! a [`Hello`](clipbroad_core::protocol::Hello), then one frame per clip. It reads the other
//! side's stream the same way. Each file transfer is a bidirectional stream on the same
//! connection, with a lower priority than the clip stream (see [`Node::fetch_files`]).
//!
//! # Files
//!
//! A files clip only announces the files. The sender broadcasts it with
//! [`Node::broadcast_files`] and from then on serves those files, and only those, from their
//! original paths ([`Offer`]). The receiver calls [`Node::fetch_files`], which downloads them into
//! a [`FileCache`] and returns their local paths.
//!
//! # Trust
//!
//! The mesh is full and nothing is forwarded, so a clip must come from the machine that copied
//! it. A clip whose `origin` is not the sending peer is dropped. The receiver still has to get
//! the content through [`Clip::into_content`], which checks the payload.
//!
//! # Pairing
//!
//! [`pair`] adds a peer: one machine shows a ticket and the other dials it, on a separate ALPN.
//! A running node can do either side with its own endpoint ([`Node::host_pairing`],
//! [`Node::join_pairing`]), and [`Node::add_peer`] starts syncing with the new peer.

mod cache;
mod files;
pub mod pair;
mod peer;
mod session;
#[cfg(test)]
mod tests;

use std::{
    collections::{HashMap, HashSet},
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError, RwLock},
};

pub use cache::{Cleaned, FileCache, StoreError};
use clipbroad_config::{Config, IdentityKey, Peer};
use clipbroad_core::{
    FileList,
    protocol::{self, Clip, FrameError, Message},
};
pub use files::{FetchError, Offer};
use iroh::{
    Endpoint, EndpointId, SecretKey,
    endpoint::{
        AfterHandshakeOutcome, BindError, BindOpts, Builder, Connection, EndpointHooks, presets,
    },
};
use iroh_mdns_address_lookup::MdnsAddressLookup;
use tokio::{
    sync::{
        broadcast,
        mpsc::{self, error::TrySendError},
        watch,
    },
    task::JoinSet,
};
use tracing::{debug, info, warn};

/// ALPN of the clipbroad protocol. Both sides must offer it to connect.
pub const ALPN: &[u8] = b"clipbroad/1";

/// mDNS service each node announces itself under, as `<id>._clipbroad._udp.local` with its
/// addresses. Its own name keeps clipbroad apart from other iroh apps on the LAN.
pub const MDNS_SERVICE: &str = "clipbroad";

/// How many received clips can wait for the daemon before the peers are slowed down.
const INBOX_CAPACITY: usize = 64;
/// How many outgoing clips a slow connection can fall behind before it skips some.
const OUTBOX_CAPACITY: usize = 16;

/// Codes sent when this node closes a connection.
mod close {
    use iroh::endpoint::VarInt;

    pub const NORMAL: VarInt = VarInt::from_u32(0);
    pub const REPLACED: VarInt = VarInt::from_u32(1);
    pub const PROTOCOL_ERROR: VarInt = VarInt::from_u32(2);
    pub const NOT_PAIRED: VarInt = VarInt::from_u32(3);
}

/// An encoded frame, shared by every connection that sends it.
type Frame = Arc<[u8]>;

/// A clip received from a peer.
#[derive(Debug)]
pub struct Received {
    /// The peer that sent it. Always the clip's origin: clips relayed for others are dropped.
    pub from: EndpointId,
    /// Not verified yet: get the content with [`Clip::into_content`].
    pub clip: Clip,
}

/// Whether a configured peer is connected right now. See [`Node::links`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    Disconnected,
    /// At least one connection finished the `Hello` exchange. `addr` is the peer's address on the
    /// path the newest connection used when it connected, or `None` if that was not an IP path.
    Connected {
        addr: Option<SocketAddr>,
    },
}

/// The [`Link`] of every configured peer.
pub type Links = HashMap<EndpointId, Link>;

/// Why the node could not start.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("could not listen on UDP port {port}")]
    Bind {
        port: u16,
        #[source]
        source: BindError,
    },
    #[error("could not generate a random pairing secret")]
    Random(#[source] getrandom::Error),
}

/// A running node. Dropping it stops all connections; [`Node::shutdown`] also tells peers.
#[derive(Debug)]
pub struct Node {
    endpoint: Endpoint,
    outbox: broadcast::Sender<Frame>,
    /// Files of the latest broadcast clip, served to peers that ask.
    offer: watch::Sender<Option<Arc<Offer>>>,
    links: watch::Sender<Links>,
    lan_discovery: bool,
    /// Kept to start the task of a peer added later.
    channels: peer::Channels,
    /// Shared with the endpoint's hook, so an added peer can connect.
    allowlist: Arc<RwLock<HashSet<EndpointId>>>,
    routes: Arc<Routes>,
    tasks: Mutex<JoinSet<()>>,
    peers: Mutex<HashMap<EndpointId, Arc<peer::PeerState>>>,
}

/// Where the accept loop hands each incoming connection.
#[derive(Debug, Default)]
struct Routes {
    /// Each peer's task.
    peers: Mutex<HashMap<EndpointId, mpsc::Sender<Connection>>>,
    /// The pairing host waiting for requests, if any (see [`Node::host_pairing`]).
    pairing: Mutex<Option<mpsc::Sender<Connection>>>,
}

impl Node {
    /// Starts listening on `config.listen_port` (all interfaces, IPv4 and IPv6 if available) and
    /// starts connecting to `config.peers`.
    ///
    /// If `config.mdns` is on but mDNS cannot start (for example, no network interface allows
    /// multicast), the node logs a warning and runs without it.
    ///
    /// Returns the node and the receiver of incoming clips. The receiver must be drained: when it
    /// is full, peers wait.
    ///
    /// # Errors
    ///
    /// [`Error::Bind`] if the port is in use or the sockets cannot be created.
    ///
    /// # Panics
    ///
    /// Only on a bug: if iroh rejects one unspecified bind address per IP family.
    pub async fn spawn(
        identity: &IdentityKey,
        config: &Config,
    ) -> Result<(Self, mpsc::Receiver<Received>), Error> {
        let port = config.listen_port.get();
        let allowlist = config.peers.iter().map(|peer| peer.id).collect();
        let allowlist = Arc::new(RwLock::new(allowlist));
        let mut builder = endpoint_builder(identity, port)
            .alpns(vec![ALPN.to_vec(), pair::PAIR_ALPN.to_vec()])
            .hooks(Allowlist(Arc::clone(&allowlist)));
        let mdns = config.mdns.then(|| start_mdns(identity)).flatten();
        let discovery = mdns.is_some();
        if let Some(mdns) = mdns {
            builder = builder.address_lookup(mdns);
        }
        let endpoint = builder
            .bind()
            .await
            .map_err(|source| Error::Bind { port, source })?;

        let (inbox_tx, inbox) = mpsc::channel(INBOX_CAPACITY);
        let (outbox, _) = broadcast::channel(OUTBOX_CAPACITY);
        let offer = watch::Sender::new(None);
        let links = config.peers.iter().map(|p| (p.id, Link::Disconnected));
        let links = watch::Sender::new(links.collect());
        let channels = peer::Channels {
            outbox: outbox.clone(),
            inbox: inbox_tx,
            offer: offer.subscribe(),
            links: links.clone(),
        };
        let routes = Arc::new(Routes::default());
        let mut tasks = JoinSet::new();
        tasks.spawn(accept_loop(endpoint.clone(), Arc::clone(&routes)));
        let node = Self {
            endpoint,
            outbox,
            offer,
            links,
            lan_discovery: discovery,
            channels,
            allowlist,
            routes,
            tasks: Mutex::new(tasks),
            peers: Mutex::new(HashMap::new()),
        };
        for peer in &config.peers {
            node.start_peer(peer);
        }
        Ok((node, inbox))
    }

    /// Starts syncing with `peer`, a machine that was just paired: it may connect from now on,
    /// and the node dials it like the peers in the config. Returns `false`, and changes nothing,
    /// if it is a peer already.
    ///
    /// The node does not save it: add it to the config file too.
    pub fn add_peer(&self, peer: &Peer) -> bool {
        if !lock_write(&self.allowlist).insert(peer.id) {
            return false;
        }
        self.links.send_modify(|links| {
            links.insert(peer.id, Link::Disconnected);
        });
        self.start_peer(peer);
        info!(peer = %peer.id.fmt_short(), "added peer");
        true
    }

    /// Runs the task that keeps `peer` connected. The allowlist and the links must have it.
    fn start_peer(&self, peer: &Peer) {
        let (conn_tx, conn_rx) = mpsc::channel(4);
        let peer_task = peer::PeerTask::new(
            self.endpoint.clone(),
            peer.id,
            &peer.addrs,
            self.lan_discovery,
            self.channels.clone(),
        );
        lock(&self.peers).insert(peer.id, peer_task.state());
        lock(&self.routes.peers).insert(peer.id, conn_tx);
        lock(&self.tasks).spawn(peer_task.run(conn_rx));
    }

    /// This node's public identity.
    #[must_use]
    pub fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// Whether LAN discovery (mDNS) is running: `config.mdns` is on and mDNS could start.
    #[must_use]
    pub fn lan_discovery(&self) -> bool {
        self.lan_discovery
    }

    /// Follows the [`Link`] of every configured peer. It changes when a peer connects or its last
    /// connection ends.
    #[must_use]
    pub fn links(&self) -> watch::Receiver<Links> {
        self.links.subscribe()
    }

    /// Sends `clip` to every connected peer. Peers that are not connected right now miss it.
    ///
    /// The files of the previous clip are no longer served. For a files clip, use
    /// [`Node::broadcast_files`].
    ///
    /// # Errors
    ///
    /// [`FrameError::TooLarge`] if the clip does not fit in a frame. Nothing changes then.
    pub fn broadcast(&self, clip: Clip) -> Result<(), FrameError> {
        self.send_clip(clip, None)
    }

    /// Sends `clip`, which announces the files of `offer`, to every connected peer, and serves
    /// those files to any peer that asks until the next broadcast. A transfer already running
    /// for an earlier clip goes on until the receiver stops it.
    ///
    /// # Errors
    ///
    /// [`FrameError::TooLarge`] if the clip does not fit in a frame (tens of thousands of
    /// files). Nothing changes then.
    pub fn broadcast_files(&self, clip: Clip, offer: Offer) -> Result<(), FrameError> {
        self.send_clip(clip, Some(Arc::new(offer)))
    }

    fn send_clip(&self, clip: Clip, offer: Option<Arc<Offer>>) -> Result<(), FrameError> {
        let frame = protocol::encode(&Message::Clip(clip))?;
        // Serve the files before announcing them, so a quick request finds them.
        self.offer.send_replace(offer);
        // An error only means that no peer is connected.
        let _ = self.outbox.send(frame.into());
        Ok(())
    }

    /// Downloads the files of a files clip from `from`, the peer that sent it, into `cache`, and
    /// returns their local paths in the order of `files`. Files already in the cache are not
    /// downloaded again. Afterwards it cleans the cache, keeping these files.
    ///
    /// The limits come first: a file over `file_max_bytes`, or files that add up to more than
    /// `file_cache_max_bytes`, fail before anything is asked for. Each file is then written to a
    /// temporary file while it is hashed, and only moved to its cache path if its size and hash
    /// match `files` (see [`FileCache`]). If the connection drops, the file is asked for again on
    /// the next connection, up to three tries.
    ///
    /// Dropping the future cancels the transfer: the stream is closed and the temporary file
    /// deleted. When a newer clip arrives, cancel the running fetch this way.
    ///
    /// # Errors
    ///
    /// [`FetchError`] for the first file that cannot be fetched. Files fetched before it stay in
    /// the cache.
    pub async fn fetch_files(
        &self,
        from: EndpointId,
        files: &FileList,
        cache: &FileCache,
    ) -> Result<Vec<PathBuf>, FetchError> {
        let live = lock(&self.peers)
            .get(&from)
            .ok_or(FetchError::UnknownPeer)?
            .live();
        files::fetch(live, files, cache).await
    }

    /// Closes every connection, telling peers right away, and stops the node.
    pub async fn shutdown(self) {
        let mut tasks = self
            .tasks
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        tasks.shutdown().await;
        self.endpoint.close().await;
    }
}

/// An endpoint with `identity` that will listen on `port` on all interfaces (IPv4, and IPv6 if
/// available), without relays or address lookup (see the crate docs).
///
/// # Panics
///
/// Only on a bug: if iroh rejects one unspecified bind address per IP family.
fn endpoint_builder(identity: &IdentityKey, port: u16) -> Builder {
    Endpoint::builder(presets::Minimal)
        .secret_key(SecretKey::from_bytes(&identity.to_bytes()))
        .clear_ip_transports()
        .bind_addr(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)))
        .expect("a single IPv4 bind address is valid")
        .bind_addr_with_opts(
            SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)),
            // Some machines have IPv6 disabled; IPv4 is enough.
            BindOpts::default().set_is_required(false),
        )
        .expect("a single IPv6 bind address is valid")
}

/// Starts announcing this node on the LAN and listening for other nodes' announcements. Returns
/// `None`, after a warning, if the multicast sockets cannot be set up.
fn start_mdns(identity: &IdentityKey) -> Option<MdnsAddressLookup> {
    let id = SecretKey::from_bytes(&identity.to_bytes()).public();
    match MdnsAddressLookup::builder()
        .service_name(MDNS_SERVICE)
        .build(id)
    {
        Ok(mdns) => {
            info!("LAN discovery (mDNS) on");
            Some(mdns)
        }
        Err(e) => {
            warn!(error = %e, "LAN discovery (mDNS) could not start; only static addresses work");
            None
        }
    }
}

/// Locks a mutex. Nothing panics while holding one of the node's locks, but if it did, the data
/// would still be consistent: every change is a single insert.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn lock_write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

/// Rejects connections from anyone who is not a configured peer, except to pair: those carry
/// [`pair::PAIR_ALPN`] and only reach a pairing host (see [`Node::host_pairing`]).
#[derive(Debug)]
struct Allowlist(Arc<RwLock<HashSet<EndpointId>>>);

impl EndpointHooks for Allowlist {
    fn after_handshake<'a>(
        &'a self,
        conn: &'a Connection,
    ) -> impl Future<Output = AfterHandshakeOutcome> + Send + 'a {
        let remote = conn.remote_id();
        let allowed = || {
            self.0
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .contains(&remote)
        };
        let outcome = if conn.alpn() == pair::PAIR_ALPN || allowed() {
            AfterHandshakeOutcome::Accept
        } else {
            warn!(remote = %remote, "rejected connection from an unpaired node");
            AfterHandshakeOutcome::Reject {
                error_code: close::NOT_PAIRED,
                reason: b"not paired".to_vec(),
            }
        };
        std::future::ready(outcome)
    }
}

/// Accepts incoming connections and hands each one to its peer's task, or to pair to the pairing
/// host. Without a host, a pairing connection is closed.
async fn accept_loop(endpoint: Endpoint, routes: Arc<Routes>) {
    let mut handshakes = JoinSet::new();
    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                // `None` means the endpoint was closed.
                let Some(incoming) = incoming else { break };
                handshakes.spawn(incoming.into_future());
            }
            Some(handshake) = handshakes.join_next() => {
                let Ok(handshake) = handshake else { continue };
                let conn = match handshake {
                    Ok(conn) => conn,
                    Err(e) => {
                        debug!(error = %e, "incoming connection failed");
                        continue;
                    }
                };
                if conn.alpn() == pair::PAIR_ALPN {
                    let host = lock(&routes.pairing).clone();
                    // The host checks one request at a time, so a full queue refuses the rest.
                    let refused = match host {
                        Some(host) => host.try_send(conn).err().map(TrySendError::into_inner),
                        None => Some(conn),
                    };
                    if let Some(conn) = refused {
                        conn.close(close::NOT_PAIRED, b"not pairing");
                    }
                    continue;
                }
                // The allowlist only lets peers through, and every peer has a route.
                let route = lock(&routes.peers).get(&conn.remote_id()).cloned();
                if let Some(route) = route {
                    // An error means the node is stopping.
                    let _ = route.send(conn).await;
                }
            }
        }
    }
}
