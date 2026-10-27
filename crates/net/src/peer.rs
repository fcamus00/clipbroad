//! One task per peer: dials it, adopts its incoming connections and runs a session on each.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use iroh::{Endpoint, EndpointAddr, EndpointId, TransportAddr, endpoint::Connection};
use tokio::{
    sync::{broadcast, mpsc, watch},
    task::JoinSet,
    time::{self, Instant},
};
use tracing::{debug, info};

use crate::{ALPN, Frame, Link, Links, Received, close, files::Offer, session};

/// Delay before the first redial, and after a session that was up ends.
const MIN_BACKOFF: Duration = Duration::from_millis(500);
/// Longest delay between dials to a peer that stays unreachable.
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Longest a single dial may take.
const DIAL_TIMEOUT: Duration = Duration::from_secs(5);

/// The node-wide channels every peer task uses.
#[derive(Clone)]
pub(crate) struct Channels {
    /// Clips to send to every peer.
    pub(crate) outbox: broadcast::Sender<Frame>,
    /// Clips received from any peer.
    pub(crate) inbox: mpsc::Sender<Received>,
    /// Files of the latest broadcast clip.
    pub(crate) offer: watch::Receiver<Option<Arc<Offer>>>,
    /// Where each peer's [`Link`] is published.
    pub(crate) links: watch::Sender<Links>,
}

pub(crate) struct PeerTask {
    endpoint: Endpoint,
    id: EndpointId,
    /// Where to dial the peer, or `None` if it has no static address and LAN discovery is off (it
    /// can still dial us). With discovery on, iroh asks mDNS for the peer's current addresses and
    /// tries them along with the static ones.
    addr: Option<EndpointAddr>,
    outbox: broadcast::Sender<Frame>,
    inbox: mpsc::Sender<Received>,
    offer: watch::Receiver<Option<Arc<Offer>>>,
    state: Arc<PeerState>,
}

impl PeerTask {
    pub(crate) fn new(
        endpoint: Endpoint,
        id: EndpointId,
        addrs: &[SocketAddr],
        discovery: bool,
        channels: Channels,
    ) -> Self {
        let Channels {
            outbox,
            inbox,
            offer,
            links,
        } = channels;
        let addr = (discovery || !addrs.is_empty()).then(|| {
            addrs
                .iter()
                .fold(EndpointAddr::new(id), |addr, ip| addr.with_ip_addr(*ip))
        });
        let state = PeerState {
            id,
            links,
            // Only the node with the smaller id closes duplicate connections (see the crate docs).
            closes_duplicates: endpoint.id() < id,
            live_sessions: AtomicUsize::new(0),
            newest: watch::Sender::new(None),
        };
        Self {
            endpoint,
            id,
            addr,
            outbox,
            inbox,
            offer,
            state: Arc::new(state),
        }
    }

    pub(crate) fn state(&self) -> Arc<PeerState> {
        Arc::clone(&self.state)
    }

    /// Runs until the node stops. `incoming` brings the connections this peer opened to us.
    pub(crate) async fn run(self, mut incoming: mpsc::Receiver<Connection>) {
        // Each session returns whether the peer completed the `Hello` exchange.
        let mut sessions = JoinSet::new();
        let mut backoff = MIN_BACKOFF;
        let mut next_dial = Instant::now();

        loop {
            let dial_now = sessions.is_empty() && self.addr.is_some();
            let conn = tokio::select! {
                Some(conn) = incoming.recv() => conn,
                Some(ended) = sessions.join_next() => {
                    // A session that got past `Hello` was healthy: the peer just went away, so
                    // try again soon. A failed handshake keeps growing the delay.
                    if matches!(ended, Ok(true)) {
                        backoff = MIN_BACKOFF;
                    }
                    if sessions.is_empty() {
                        next_dial = Instant::now() + backoff;
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                    }
                    continue;
                }
                dialed = self.dial(next_dial), if dial_now => {
                    let Some(conn) = dialed else {
                        next_dial = Instant::now() + backoff;
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                        continue;
                    };
                    conn
                }
                else => break,
            };

            sessions.spawn(session::run(
                conn,
                self.outbox.subscribe(),
                self.inbox.clone(),
                self.offer.clone(),
                Arc::clone(&self.state),
            ));
        }
    }

