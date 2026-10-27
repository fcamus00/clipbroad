//! Pairing over localhost. Tickets are pointed at 127.0.0.1, since a host only advertises its
//! network addresses.

use std::{
    net::{Ipv4Addr, UdpSocket},
    time::Instant,
};

use super::*;

const TTL: Duration = Duration::from_secs(10);

struct Machine {
    key: IdentityKey,
    port: NonZeroU16,
    name: &'static str,
}

impl Machine {
    fn new(name: &'static str) -> Self {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("bind a free port");
        let port = socket.local_addr().expect("local address").port();
        Self {
            key: IdentityKey::generate().expect("random key"),
            port: NonZeroU16::new(port).expect("the OS never assigns port 0"),
            name,
        }
    }

    fn id(&self) -> EndpointId {
        SecretKey::from_bytes(&self.key.to_bytes()).public()
    }

    fn addr(&self) -> SocketAddr {
        (Ipv4Addr::LOCALHOST, self.port.get()).into()
    }

    /// Starts a host and returns it with its ticket, pointed at localhost.
    async fn host(&self) -> (Host, Ticket) {
        let host = Host::bind(&self.key, self.port, Some(self.name.to_owned()))
            .await
            .expect("host starts");
        let ticket = Ticket {
            addrs: vec![self.addr()],
            ..host.ticket().clone()
        };
        (host, ticket)
    }

    async fn join(&self, ticket: &Ticket) -> Result<PairedPeer, PairError> {
        join(&self.key, self.port, ticket, Some(self.name.to_owned())).await
    }

    /// Checks what the other side learned about this machine. Its address list starts with the
    /// localhost address it was seen at, followed by its network addresses.
    fn assert_learned(&self, peer: &PairedPeer) {
        assert_eq!(peer.id, self.id());
        assert_eq!(peer.name.as_deref(), Some(self.name));
        assert_eq!(peer.addrs.first(), Some(&self.addr()), "{peer:?}");
        assert!(
            peer.addrs.iter().all(|addr| addr.port() == self.port.get()),
            "{peer:?}"
        );
    }
}

/// Runs the host side in a task: accept one request and confirm it.
fn serve(host: Host, ttl: Duration) -> tokio::task::JoinHandle<Result<PairedPeer, PairError>> {
    tokio::spawn(async move { host.accept(ttl).await?.confirm().await })
}

#[test]
fn ticket_round_trips_as_one_line() {
    let key = SecretKey::from_bytes(&[3; 32]);
    let ticket = Ticket::new(
        key.public(),
        vec![
            "192.168.1.20:42420".parse().expect("valid"),
            "[fd7a:115c:a1e0::1]:42420".parse().expect("valid"),
        ],
    )
    .expect("random secret");

    let text = ticket.to_string();
    assert!(text.chars().all(|c| c.is_ascii_alphanumeric()), "{text}");
    assert_eq!(text.parse::<Ticket>().expect("valid"), ticket);
    // Case and surrounding spaces from copy-paste do not matter.
    let pasted = format!("  {}\n", text.to_lowercase());
    assert_eq!(pasted.parse::<Ticket>().expect("valid"), ticket);

    let debug = format!("{ticket:?}");
    assert!(debug.contains("redacted"), "{debug}");
    assert!(!debug.contains(&format!("{:?}", ticket.secret)), "{debug}");
}

#[test]
fn bad_tickets_are_rejected() {
    let ticket =
        Ticket::new(SecretKey::from_bytes(&[3; 32]).public(), Vec::new()).expect("random secret");
    let text = ticket.to_string();

    let cut = &text[..text.len() / 2];
    assert!(matches!(
        cut.parse::<Ticket>(),
        Err(TicketError::Invalid | TicketError::Encoding(_))
    ));
    assert!(matches!(
        "hello world!".parse::<Ticket>(),
        Err(TicketError::Encoding(_))
    ));
    assert!(matches!("".parse::<Ticket>(), Err(TicketError::Invalid)));

    let future = BASE32_NOPAD_NOCASE.encode(&[2, 0, 0]);
    let err = future.parse::<Ticket>().expect_err("unknown version");
    assert!(matches!(err, TicketError::Version(2)), "{err}");
}

#[tokio::test]
async fn both_sides_learn_each_other() {
    let (a, b) = (Machine::new("a"), Machine::new("b"));
    let (host, ticket) = a.host().await;
    assert_eq!(ticket.id, a.id());
    let host = serve(host, TTL);

    a.assert_learned(&b.join(&ticket).await.expect("paired"));
    b.assert_learned(&host.await.expect("no panic").expect("paired"));
}

#[tokio::test]
async fn wrong_secret_is_rejected_without_ending_the_session() {
    let (a, b, intruder) = (
        Machine::new("a"),
        Machine::new("b"),
        Machine::new("intruder"),
    );
    let (host, ticket) = a.host().await;
    let host = serve(host, TTL);

    let mut guess = ticket.clone();
    guess.secret[0] ^= 1;
    let err = intruder.join(&guess).await.expect_err("wrong secret");
    assert!(matches!(err, PairError::WrongSecret), "{err}");

    a.assert_learned(&b.join(&ticket).await.expect("paired"));
    b.assert_learned(&host.await.expect("no panic").expect("paired"));
}

#[tokio::test]
async fn ticket_works_only_once() {
    let (a, b, c) = (Machine::new("a"), Machine::new("b"), Machine::new("c"));
    let (host, ticket) = a.host().await;
    let host = serve(host, TTL);
    b.join(&ticket).await.expect("paired");
    host.await.expect("no panic").expect("paired");

    let err = c.join(&ticket).await.expect_err("ticket already used");
    assert!(matches!(err, PairError::Unreachable), "{err}");
}

#[tokio::test]
async fn expired_ticket_is_rejected() {
    let (a, b) = (Machine::new("a"), Machine::new("b"));
    let (host, ticket) = a.host().await;
    let started = Instant::now();
    let err = host
        .accept(Duration::from_millis(200))
        .await
        .expect_err("nobody joined");
    assert!(matches!(err, PairError::Expired), "{err}");
    assert!(started.elapsed() < TTL);

    let err = b.join(&ticket).await.expect_err("expired");
    assert!(matches!(err, PairError::Unreachable), "{err}");
}

#[tokio::test]
async fn own_ticket_is_rejected() {
    let a = Machine::new("a");
    let (_host, ticket) = a.host().await;
    let err = a.join(&ticket).await.expect_err("same machine");
    assert!(matches!(err, PairError::OwnTicket), "{err}");
}
