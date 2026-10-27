//! One task per peer: dials it, adopts its incoming connections and runs a session on each.

use std::{
    net::SocketAddr,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use iroh::{Endpoint, EndpointAddr, EndpointId, endpoint::Connection};
use tokio::{
    sync::{broadcast, mpsc},
    task::JoinSet,
    time::{self, Instant},
};
use tracing::{debug, info};

use crate::{ALPN, Frame, Received, close, session};

/// Delay before the first redial, and after a session that was up ends.
const MIN_BACKOFF: Duration = Duration::from_millis(500);
/// Longest delay between dials to a peer that stays unreachable.
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Longest a single dial may take.
const DIAL_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct PeerTask {
    endpoint: Endpoint,
    id: EndpointId,
    /// Where to dial the peer, or `None` if it has no static address and LAN discovery is off (it
    /// can still dial us). With discovery on, iroh asks mDNS for the peer's current addresses and
    /// tries them along with the static ones.
    addr: Option<EndpointAddr>,
    outbox: broadcast::Sender<Frame>,
    inbox: mpsc::Sender<Received>,
    state: Arc<PeerState>,
}

impl PeerTask {
    pub(crate) fn new(
        endpoint: Endpoint,
        id: EndpointId,
        addrs: &[SocketAddr],
        discovery: bool,
        outbox: broadcast::Sender<Frame>,
        inbox: mpsc::Sender<Received>,
    ) -> Self {
        let addr = (discovery || !addrs.is_empty()).then(|| {
            addrs
                .iter()
                .fold(EndpointAddr::new(id), |addr, ip| addr.with_ip_addr(*ip))
        });
        let state = PeerState {
            // Only the node with the smaller id closes duplicate connections (see the crate docs).
            closes_duplicates: endpoint.id() < id,
            live_sessions: AtomicUsize::new(0),
            newest: Mutex::new(None),
        };
        Self {
            endpoint,
            id,
            addr,
            outbox,
            inbox,
            state: Arc::new(state),
        }
    }

    #[cfg(test)]
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
    closes_duplicates: bool,
    /// Sessions that finished the `Hello` exchange and are still up.
    live_sessions: AtomicUsize,
    /// The connection that most recently finished the `Hello` exchange, kept only when this node
    /// closes duplicates.
    newest: Mutex<Option<Connection>>,
}

impl PeerState {
    /// Records that `conn` finished the `Hello` exchange. The session counts as live until the
    /// returned guard is dropped.
    ///
    /// A connection only replaces the previous one once it is known to work: a dial that was
    /// abandoned can still complete on the other side, and must not push out a good connection.
    pub(crate) fn greeted(&self, conn: &Connection) -> LiveSession<'_> {
        info!(peer = %conn.remote_id().fmt_short(), "connected");
        if self.closes_duplicates {
            let mut newest = self.newest.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(old) = newest.replace(conn.clone())
                && old.close_reason().is_none()
            {
                debug!(peer = %conn.remote_id().fmt_short(), "closing older connection");
                old.close(close::REPLACED, b"replaced by a newer connection");
            }
        }
        self.live_sessions.fetch_add(1, Ordering::Relaxed);
        LiveSession(&self.live_sessions)
    }

    #[cfg(test)]
    pub(crate) fn live_sessions(&self) -> usize {
        self.live_sessions.load(Ordering::Relaxed)
    }
}

/// Keeps a session counted as live while it exists.
pub(crate) struct LiveSession<'a>(&'a AtomicUsize);

impl Drop for LiveSession<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}