    /// Waits until `at`, then dials the peer. Must only be called when it can be dialed.
    async fn dial(&self, at: Instant) -> Option<Connection> {
        let addr = self.addr.clone()?;
        time::sleep_until(at).await;
        match time::timeout(DIAL_TIMEOUT, self.endpoint.connect(addr, ALPN)).await {
            Ok(Ok(conn)) => Some(conn),
            Ok(Err(e)) => {
                debug!(peer = %self.id.fmt_short(), error = %e, "dial failed");
                None
            }
            Err(_) => {
                debug!(peer = %self.id.fmt_short(), "dial timed out");
                None
            }
        }
    }
}

/// What the sessions with one peer share.
#[derive(Debug)]
pub(crate) struct PeerState {
    id: EndpointId,
    /// Where this peer's [`Link`] is published (see [`Node::links`](crate::Node::links)).
    links: watch::Sender<Links>,
    closes_duplicates: bool,
    /// Sessions that finished the `Hello` exchange and are still up. Only changed while holding
    /// the `links` lock, so the published link always matches it.
    live_sessions: AtomicUsize,
    /// The connection that most recently finished the `Hello` exchange. File transfers use it,
    /// and when this node closes duplicates, it is the one that stays.
    newest: watch::Sender<Option<Connection>>,
}

impl PeerState {
    /// Records that `conn` finished the `Hello` exchange. The session counts as live until the
    /// returned guard is dropped.
    ///
    /// A connection only replaces the previous one once it is known to work: a dial that was
    /// abandoned can still complete on the other side, and must not push out a good connection.
    pub(crate) fn greeted(&self, conn: &Connection) -> LiveSession<'_> {
        info!(peer = %conn.remote_id().fmt_short(), "connected");
        let old = self.newest.send_replace(Some(conn.clone()));
        if self.closes_duplicates
            && let Some(old) = old
            && old.close_reason().is_none()
        {
            debug!(peer = %conn.remote_id().fmt_short(), "closing older connection");
            old.close(close::REPLACED, b"replaced by a newer connection");
        }
        let addr = selected_ip(conn);
        self.links.send_modify(|links| {
            self.live_sessions.fetch_add(1, Ordering::Relaxed);
            links.insert(self.id, Link::Connected { addr });
        });
        LiveSession(self)
    }

    /// Follows the connection that most recently finished the `Hello` exchange, which may have
    /// closed since.
    pub(crate) fn newest(&self) -> watch::Receiver<Option<Connection>> {
        self.newest.subscribe()
    }

    #[cfg(test)]
    pub(crate) fn live_sessions(&self) -> usize {
        self.live_sessions.load(Ordering::Relaxed)
    }
}

/// Keeps a session counted as live while it exists. The last one to go marks the peer
/// disconnected.
pub(crate) struct LiveSession<'a>(&'a PeerState);

impl Drop for LiveSession<'_> {
    fn drop(&mut self) {
        let peer = self.0;
        peer.links.send_if_modified(|links| {
            let was_last = peer.live_sessions.fetch_sub(1, Ordering::Relaxed) == 1;
            if was_last {
                links.insert(peer.id, Link::Disconnected);
            }
            was_last
        });
    }
}

/// The peer's address on the path `conn` sends on, if it is a direct IP path.
fn selected_ip(conn: &Connection) -> Option<SocketAddr> {
    let paths = conn.paths();
    let selected = paths.iter().find(iroh::endpoint::Path::is_selected)?;
    match selected.remote_addr() {
        TransportAddr::Ip(addr) => Some(*addr),
        _ => None,
    }
}
