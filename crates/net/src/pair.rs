//! Pairing: two machines learn each other's id so they can add each other to their configs.
//!
//! Machine A starts a [`Host`], shows its [`Ticket`] and waits. Machine B calls [`join`] with that
//! ticket: it dials A on [`PAIR_ALPN`] and sends the ticket's secret, its name and its addresses.
//! If the secret matches, A answers with its own name and both sides get a [`PairedPeer`]. B
//! already has A's addresses from the ticket.
//!
//! iroh's TLS already proves both ids, so the secret only proves that B got the ticket from A's
//! screen. A host accepts one valid request and then stops listening; a wrong secret only closes
//! that connection, and the host keeps waiting until its time runs out. Both sides listen on
//! their normal port, so the addresses they report, and the one B connects from, are the ones
//! to save.
//!
//! Either side can run on its own endpoint ([`Host::bind`], [`join`]) or on the endpoint of a
//! running [`Node`] ([`Node::host_pairing`], [`Node::join_pairing`]).

use std::{fmt, net::SocketAddr, num::NonZeroU16, str::FromStr, time::Duration};

use clipbroad_config::IdentityKey;
use data_encoding::BASE32_NOPAD_NOCASE;
use iroh::{
    Endpoint, EndpointAddr, EndpointId, SecretKey, TransportAddr,
    endpoint::{
        ClosedStream, ConnectError, ConnectingError, Connection, ConnectionError, Incoming,
        ReadToEndError, SendStream, VarInt, WriteError,
    },
};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq as _;
use tokio::{sync::mpsc, time};
use tracing::warn;

use crate::{Error, Node, close, endpoint_builder, lock};

/// ALPN of the pairing protocol. Only a [`Host`] offers it.
pub const PAIR_ALPN: &[u8] = b"clipbroad-pair/1";

const TICKET_VERSION: u8 = 1;
const SECRET_LEN: usize = 16;
/// Pairing messages hold a secret, a name and a few addresses; anything bigger is not one.
const MAX_MESSAGE_LEN: usize = 4096;
/// Longest one pairing attempt may take on the host, so a silent client cannot hold it.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest [`join`] waits for the host. Shorter in tests, where a used or expired ticket is
/// expected to time out.
const JOIN_TIMEOUT: Duration = Duration::from_secs(if cfg!(test) { 2 } else { 10 });
/// How long the host waits for the joiner to read the reply before it closes.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
/// Close code when the request had the wrong secret.
const WRONG_SECRET: VarInt = VarInt::from_u32(10);

/// What a host shows: how to reach it and the secret that proves the joiner saw it.
///
/// Written as one line of base32 (see [`Display`](fmt::Display) and [`FromStr`]). `Debug` hides
/// the secret.
#[derive(Clone, PartialEq, Eq)]
pub struct Ticket {
    pub id: EndpointId,
    /// The host's addresses, all on its listening port.
    pub addrs: Vec<SocketAddr>,
    secret: [u8; SECRET_LEN],
}

/// The ticket as it is encoded. The version comes first so that a newer layout is reported as
/// such instead of as garbage.
#[derive(Serialize, Deserialize)]
struct TicketWire {
    version: u8,
    id: [u8; 32],
    addrs: Vec<SocketAddr>,
    secret: [u8; SECRET_LEN],
}

/// Why a ticket text was rejected.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TicketError {
    #[error("not a clipbroad ticket: it should be one line of letters and digits")]
    Encoding(#[from] data_encoding::DecodeError),
    #[error("ticket version {0} is not supported; use the same clipbroad version on both machines")]
    Version(u8),
    #[error("not a clipbroad ticket (cut off when copying?)")]
    Invalid,
}

/// The other machine, as learned while pairing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedPeer {
    pub id: EndpointId,
    /// The name the other machine gave, usually its hostname.
    pub name: Option<String>,
    /// Its addresses, all on its listening port.
    pub addrs: Vec<SocketAddr>,
}

