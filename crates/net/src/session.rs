//! One connection with a peer: the `Hello` exchange, then clips in both directions.

use std::{sync::Arc, time::Duration};

use clipbroad_core::protocol::{self, FrameError, Hello, Message};
use iroh::{
    EndpointId,
    endpoint::{Connection, ConnectionError, ReadError, RecvStream, SendStream, WriteError},
};
use tokio::{
    sync::{broadcast, mpsc},
    time,
};
use tracing::{debug, warn};

use crate::{Frame, Received, close, peer::PeerState};

/// Most bytes taken from the stream per read.
const READ_CHUNK: usize = 64 * 1024;
/// Longest the `Hello` exchange may take. Without a limit, a connection that never says `Hello`
/// would stay pending forever and stop the peer task from dialing again.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
enum SessionError {
    #[error(transparent)]
    Connection(#[from] ConnectionError),
    #[error(transparent)]
    Write(#[from] WriteError),
    #[error(transparent)]
    Read(#[from] ReadError),
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("{0}")]
    Protocol(&'static str),
    #[error("no Hello within {HELLO_TIMEOUT:?}")]
    HelloTimeout,
    /// The node is stopping.
    #[error("node stopped")]
    Stopped,
}

/// Runs a session until the connection ends, and closes it.
///
/// Returns whether the peer completed the `Hello` exchange, so the caller can tell a peer that
/// went away from one it cannot talk to.
pub(crate) async fn run(
    conn: Connection,
    mut outbox: broadcast::Receiver<Frame>,
    inbox: mpsc::Sender<Received>,
    peer: Arc<PeerState>,
) -> bool {
    let handshake = time::timeout(HELLO_TIMEOUT, handshake(&conn)).await;
    let (mut send, mut reader) = match handshake.unwrap_or(Err(SessionError::HelloTimeout)) {
        Ok(streams) => streams,
        Err(e) => {
            close_after(&conn, &e);
            return false;
        }
    };
    let _live = peer.greeted(&conn);
    let result = tokio::select! {
        result = send_clips(&mut send, &mut outbox) => result,
        result = receive_clips(&mut reader, conn.remote_id(), &inbox) => result,
    };
    // Both loops only stop on an error; a peer that finishes its stream is the exception.
    match result {
        Ok(()) => conn.close(close::NORMAL, b""),
        Err(e) => close_after(&conn, &e),
    }
    true
}

/// Sends our `Hello` and waits for the peer's. Returns our stream and the reader of theirs.
async fn handshake(conn: &Connection) -> Result<(SendStream, FrameReader), SessionError> {
    let mut send = conn.open_uni().await?;
    let hello = protocol::encode(&Message::Hello(Hello::CURRENT))?;
    send.write_all(&hello).await?;

    let mut reader = FrameReader::new(conn.accept_uni().await?);
    match reader.next().await? {
        Some(Message::Hello(_)) => Ok((send, reader)),
        Some(Message::Clip(_)) => Err(SessionError::Protocol("clip before Hello")),
        None => Err(SessionError::Protocol("stream ended before Hello")),
    }
}

/// Closes the connection after `error`, telling the peer if it broke the protocol.
fn close_after(conn: &Connection, error: &SessionError) {
    let peer = conn.remote_id().fmt_short();
    if matches!(error, SessionError::Frame(_) | SessionError::Protocol(_)) {
        warn!(%peer, %error, "closing connection: peer broke the protocol");
        conn.close(close::PROTOCOL_ERROR, error.to_string().as_bytes());
    } else {
        debug!(%peer, %error, "connection ended");
        conn.close(close::NORMAL, b"");
    }
}

async fn send_clips(
    send: &mut SendStream,
    outbox: &mut broadcast::Receiver<Frame>,
) -> Result<(), SessionError> {
    loop {
        match outbox.recv().await {
            Ok(frame) => send.write_all(&frame).await?,
            Err(broadcast::error::RecvError::Lagged(missed)) => {
                warn!(missed, "connection too slow, skipped clips");
            }
            Err(broadcast::error::RecvError::Closed) => return Err(SessionError::Stopped),
        }
    }
}

async fn receive_clips(
    reader: &mut FrameReader,
    peer: EndpointId,
    inbox: &mpsc::Sender<Received>,
) -> Result<(), SessionError> {
    while let Some(message) = reader.next().await? {
        let clip = match message {
            Message::Clip(clip) => clip,
            Message::Hello(_) => return Err(SessionError::Protocol("second Hello")),
        };
        if clip.origin.as_bytes() != peer.as_bytes() {
            warn!(
                peer = %peer.fmt_short(),
                origin = %clip.origin,
                "dropped a clip relayed for another node"
            );
            continue;
        }
        inbox
            .send(Received { from: peer, clip })
            .await
            .map_err(|_| SessionError::Stopped)?;
    }
    Ok(())
}

/// Reads whole frames from a stream.
struct FrameReader {
    stream: RecvStream,
    buf: Vec<u8>,
}

impl FrameReader {
    fn new(stream: RecvStream) -> Self {
        Self {
            stream,
            buf: Vec::new(),
        }
    }

    /// The next message, or `None` when the peer finished the stream.
    async fn next(&mut self) -> Result<Option<Message>, SessionError> {
        loop {
            if let Some((message, used)) = protocol::decode(&self.buf)? {
                self.buf.drain(..used);
                return Ok(Some(message));
            }
            match self.stream.read_chunk(READ_CHUNK).await? {
                Some(chunk) => self.buf.extend_from_slice(&chunk),
                None if self.buf.is_empty() => return Ok(None),
                None => return Err(SessionError::Protocol("stream ended inside a frame")),
            }
        }
    }
}
