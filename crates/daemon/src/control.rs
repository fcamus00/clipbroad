//! The control channel: how `clipbroad pair` asks the running daemon to pair.
//!
//! The daemon listens on a local socket: a Unix socket `control.sock` beside the identity file
//! on Linux and macOS, and a named pipe on Windows whose name comes from the same path. Either
//! way it follows the identity, so two instances with their own `--config` never share one, and
//! only this user can connect: the directory is private on Unix, and the pipe refuses remote
//! clients and other users' writes on Windows. A socket and not a file like `status.toml`:
//! pairing is a conversation (ask for a ticket, show it, wait for the result) that a file would
//! need polling and locking for.
//!
//! Each message is a big-endian `u32` length and the message in `postcard`. The client sends one
//! [`Request`]; the daemon answers with [`Reply`]s until a [`Reply::Paired`] or a
//! [`Reply::Failed`]. While it hosts, the daemon watches the stream: if the client quits, the
//! daemon stops waiting for the other machine.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use clipbroad_config::{Paths, Peer};
use clipbroad_net::Node;
use clipbroad_net::pair::Ticket;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::time;
use tracing::{debug, info, warn};

use crate::pair::{self, Paired, TICKET_TTL};

#[cfg_attr(unix, path = "control/unix.rs")]
#[cfg_attr(windows, path = "control/windows.rs")]
mod platform;

pub use platform::{Listener, ServerStream, connect};

/// Control messages are a ticket or a short error; anything bigger is not one.
const MAX_MESSAGE_LEN: u32 = 64 * 1024;
/// How long the daemon waits for a client's request, so that one which sends nothing does not
/// keep others out.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Either end of a control connection.
pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Stream for S {}

/// What `clipbroad pair` asks the daemon.
#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    /// Show a ticket and wait for the other machine.
    Host,
    /// Pair with the machine that showed this ticket.
    Join(String),
}

/// What the daemon answers.
#[derive(Debug, Serialize, Deserialize)]
pub enum Reply {
    /// The ticket to show, while the daemon waits. Only for [`Request::Host`].
    Ticket(String),
    Paired(Paired),
    /// Why pairing failed, ready to show.
    Failed(String),
}

/// Where the daemon listens: `control.sock` beside the identity file. On Windows, the pipe name
/// comes from this path.
fn socket_path(paths: &Paths) -> PathBuf {
    paths.secret_key_file.with_file_name("control.sock")
}

pub async fn write<T: Serialize>(
    stream: &mut (impl AsyncWrite + Unpin),
    message: &T,
) -> Result<()> {
    let bytes = postcard::to_stdvec(message)?;
    let len = u32::try_from(bytes.len())
        .ok()
        .filter(|&len| len <= MAX_MESSAGE_LEN)
        .context("control message too large")?;
    stream.write_u32(len).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

pub async fn read<T: DeserializeOwned>(stream: &mut (impl AsyncRead + Unpin)) -> Result<T> {
    let len = stream
        .read_u32()
        .await
        .context("the other side closed the control channel")?;
    ensure!(len <= MAX_MESSAGE_LEN, "control message too large");
    let mut bytes = vec![0; len as usize];
    stream.read_exact(&mut bytes).await?;
    // Both sides are the same binary, unless the daemon was started before an upgrade.
    postcard::from_bytes(&bytes)
        .context("invalid control message; if clipbroad was just updated, restart `clipbroad run`")
}

/// Opens the daemon's end, or returns `None`, after a warning, if it cannot: the daemon syncs
/// anyway, and `clipbroad pair` then asks to stop it.
pub fn listen(paths: &Paths) -> Option<Listener> {
    Listener::bind(paths)
        .inspect_err(|error| {
            warn!(
                %error,
                "cannot open the control channel; `clipbroad pair` will ask to stop this daemon"
            );
        })
        .ok()
}

/// Serves one client: pairs as it asks, saving the peer to the config and adding it to `node`.
/// Returns the peer if it was new, so the daemon can list it in its status.
pub async fn serve(stream: impl Stream, node: &Node, paths: &Paths) -> Option<Peer> {
    let (mut reader, mut writer) = tokio::io::split(stream);
    let request = match time::timeout(REQUEST_TIMEOUT, read(&mut reader)).await {
        Ok(Ok(request)) => request,
        // Such as a second daemon checking whether this one is alive.
        Ok(Err(error)) => {
            debug!("control client sent no request: {error:#}");
            return None;
        }
        Err(_) => {
            debug!("control client sent no request in time");
            return None;
        }
    };
    let outcome = async {
        match request {
            Request::Host => {
                let host = pair::host_on(node)?;
                write(&mut writer, &Reply::Ticket(host.ticket().to_string())).await?;
                info!("waiting for the other machine to use the pairing ticket");
                tokio::select! {
                    paired = pair::finish_host(host, paths, TICKET_TTL, Some(node)) => paired,
                    // The client sends nothing more, so this only ends when it quits.
                    _ = reader.read_u8() => anyhow::bail!("`clipbroad pair` quit"),
                }
            }
            Request::Join(ticket) => {
                let ticket: Ticket = ticket.parse()?;
                pair::join_on(node, paths, &ticket).await
            }
        }
    }
    .await;
    let reply = match &outcome {
        Ok((peer, added)) => {
            let name = peer.name.as_deref().unwrap_or("(no name)");
            info!(peer = %peer.id.fmt_short(), name, added, "paired");
            Reply::Paired(Paired::new((peer.clone(), *added)))
        }
        Err(error) => {
            warn!("pairing failed: {error:#}");
            Reply::Failed(format!("{error:#}"))
        }
    };
    // The client may be gone already.
    let _ = write(&mut writer, &reply).await;
    outcome.ok().and_then(|(peer, added)| added.then_some(peer))
}

/// Answers a client while another one is pairing.
pub async fn refuse(mut stream: impl Stream) {
    let reply = Reply::Failed("another `clipbroad pair` is already running".to_owned());
    let _ = write(&mut stream, &reply).await;
}

#[cfg(test)]
mod tests;
