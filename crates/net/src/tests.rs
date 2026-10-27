//! Nodes talking to each other over localhost, plus raw iroh endpoints that act as peers to test
//! how a node reacts to things a real node never does.

use std::{
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    num::NonZeroU16,
    time::Duration,
};

use clipbroad_config::Peer;
use clipbroad_core::{
    ClipContent,
    protocol::{ClipId, Hello, NodeId},
};
use iroh::{
    EndpointAddr, Watcher as _,
    endpoint::{ConnectError, ConnectingError, ConnectionError, RecvStream, SendStream, VarInt},
};
use tokio::time;

use super::*;

/// Upper bound for anything a test waits on. Localhost takes milliseconds; this only stops a
/// broken test from hanging CI.
const TIMEOUT: Duration = Duration::from_secs(10);

/// A node before it starts: its identity and the port it will listen on.
struct Spec {
    key: IdentityKey,
    port: NonZeroU16,
}

impl Spec {
    fn new() -> Self {
        Self {
            key: IdentityKey::generate().expect("random key"),
            port: free_port(),
        }
    }

    /// Two specs, the one with the smaller id first.
    fn ordered_pair() -> (Self, Self) {
        let (a, b) = (Self::new(), Self::new());
        if a.id() < b.id() { (a, b) } else { (b, a) }
    }

    fn id(&self) -> EndpointId {
        self.secret().public()
    }

    fn secret(&self) -> SecretKey {
        SecretKey::from_bytes(&self.key.to_bytes())
    }

    fn addr(&self) -> SocketAddr {
        (Ipv4Addr::LOCALHOST, self.port.get()).into()
    }

    /// This node as a config entry, dialable at `addr`.
    fn peer(&self) -> Peer {
        Peer {
            id: self.id(),
            name: None,
            addrs: vec![self.addr()],
        }
    }

    /// This node as a config entry the node cannot dial (it has to dial in).
    fn peer_without_addrs(&self) -> Peer {
        Peer {
            addrs: Vec::new(),
            ..self.peer()
        }
    }

    /// Starts the node without mDNS, so it reaches peers only at their static addresses.
    async fn spawn(&self, peers: Vec<Peer>) -> (Node, mpsc::Receiver<Received>) {
        self.spawn_with(peers, false).await
    }

    async fn spawn_with(&self, peers: Vec<Peer>, mdns: bool) -> (Node, mpsc::Receiver<Received>) {
        let config = Config {
            listen_port: self.port,
            mdns,
            peers,
            ..Config::default()
        };
        let (node, inbox) = Node::spawn(&self.key, &config).await.expect("node starts");
        assert_eq!(node.id(), self.id());
        (node, inbox)
    }
}

/// A UDP port that was free a moment ago. Nodes need their ports up front so each one's config
/// can list the others.
fn free_port() -> NonZeroU16 {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("bind a free port");
    let port = socket.local_addr().expect("local address").port();
    NonZeroU16::new(port).expect("the OS never assigns port 0")
}

fn text_clip(origin: EndpointId, id: u64, text: &str) -> Clip {
    Clip::new(
        ClipId(id),
        NodeId::from_bytes(*origin.as_bytes()),
        1_700_000_000_000,
        ClipContent::from(text),
    )
}

/// Waits for the next clip and returns who sent it and its verified text.
async fn recv_text(inbox: &mut mpsc::Receiver<Received>) -> (EndpointId, String) {
    let received = time::timeout(TIMEOUT, inbox.recv())
        .await
        .expect("a clip arrives in time")
        .expect("node still running");
    let content = received.clip.into_content().expect("clip is intact");
    let text = content.as_text().expect("text clip").to_owned();
    (received.from, text)
}

/// Polls `condition` until it holds.
async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let poll = async {
        while !condition() {
            time::sleep(Duration::from_millis(20)).await;
        }
    };
    let done = time::timeout(TIMEOUT, poll).await;
    assert!(done.is_ok(), "timed out waiting until {what}");
}

