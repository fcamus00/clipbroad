//! The control channel: how `clipbroad pair` asks the running daemon to pair, how `clipbroad
//! service` on Windows asks it to quit, and how `clipbroad update` asks it to restart.
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
//! [`Request`]; the daemon answers with [`Reply`]s until a [`Reply::Paired`], a [`Reply::Failed`]
//! or a [`Reply::Quitting`]. While it hosts, the daemon watches the stream: if the client quits,
//! the daemon stops waiting for the other machine.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use clipbroad_config::{Paths, Peer};
use clipbroad_net::Node;
use clipbroad_net::pair::{Host, Ticket};
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
    /// Shut down, as the tray menu's "Quit" does. A new variant goes last: a daemon from before
    /// it closes the channel, which tells the client to stop it some other way.
    Quit,
    /// Shut down and run the binary again, to pick up an update.
    Restart,
}

/// What the daemon answers.
#[derive(Debug, Serialize, Deserialize)]
pub enum Reply {
    /// The ticket to show, while the daemon waits. Only for [`Request::Host`].
    Ticket(String),
    Paired(Paired),
    /// Why pairing failed, ready to show.
    Failed(String),
    /// The daemon got [`Request::Quit`] or [`Request::Restart`] and is shutting down.
    Quitting,
}

/// How serving a client ended.
#[derive(Debug)]
pub enum Served {
    /// The client asked for nothing.
    Nothing,
    /// A pairing ended, with the peer and whether it was new.
    Paired(Result<(Peer, bool)>),
    /// The daemon should shut down.
    Quit,
    /// The daemon should shut down and run the binary again.
    Restart,
}

/// How the daemon answered a [`request`](ask).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asked {
    NoDaemon,
    Agreed,
    /// It cannot do it yet: it is from before that request, or busy pairing.
    Refused,
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

/// Serves one client: pairs as it asks, saving the peer to the config and adding it to `node`, or
/// tells the daemon to quit.
pub async fn serve(stream: impl Stream, node: &Node, paths: &Paths) -> Served {
    let (mut reader, mut writer) = tokio::io::split(stream);
    let request = match time::timeout(REQUEST_TIMEOUT, read(&mut reader)).await {
        Ok(Ok(request)) => request,
        // Such as a second daemon checking whether this one is alive.
        Ok(Err(error)) => {
            debug!("control client sent no request: {error:#}");
            return Served::Nothing;
        }
        Err(_) => {
            debug!("control client sent no request in time");
            return Served::Nothing;
        }
    };
    let outcome = match request {
        Request::Quit => {
            let _ = write(&mut writer, &Reply::Quitting).await;
            return Served::Quit;
        }
        Request::Restart => {
            let _ = write(&mut writer, &Reply::Quitting).await;
            return Served::Restart;
        }
        Request::Host => {
            async {
                let host = pair::host_on(node)?;
                write(&mut writer, &Reply::Ticket(host.ticket().to_string())).await?;
                finish_hosting(host, &mut reader, node, paths).await
            }
            .await
        }
        Request::Join(ticket) => {
            async {
                let ticket: Ticket = ticket.parse()?;
                pair::join_on(node, paths, &ticket).await
            }
            .await
        }
    };
    let reply = match &outcome {
        Ok((peer, added)) => Reply::Paired(Paired::new((peer.clone(), *added))),
        Err(error) => Reply::Failed(format!("{error:#}")),
    };
    // The client may be gone already.
    let _ = write(&mut writer, &reply).await;
    Served::Paired(outcome)
}

/// Waits for the other machine, or until the client quits.
async fn finish_hosting(
    host: Host,
    client: &mut (impl AsyncRead + Unpin),
    node: &Node,
    paths: &Paths,
) -> Result<(Peer, bool)> {
    info!("waiting for the other machine to use the pairing ticket");
    tokio::select! {
        paired = pair::finish_host(host, paths, TICKET_TTL, Some(node)) => paired,
        // The client sends nothing more, so this only ends when it quits.
        _ = client.read_u8() => anyhow::bail!("`clipbroad pair` quit"),
    }
}

/// Answers a client while another one is pairing.
pub async fn refuse(mut stream: impl Stream) {
    let reply = Reply::Failed("another `clipbroad pair` is already running".to_owned());
    let _ = write(&mut stream, &reply).await;
}

/// Asks the daemon that uses `paths` to shut down. Returns whether it agreed.
#[cfg(any(windows, test))]
pub async fn quit(paths: &Paths) -> bool {
    ask(paths, &Request::Quit).await == Asked::Agreed
}

/// Asks the daemon that uses `paths` to run the binary again.
pub async fn restart(paths: &Paths) -> Asked {
    ask(paths, &Request::Restart).await
}

async fn ask(paths: &Paths, request: &Request) -> Asked {
    let Ok(Some(mut daemon)) = connect(paths).await else {
        return Asked::NoDaemon;
    };
    if write(&mut daemon, request).await.is_err() {
        return Asked::Refused;
    }
    let reply = time::timeout(REQUEST_TIMEOUT, read(&mut daemon)).await;
    if matches!(reply, Ok(Ok(Reply::Quitting))) {
        Asked::Agreed
    } else {
        Asked::Refused
    }
}

#[cfg(test)]
mod tests;