/// Why pairing failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PairError {
    #[error(transparent)]
    Start(#[from] Error),
    #[error("nobody used the ticket in time; run `clipbroad pair` again for a new one")]
    Expired,
    #[error("the ticket belongs to this machine; use it on the other one")]
    OwnTicket,
    #[error("another pairing is already waiting for its ticket; finish or cancel it first")]
    Busy,
    #[error(
        "the other machine rejected the ticket's secret; check that it was copied whole, or get \
         a new one"
    )]
    WrongSecret,
    #[error(
        "could not reach the other machine within {JOIN_TIMEOUT:?}; the ticket may be used or \
         expired, or the machines cannot reach each other"
    )]
    Unreachable,
    #[error("could not connect")]
    Connect(#[from] ConnectError),
    #[error("connection failed")]
    Connecting(#[from] ConnectingError),
    #[error("connection lost")]
    Connection(#[from] ConnectionError),
    #[error("connection lost")]
    Write(#[from] WriteError),
    #[error("connection lost")]
    Read(#[from] ReadToEndError),
    #[error("connection lost")]
    Closed(#[from] ClosedStream),
    #[error("invalid pairing message")]
    Message(#[from] postcard::Error),
}

/// First message, from the joiner.
#[derive(Serialize, Deserialize)]
struct JoinMessage {
    secret: [u8; SECRET_LEN],
    name: Option<String>,
    addrs: Vec<SocketAddr>,
}

/// The host's reply when the secret matches.
#[derive(Serialize, Deserialize)]
struct WelcomeMessage {
    name: Option<String>,
}

impl Ticket {
    fn new(id: EndpointId, addrs: Vec<SocketAddr>) -> Result<Self, Error> {
        let mut secret = [0; SECRET_LEN];
        getrandom::fill(&mut secret).map_err(Error::Random)?;
        Ok(Self { id, addrs, secret })
    }
}

impl fmt::Display for Ticket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let wire = TicketWire {
            version: TICKET_VERSION,
            id: *self.id.as_bytes(),
            addrs: self.addrs.clone(),
            secret: self.secret,
        };
        let bytes = postcard::to_stdvec(&wire).map_err(|_| fmt::Error)?;
        f.write_str(&BASE32_NOPAD_NOCASE.encode(&bytes))
    }
}

impl FromStr for Ticket {
    type Err = TicketError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let bytes = BASE32_NOPAD_NOCASE.decode(text.trim().as_bytes())?;
        match bytes.first() {
            Some(&TICKET_VERSION) => {}
            Some(&version) => return Err(TicketError::Version(version)),
            None => return Err(TicketError::Invalid),
        }
        let wire: TicketWire = postcard::from_bytes(&bytes).map_err(|_| TicketError::Invalid)?;
        Ok(Self {
            id: EndpointId::from_bytes(&wire.id).map_err(|_| TicketError::Invalid)?,
            addrs: wire.addrs,
            secret: wire.secret,
        })
    }
}

impl fmt::Debug for Ticket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ticket")
            .field("id", &self.id)
            .field("addrs", &self.addrs)
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// The machine that shows the ticket and waits for the other one.
#[derive(Debug)]
pub struct Host {
    requests: Requests,
    ticket: Ticket,
    name: Option<String>,
}

/// Where a host's pairing requests come from.
#[derive(Debug)]
enum Requests {
    /// Its own endpoint, which only offers [`PAIR_ALPN`].
    Own(Endpoint),
    /// The endpoint of a running [`Node`], which hands over its pairing connections while this
    /// receiver exists.
    Node(mpsc::Receiver<Connection>),
}

/// A pairing attempt, before or after its handshake.
enum Request {
    /// Boxed: it is five times the size of a connection, and attempts come at human speed.
    Incoming(Box<Incoming>),
    Connected(Connection),
}

impl Requests {
    /// The next attempt, or `None` once the endpoint or the node closed.
    async fn next(&mut self) -> Option<Request> {
        match self {
            Self::Own(endpoint) => endpoint
                .accept()
                .await
                .map(|incoming| Request::Incoming(Box::new(incoming))),
            Self::Node(connections) => connections.recv().await.map(Request::Connected),
        }
    }

    /// Stops taking requests.
    async fn close(self) {
        match self {
            Self::Own(endpoint) => endpoint.close().await,
            // Dropping the receiver tells the node to refuse pairing connections again.
            Self::Node(_) => {}
        }
    }
}

impl Request {
    async fn connection(self) -> Result<Connection, PairError> {
        match self {
            Self::Incoming(incoming) => Ok((*incoming).await?),
            Self::Connected(conn) => Ok(conn),
        }
    }
}

/// A valid pairing request that the host has not answered yet. [`Pending::confirm`] answers it;
/// dropping it rejects it.
#[derive(Debug)]
pub struct Pending {
    requests: Requests,
    conn: Connection,
    send: SendStream,
    peer: PairedPeer,
    name: Option<String>,
}

impl Host {
    /// Listens on `port` with a new random secret. `name` is sent to the joiner.
    ///
    /// # Errors
    ///
    /// [`Error::Bind`] if the port is in use (for example by `clipbroad run`), [`Error::Random`] if
    /// no secret can be generated.
    pub async fn bind(
        identity: &IdentityKey,
        port: NonZeroU16,
        name: Option<String>,
    ) -> Result<Self, Error> {
        let port = port.get();
        let endpoint = endpoint_builder(identity, port)
            .alpns(vec![PAIR_ALPN.to_vec()])
            .bind()
            .await
            .map_err(|source| Error::Bind { port, source })?;
        let addrs = endpoint.addr().ip_addrs().copied().collect();
        let ticket = Ticket::new(endpoint.id(), addrs)?;
        Ok(Self {
            requests: Requests::Own(endpoint),
            ticket,
            name,
        })
    }

    #[must_use]
    pub fn ticket(&self) -> &Ticket {
        &self.ticket
    }

    /// Waits up to `ttl` for a request with the right secret. Requests with a wrong secret are
    /// rejected and logged, and the wait goes on.
    ///
    /// # Errors
    ///
    /// [`PairError::Expired`] if no valid request came in time. The host stops listening either
    /// way: a ticket works once.
    pub async fn accept(mut self, ttl: Duration) -> Result<Pending, PairError> {
        let Ok(Some((conn, send, peer))) = time::timeout(ttl, self.first_valid()).await else {
            self.requests.close().await;
            return Err(PairError::Expired);
        };
        Ok(Pending {
            requests: self.requests,
            conn,
            send,
            peer,
            name: self.name,
        })
    }

    /// Takes attempts one at a time until one has the right secret. `None` if the endpoint or
    /// the node closed.
    async fn first_valid(&mut self) -> Option<(Connection, SendStream, PairedPeer)> {
        while let Some(request) = self.requests.next().await {
            match time::timeout(REQUEST_TIMEOUT, self.check(request)).await {
                Ok(Ok(valid)) => return Some(valid),
                Ok(Err(PairError::WrongSecret)) => {
                    warn!("rejected a pairing attempt with the wrong secret");
                }
                Ok(Err(error)) => warn!(%error, "a pairing attempt failed"),
                Err(_) => warn!("a pairing attempt timed out"),
            }
        }
        None
    }

    async fn check(
        &self,
        request: Request,
    ) -> Result<(Connection, SendStream, PairedPeer), PairError> {
        let conn = request.connection().await?;
        let (send, mut recv) = conn.accept_bi().await?;
        let request: JoinMessage = postcard::from_bytes(&recv.read_to_end(MAX_MESSAGE_LEN).await?)?;
        if !bool::from(request.secret.ct_eq(&self.ticket.secret)) {
            conn.close(WRONG_SECRET, b"wrong secret");
            return Err(PairError::WrongSecret);
        }
        let mut addrs = request.addrs;
        // It connected from its listening port. The address may be missing from its own list,
        // e.g. on localhost.
        if let Some(addr) = direct_addr(&conn)
            && !addrs.contains(&addr)
        {
            addrs.insert(0, addr);
        }
        let peer = PairedPeer {
            id: conn.remote_id(),
            name: request.name,
            addrs,
        };
        Ok((conn, send, peer))
    }
}

impl Pending {
    /// The machine asking to pair.
    #[must_use]
    pub fn peer(&self) -> &PairedPeer {
        &self.peer
    }

    /// Tells the joiner that pairing succeeded, and stops the host.
    ///
    /// # Errors
    ///
    /// If the connection broke before the reply was sent.
    pub async fn confirm(mut self) -> Result<PairedPeer, PairError> {
        let reply = postcard::to_stdvec(&WelcomeMessage { name: self.name })?;
        self.send.write_all(&reply).await?;
        self.send.finish()?;
        // The joiner closes once it has the reply; closing first could cut it off.
        let _ = time::timeout(CLOSE_TIMEOUT, self.conn.closed()).await;
        self.requests.close().await;
        Ok(self.peer)
    }
}

impl Node {
    /// Starts pairing as the host on this node's endpoint, so that pairing needs no other
    /// process on the port. Then use the [`Host`] as one from [`Host::bind`]. Until it is
    /// dropped, the node passes it every connection to [`PAIR_ALPN`]; without a host, it refuses
    /// them.
    ///
    /// The node does not add the peer by itself: once it is saved, call [`Node::add_peer`].
    ///
    /// # Errors
    ///
    /// [`PairError::Busy`] if another host of this node is still waiting, and
    /// [`PairError::Start`] if no secret can be generated.
    pub fn host_pairing(&self, name: Option<String>) -> Result<Host, PairError> {
        let mut pairing = lock(&self.routes.pairing);
        if pairing.as_ref().is_some_and(|host| !host.is_closed()) {
            return Err(PairError::Busy);
        }
        let addrs = self.endpoint.addr().ip_addrs().copied().collect();
        let ticket = Ticket::new(self.endpoint.id(), addrs)?;
        let (requests, received) = mpsc::channel(4);
        *pairing = Some(requests);
        Ok(Host {
            requests: Requests::Node(received),
            ticket,
            name,
        })
    }

    /// Pairs with the host in `ticket` from this node's endpoint, like [`join`] does from its
    /// own. Then save the peer and call [`Node::add_peer`].
    ///
    /// # Errors
    ///
    /// As [`join`].
    pub async fn join_pairing(
        &self,
        ticket: &Ticket,
        name: Option<String>,
    ) -> Result<PairedPeer, PairError> {
        if self.endpoint.id() == ticket.id {
            return Err(PairError::OwnTicket);
        }
        join_from(&self.endpoint, ticket, name).await
    }
}

/// Pairs with the host in `ticket`, listening on `port` so the host sees this machine's listening
/// address. `name` is sent to the host.
///
/// # Errors
///
/// [`PairError::WrongSecret`] if the host rejects the secret, [`PairError::Unreachable`] if it does
/// not answer (also the case for a used or expired ticket: nobody listens anymore), and the
/// other variants if the connection breaks.
pub async fn join(
    identity: &IdentityKey,
    port: NonZeroU16,
    ticket: &Ticket,
    name: Option<String>,
) -> Result<PairedPeer, PairError> {
    if SecretKey::from_bytes(&identity.to_bytes()).public() == ticket.id {
        return Err(PairError::OwnTicket);
    }
    let port = port.get();
    let endpoint = endpoint_builder(identity, port)
        .bind()
        .await
        .map_err(|source| Error::Bind { port, source })?;
    let result = join_from(&endpoint, ticket, name).await;
    endpoint.close().await;
    result
}

async fn join_from(
    endpoint: &Endpoint,
    ticket: &Ticket,
    name: Option<String>,
) -> Result<PairedPeer, PairError> {
    time::timeout(JOIN_TIMEOUT, exchange(endpoint, ticket, name))
        .await
        .unwrap_or(Err(PairError::Unreachable))
}

async fn exchange(
    endpoint: &Endpoint,
    ticket: &Ticket,
    name: Option<String>,
) -> Result<PairedPeer, PairError> {
    let addr = ticket
        .addrs
        .iter()
        .fold(EndpointAddr::new(ticket.id), |addr, ip| {
            addr.with_ip_addr(*ip)
        });
    let conn = endpoint.connect(addr, PAIR_ALPN).await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    let request = JoinMessage {
        secret: ticket.secret,
        name,
        addrs: endpoint.addr().ip_addrs().copied().collect(),
    };
    send.write_all(&postcard::to_stdvec(&request)?).await?;
    send.finish()?;

    let reply = match recv.read_to_end(MAX_MESSAGE_LEN).await {
        Ok(reply) => reply,
        Err(_) if was_wrong_secret(&conn) => return Err(PairError::WrongSecret),
        Err(e) => return Err(e.into()),
    };
    let welcome: WelcomeMessage = postcard::from_bytes(&reply)?;
    let peer = PairedPeer {
        id: conn.remote_id(),
        name: welcome.name,
        addrs: ticket.addrs.clone(),
    };
    conn.close(close::NORMAL, b"");
    Ok(peer)
}

fn was_wrong_secret(conn: &Connection) -> bool {
    matches!(
        conn.close_reason(),
        Some(ConnectionError::ApplicationClosed(close)) if close.error_code == WRONG_SECRET
    )
}

/// The IP address of the other side of `conn`: the one of the path in use, else of any path.
fn direct_addr(conn: &Connection) -> Option<SocketAddr> {
    let paths = conn.paths();
    let selected = paths.iter().filter(iroh::endpoint::Path::is_selected);
    selected
        .chain(paths.iter())
        .find_map(|path| match path.remote_addr() {
            TransportAddr::Ip(addr) => Some(*addr),
            _ => None,
        })
}

#[cfg(test)]
mod tests;