impl Node {
    fn live_sessions_with(&self, peer: EndpointId) -> usize {
        self.peers[&peer].live_sessions()
    }

    /// Whether this node has exactly one working connection to each of `peers`.
    fn connected_once_to(&self, peers: &[&Spec]) -> bool {
        peers.iter().all(|p| self.live_sessions_with(p.id()) == 1)
    }
}

/// An iroh endpoint that is not a clipbroad node, used to dial a node directly.
async fn raw_endpoint(spec: &Spec) -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .secret_key(spec.secret())
        .clear_ip_transports()
        .bind_addr(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .expect("valid bind address")
        .bind()
        .await
        .expect("raw endpoint binds")
}

async fn raw_connect(endpoint: &Endpoint, node: &Spec) -> Connection {
    let addr = EndpointAddr::new(node.id()).with_ip_addr(node.addr());
    time::timeout(TIMEOUT, endpoint.connect(addr, ALPN))
        .await
        .expect("connect in time")
        .expect("handshake succeeds")
}

/// Does the `Hello` exchange by hand. Returns our stream and the node's.
async fn raw_hello(conn: &Connection) -> (SendStream, RecvStream) {
    let mut send = conn.open_uni().await.expect("open stream");
    write(&mut send, &Message::Hello(Hello::CURRENT)).await;
    let mut recv = time::timeout(TIMEOUT, conn.accept_uni())
        .await
        .expect("node opens its stream in time")
        .expect("node stream");
    assert_eq!(read(&mut recv).await, Message::Hello(Hello::CURRENT));
    (send, recv)
}

async fn write(send: &mut SendStream, message: &Message) {
    let frame = protocol::encode(message).expect("encode");
    send.write_all(&frame).await.expect("write frame");
}

async fn read(recv: &mut RecvStream) -> Message {
    let mut buf = Vec::new();
    let read = async {
        loop {
            if let Some((message, _)) = protocol::decode(&buf).expect("valid frame") {
                return message;
            }
            let chunk = recv.read_chunk(usize::MAX).await.expect("read");
            buf.extend_from_slice(&chunk.expect("stream still open"));
        }
    };
    time::timeout(TIMEOUT, read).await.expect("frame in time")
}

/// Waits for the connection to close and returns the code the closing side sent, if any.
async fn close_code(conn: &Connection) -> Option<VarInt> {
    let reason = time::timeout(TIMEOUT, conn.closed())
        .await
        .expect("connection closes in time");
    match reason {
        ConnectionError::ApplicationClosed(close) => Some(close.error_code),
        _ => None,
    }
}

#[tokio::test]
async fn no_relays_and_no_address_lookup() {
    let (node, _inbox) = Spec::new().spawn(Vec::new()).await;

    let lookup = node.endpoint.address_lookup().expect("endpoint open");
    assert!(lookup.is_empty(), "no DNS/pkarr address lookup configured");
    assert!(
        node.endpoint.home_relay_status().get().is_empty(),
        "no relays"
    );
    assert_eq!(node.endpoint.addr().relay_urls().count(), 0);
    node.shutdown().await;
}

#[tokio::test]
async fn mdns_is_the_only_address_lookup() {
    let (node, _inbox) = Spec::new().spawn_with(Vec::new(), true).await;

    let lookup = node.endpoint.address_lookup().expect("endpoint open");
    assert_eq!(lookup.len(), 1, "only mDNS, no DNS/pkarr");
    assert!(
        node.endpoint.home_relay_status().get().is_empty(),
        "no relays"
    );
    node.shutdown().await;
}

#[tokio::test]
#[ignore = "needs multicast between local sockets, which CI runners may not allow; run with \
            `cargo nextest run -p clipbroad-net --run-ignored only`"]
