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
//! side's stream the same way.
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

pub mod pair;
mod peer;
mod session;
#[cfg(test)]
mod tests;

use std::{
    collections::{HashMap, HashSet},
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
};

use clipbroad_config::{Config, IdentityKey};
use clipbroad_core::protocol::{self, Clip, FrameError, Message};
use iroh::{
    Endpoint, EndpointId, SecretKey,
    endpoint::{
        AfterHandshakeOutcome, BindError, BindOpts, Builder, Connection, EndpointHooks, presets,
    },
};
use iroh_mdns_address_lookup::MdnsAddressLookup;
use tokio::{
    sync::{broadcast, mpsc, watch},
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
    links: watch::Sender<Links>,
    lan_discovery: bool,
    tasks: JoinSet<()>,
    #[cfg(test)]
    peers: HashMap<EndpointId, Arc<peer::PeerState>>,
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
        let mut builder = endpoint_builder(identity, port)
            .alpns(vec![ALPN.to_vec()])
            .hooks(Allowlist(allowlist));
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
        let links = config.peers.iter().map(|p| (p.id, Link::Disconnected));
        let links = watch::Sender::new(links.collect());
        let mut tasks = JoinSet::new();
        let mut routes = HashMap::new();
        #[cfg(test)]
        let mut peer_states = HashMap::new();
        for peer in &config.peers {
            let (conn_tx, conn_rx) = mpsc::channel(4);
            routes.insert(peer.id, conn_tx);
            let peer_task = peer::PeerTask::new(
                endpoint.clone(),
                peer.id,
                &peer.addrs,
                discovery,
                outbox.clone(),
                inbox_tx.clone(),
                links.clone(),
            );
            #[cfg(test)]
            peer_states.insert(peer.id, peer_task.state());
            tasks.spawn(peer_task.run(conn_rx));
        }
        tasks.spawn(accept_loop(endpoint.clone(), routes));

        let node = Self {
            endpoint,
            outbox,
            links,
            lan_discovery: discovery,
            tasks,
            #[cfg(test)]
            peers: peer_states,
        };
        Ok((node, inbox))
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
    /// # Errors
    ///
    /// [`FrameError::TooLarge`] if the clip does not fit in a frame.
    pub fn broadcast(&self, clip: Clip) -> Result<(), FrameError> {
        let frame = protocol::encode(&Message::Clip(clip))?;
        // An error only means that no peer is connected.
        let _ = self.outbox.send(frame.into());
        Ok(())
    }

    /// Closes every connection, telling peers right away, and stops the node.
    pub async fn shutdown(mut self) {
        self.tasks.shutdown().await;
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

/// Rejects connections from anyone who is not a configured peer.
#[derive(Debug)]
struct Allowlist(HashSet<EndpointId>);

impl EndpointHooks for Allowlist {
    fn after_handshake<'a>(
        &'a self,
        conn: &'a Connection,
    ) -> impl Future<Output = AfterHandshakeOutcome> + Send + 'a {
        let remote = conn.remote_id();
        let outcome = if self.0.contains(&remote) {
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

/// Accepts incoming connections and hands each one to its peer's task.
async fn accept_loop(endpoint: Endpoint, routes: HashMap<EndpointId, mpsc::Sender<Connection>>) {
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
                // The allowlist only lets peers through, and every peer has a route.
                if let Some(route) = routes.get(&conn.remote_id()) {
                    // An error means the node is stopping.
                    let _ = route.send(conn).await;
                }
            }
        }
    }
}
