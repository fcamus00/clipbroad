//! The control channel without a daemon behind it. `run`'s tests pair two daemons through it.

use super::*;

fn paths(dir: &tempfile::TempDir) -> Paths {
    Paths::beside_config(dir.path().join("config.toml"))
}

#[tokio::test]
async fn nobody_listens_without_a_daemon() {
    let dir = tempfile::tempdir().expect("temp dir");
    assert!(connect(&paths(&dir)).await.expect("no error").is_none());
}

#[tokio::test]
async fn messages_cross_and_the_channel_closes_with_the_listener() {
    let dir = tempfile::tempdir().expect("temp dir");
    let paths = paths(&dir);
    let mut listener = Listener::bind(&paths).expect("binds");

    let (client, server) = tokio::join!(connect(&paths), listener.accept());
    let mut client = client.expect("no error").expect("daemon listens");
    let mut server = server.expect("accepts");
    write(&mut client, &Request::Join("TICKET".to_owned()))
        .await
        .expect("sent");
    let request: Request = read(&mut server).await.expect("received");
    assert!(matches!(request, Request::Join(ticket) if ticket == "TICKET"));
    write(&mut server, &Reply::Failed("nope".to_owned()))
        .await
        .expect("sent");
    let reply: Reply = read(&mut client).await.expect("received");
    assert!(matches!(reply, Reply::Failed(error) if error == "nope"));

    // Only one daemon per config.
    assert!(Listener::bind(&paths).is_err());
    drop((client, server, listener));
    assert!(connect(&paths).await.expect("no error").is_none());
}

#[tokio::test]
async fn oversized_message_is_refused_before_reading_it() {
    let (mut a, mut b) = tokio::io::duplex(64);
    a.write_u32(MAX_MESSAGE_LEN + 1).await.expect("sent");
    let error = read::<Request>(&mut b).await.expect_err("too large");
    assert!(error.to_string().contains("too large"), "{error:#}");
}

/// A daemon that was killed leaves its socket file behind.
#[cfg(unix)]
#[tokio::test]
async fn socket_left_by_a_dead_daemon_is_replaced() {
    let dir = tempfile::tempdir().expect("temp dir");
    let paths = paths(&dir);
    drop(std::os::unix::net::UnixListener::bind(socket_path(&paths)).expect("binds"));
    assert!(socket_path(&paths).exists());

    assert!(connect(&paths).await.expect("no error").is_none());
    let _listener = Listener::bind(&paths).expect("replaces it");
    assert!(connect(&paths).await.expect("no error").is_some());
}