async fn nodes_find_each_other_with_mdns() {
    let (a, b) = (Spec::new(), Spec::new());
    // Neither knows where the other listens: only mDNS can connect them.
    let (node_a, mut inbox_a) = a.spawn_with(vec![b.peer_without_addrs()], true).await;
    let (node_b, mut inbox_b) = b.spawn_with(vec![a.peer_without_addrs()], true).await;

    // Finding a peer can take a few mDNS rounds plus a redial, so allow more than `TIMEOUT`.
    let connected = time::timeout(Duration::from_secs(30), async {
        while !(node_a.connected_once_to(&[&b]) && node_b.connected_once_to(&[&a])) {
            time::sleep(Duration::from_millis(50)).await;
        }
    });
    assert!(connected.await.is_ok(), "nodes found each other in time");

    node_a
        .broadcast(text_clip(a.id(), 1, "from a"))
        .expect("send");
    node_b
        .broadcast(text_clip(b.id(), 1, "from b"))
        .expect("send");
    assert_eq!(recv_text(&mut inbox_b).await, (a.id(), "from a".to_owned()));
    assert_eq!(recv_text(&mut inbox_a).await, (b.id(), "from b".to_owned()));

    node_a.shutdown().await;
    node_b.shutdown().await;
}

#[tokio::test]
async fn three_nodes_exchange_clips() {
    let specs = [Spec::new(), Spec::new(), Spec::new()];
    let mut nodes = Vec::new();
    for spec in &specs {
        let others = specs.iter().filter(|s| s.id() != spec.id());
        nodes.push(spec.spawn(others.map(Spec::peer).collect()).await);
    }
    for (i, (node, _)) in nodes.iter().enumerate() {
        let others: Vec<_> = specs.iter().filter(|s| s.id() != specs[i].id()).collect();
        wait_until("all nodes are connected", || {
            node.connected_once_to(&others)
        })
        .await;
    }

    for (sender, (node, _)) in nodes.iter().enumerate() {
        for n in 0..2 {
            let clip = text_clip(node.id(), n, &format!("from {sender}, #{n}"));
            node.broadcast(clip).expect("small clip");
        }
    }

    // Each node gets both clips of each other node exactly once, in the order they were sent.
    for (receiver, (_, inbox)) in nodes.iter_mut().enumerate() {
        let mut got = Vec::new();
        for _ in 0..4 {
            got.push(recv_text(inbox).await);
        }
        for (sender, spec) in specs.iter().enumerate().filter(|(s, _)| *s != receiver) {
            let texts: Vec<_> = got
                .iter()
                .filter(|(from, _)| *from == spec.id())
                .map(|(_, text)| text.as_str())
                .collect();
            let expected = [format!("from {sender}, #0"), format!("from {sender}, #1")];
            assert_eq!(texts, expected, "node {receiver} from node {sender}");
        }
        assert!(
            inbox.try_recv().is_err(),
            "no duplicates at node {receiver}"
        );
    }

    for (node, _) in nodes {
        node.shutdown().await;
    }
}

#[tokio::test]
async fn nodes_dialing_each_other_at_once_keep_one_connection() {
    let (a, b) = (Spec::new(), Spec::new());
    let ((node_a, mut inbox_a), (node_b, mut inbox_b)) =
        tokio::join!(a.spawn(vec![b.peer()]), b.spawn(vec![a.peer()]));

    wait_until("both have one connection", || {
        node_a.connected_once_to(&[&b]) && node_b.connected_once_to(&[&a])
    })
    .await;

    for n in 0..2 {
        node_a
            .broadcast(text_clip(a.id(), n, &format!("a{n}")))
            .expect("send");
        node_b
            .broadcast(text_clip(b.id(), n, &format!("b{n}")))
            .expect("send");
    }
    assert_eq!(recv_text(&mut inbox_b).await, (a.id(), "a0".to_owned()));
    assert_eq!(recv_text(&mut inbox_b).await, (a.id(), "a1".to_owned()));
    assert_eq!(recv_text(&mut inbox_a).await, (b.id(), "b0".to_owned()));
    assert_eq!(recv_text(&mut inbox_a).await, (b.id(), "b1".to_owned()));

    node_a.shutdown().await;
    node_b.shutdown().await;
}

