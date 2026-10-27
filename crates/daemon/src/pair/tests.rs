//! Pairs two machines with their own config directories, then shows that their nodes accept each
//! other and exchange clips.

use std::fs;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};

use clipbroad_core::ClipContent;
use clipbroad_core::protocol::{Clip, ClipId, NodeId};
use clipbroad_net::{Node, Received};
use tokio::sync::{mpsc, oneshot};
use tokio::time;

use super::*;

/// Upper bound for anything a test waits on, so a broken test does not hang CI.
const TIMEOUT: Duration = Duration::from_secs(10);

/// A machine: its own directory with a config file that sets a free port.
struct Machine {
    _dir: tempfile::TempDir,
    paths: Paths,
    port: u16,
}

impl Machine {
    fn new() -> Self {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("bind a free port");
        let port = socket.local_addr().expect("local address").port();
        let dir = tempfile::tempdir().expect("temp dir");
        let paths = Paths::beside_config(dir.path().join("config.toml"));
        fs::write(
            &paths.config_file,
            format!("# my settings\nlisten_port = {port}\n"),
        )
        .expect("write config");
        Self {
            _dir: dir,
            paths,
            port,
        }
    }

    fn addr(&self) -> SocketAddr {
        (Ipv4Addr::LOCALHOST, self.port).into()
    }

    fn config(&self) -> Config {
        Config::load_or_default(&self.paths.config_file).expect("valid config")
    }

    async fn spawn_node(&self) -> (Node, mpsc::Receiver<Received>) {
        let identity = IdentityKey::load(&self.paths.secret_key_file).expect("identity exists");
        Node::spawn(&identity, &self.config())
            .await
            .expect("node starts")
    }
}

/// Sends `text` from `from` and waits until `to` receives it.
async fn exchange(from: &Node, to: &mut mpsc::Receiver<Received>, text: &str) {
    let origin = NodeId::from_bytes(*from.id().as_bytes());
    let clip = Clip::new(
        ClipId(1),
        origin,
        1_700_000_000_000,
        ClipContent::from(text),
    );
    // Broadcast until the connection is up: clips sent before that are not queued.
    let received = time::timeout(TIMEOUT, async {
        loop {
            from.broadcast(clip.clone()).expect("small clip");
            if let Ok(Some(received)) = time::timeout(Duration::from_millis(100), to.recv()).await {
                return received;
            }
        }
    })
    .await
    .expect("the clip arrives in time");
    assert_eq!(received.from, from.id());
    let content = received.clip.into_content().expect("intact clip");
    assert_eq!(content.as_text(), Some(text));
}

#[tokio::test]
async fn paired_machines_accept_each_other() {
    let (a, b) = (Machine::new(), Machine::new());
    let (ticket_tx, ticket_rx) = oneshot::channel();
    let a_addr = a.addr();
    let a_paths = a.paths.clone();
    let host = tokio::spawn(async move {
        host(&a_paths, TIMEOUT, |ticket| {
            // A only advertises its network addresses; the test stays on localhost.
            let mut local = ticket.clone();
            local.addrs = vec![a_addr];
            ticket_tx.send(local).expect("test is waiting");
        })
        .await
    });

    let ticket = ticket_rx.await.expect("host shows a ticket");
    let (a_as_peer, added) = join(&b.paths, &ticket).await.expect("paired");
    assert!(added);
    let (b_as_peer, added) = host.await.expect("no panic").expect("paired");
    assert!(added);

    // Each side saved the other with its id, hostname and listening address.
    assert_eq!(a_as_peer.id, ticket.id);
    assert_eq!(a_as_peer.name, hostname());
    assert_eq!(a_as_peer.addrs, [a.addr()]);
    assert_eq!(b_as_peer.addrs.first(), Some(&b.addr()));
    assert_ne!(a_as_peer.id, b_as_peer.id);
    assert_eq!(b.config().peers, [a_as_peer]);
    assert_eq!(a.config().peers, [b_as_peer]);
    let text = fs::read_to_string(&a.paths.config_file).expect("read");
    assert!(text.starts_with("# my settings\n"), "{text}");

    // The normal allowlist now lets them talk, in both directions.
    let (node_a, mut inbox_a) = a.spawn_node().await;
    let (node_b, mut inbox_b) = b.spawn_node().await;
    exchange(&node_a, &mut inbox_b, "from a").await;
    exchange(&node_b, &mut inbox_a, "from b").await;
    node_a.shutdown().await;
    node_b.shutdown().await;
}