#[tokio::test]
async fn smaller_id_replaces_an_older_connection_with_the_newer_one() {
    let (small, big) = Spec::ordered_pair();
    let (node, mut inbox) = small.spawn(vec![big.peer_without_addrs()]).await;
    let raw = raw_endpoint(&big).await;

    let first = raw_connect(&raw, &small).await;
    let _first_streams = raw_hello(&first).await;
    let second = raw_connect(&raw, &small).await;
    let (mut send, mut recv) = raw_hello(&second).await;

    assert_eq!(close_code(&first).await, Some(close::REPLACED));
    // The newer connection works both ways.
    node.broadcast(text_clip(small.id(), 1, "to big"))
        .expect("send");
    let expected = text_clip(small.id(), 1, "to big");
    assert_eq!(read(&mut recv).await, Message::Clip(expected));
    write(
        &mut send,
        &Message::Clip(text_clip(big.id(), 1, "to small")),
    )
    .await;
    assert_eq!(
        recv_text(&mut inbox).await,
        (big.id(), "to small".to_owned())
    );

    node.shutdown().await;
}

#[tokio::test]
async fn bigger_id_leaves_duplicate_connections_to_the_peer() {
    let (small, big) = Spec::ordered_pair();
    let (node, _inbox) = big.spawn(vec![small.peer_without_addrs()]).await;
    let raw = raw_endpoint(&small).await;

    let first = raw_connect(&raw, &big).await;
    let _first_streams = raw_hello(&first).await;
    let second = raw_connect(&raw, &big).await;
    let _second_streams = raw_hello(&second).await;

    wait_until("both sessions are live", || {
        node.live_sessions_with(small.id()) == 2
    })
    .await;
    assert!(first.close_reason().is_none(), "the smaller id decides");
    node.shutdown().await;
}

#[tokio::test]
async fn unpaired_node_is_rejected() {
    let paired = Spec::new();
    let node_spec = Spec::new();
    let (node, _inbox) = node_spec.spawn(vec![paired.peer_without_addrs()]).await;
    let stranger = raw_endpoint(&Spec::new()).await;

    let addr = EndpointAddr::new(node_spec.id()).with_ip_addr(node_spec.addr());
    let code = match time::timeout(TIMEOUT, stranger.connect(addr, ALPN))
        .await
        .expect("answer in time")
    {
        Ok(conn) => close_code(&conn).await,
        // The node may close it before our side has finished the handshake.
        Err(ConnectError::Connecting {
            source:
                ConnectingError::ConnectionError {
                    source: ConnectionError::ApplicationClosed(close),
                    ..
                },
            ..
        }) => Some(close.error_code),
        Err(_) => None,
    };
    assert_eq!(code, Some(close::NOT_PAIRED));
    node.shutdown().await;
}

#[tokio::test]
async fn other_protocol_version_is_refused() {
    let peer = Spec::new();
    let node_spec = Spec::new();
    let (node, _inbox) = node_spec.spawn(vec![peer.peer_without_addrs()]).await;
    let raw = raw_endpoint(&peer).await;

    let conn = raw_connect(&raw, &node_spec).await;
    let mut send = conn.open_uni().await.expect("open stream");
    let future = Hello {
        protocol_version: protocol::PROTOCOL_VERSION + 1,
    };
    write(&mut send, &Message::Hello(future)).await;

    assert_eq!(close_code(&conn).await, Some(close::PROTOCOL_ERROR));
    node.shutdown().await;
}

#[tokio::test]
async fn clip_relayed_for_another_node_is_dropped() {
    let peer = Spec::new();
    let node_spec = Spec::new();
    let (node, mut inbox) = node_spec.spawn(vec![peer.peer_without_addrs()]).await;
    let raw = raw_endpoint(&peer).await;
    let conn = raw_connect(&raw, &node_spec).await;
    let (mut send, _recv) = raw_hello(&conn).await;

    let someone_else = Spec::new().id();
    write(
        &mut send,
        &Message::Clip(text_clip(someone_else, 1, "relayed")),
    )
    .await;
    write(&mut send, &Message::Clip(text_clip(peer.id(), 2, "own"))).await;

    // Frames on a stream arrive in order, so had the relayed clip passed, it would come first.
    assert_eq!(recv_text(&mut inbox).await, (peer.id(), "own".to_owned()));
    node.shutdown().await;
}

#[tokio::test]
async fn clips_flow_again_after_a_node_restarts() {
    let (a, b) = (Spec::new(), Spec::new());
    let (node_a, mut inbox_a) = a.spawn(vec![b.peer()]).await;
    let (node_b, _) = b.spawn(vec![a.peer()]).await;
    wait_until("connected", || node_a.connected_once_to(&[&b])).await;

    node_b.shutdown().await;
    wait_until("a notices", || node_a.live_sessions_with(b.id()) == 0).await;

    // Same identity and port, as after a reboot.
    let (node_b, mut inbox_b) = b.spawn(vec![a.peer()]).await;
    wait_until("reconnected", || {
        node_a.connected_once_to(&[&b]) && node_b.connected_once_to(&[&a])
    })
    .await;

    node_a
        .broadcast(text_clip(a.id(), 1, "after"))
        .expect("send");
    node_b
        .broadcast(text_clip(b.id(), 1, "back"))
        .expect("send");
    assert_eq!(recv_text(&mut inbox_b).await, (a.id(), "after".to_owned()));
    assert_eq!(recv_text(&mut inbox_a).await, (b.id(), "back".to_owned()));

    node_a.shutdown().await;
    node_b.shutdown().await;
}

#[tokio::test]
async fn node_keeps_dialing_until_the_peer_shows_up() {
    let (a, b) = (Spec::new(), Spec::new());
    // `b` cannot dial `a`, so only `a`'s retries can connect them.
    let (node_a, _inbox_a) = a.spawn(vec![b.peer()]).await;
    time::sleep(Duration::from_millis(300)).await;
    let (node_b, mut inbox_b) = b.spawn(vec![a.peer_without_addrs()]).await;

    wait_until("connected", || node_a.connected_once_to(&[&b])).await;
    node_a
        .broadcast(text_clip(a.id(), 1, "late"))
        .expect("send");
    assert_eq!(recv_text(&mut inbox_b).await, (a.id(), "late".to_owned()));

    node_a.shutdown().await;
    node_b.shutdown().await;
}

#[tokio::test]
async fn links_follow_the_connection_to_each_peer() {
    let (a, b) = (Spec::new(), Spec::new());
    let (node_a, _inbox_a) = a.spawn(vec![b.peer()]).await;
    assert!(!node_a.lan_discovery());
    let mut links = node_a.links();
    assert_eq!(links.borrow()[&b.id()], Link::Disconnected);

    let (node_b, _inbox_b) = b.spawn(vec![a.peer()]).await;
    let b_id = b.id();
    let link_to_b = |want_connected: bool| {
        move |links: &Links| matches!(links[&b_id], Link::Connected { .. }) == want_connected
    };
    let connected = time::timeout(TIMEOUT, links.wait_for(link_to_b(true)))
        .await
        .expect("connects in time")
        .expect("node still running")[&b.id()];
    // Both nodes send from their listening port, whoever dialed.
    assert_eq!(
        connected,
        Link::Connected {
            addr: Some(b.addr())
        }
    );

    node_b.shutdown().await;
    time::timeout(TIMEOUT, links.wait_for(link_to_b(false)))
        .await
        .expect("notices in time")
        .expect("node still running");
    node_a.shutdown().await;
}
