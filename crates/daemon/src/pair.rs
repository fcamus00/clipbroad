//! `clipbroad pair`: adds a peer to the config by passing a ticket from one machine to the other.
//!
//! If `clipbroad run` is running with the same config, `pair` asks it to pair over the control
//! channel (see [`control`]): the daemon pairs on its own endpoint and syncs with the new peer
//! right away. Otherwise `pair` pairs on its own, on the same port the daemon would use.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clipbroad_config::{Config, IdentityKey, Paths, Peer};
use clipbroad_net::Node;
use clipbroad_net::pair::{self, Host, PairError, PairedPeer, Ticket};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};

use crate::control::{self, Reply, Request};

#[derive(Debug, clap::Args)]
pub struct Args {
    /// The ticket shown by `clipbroad pair` on the other machine. Without it, this machine shows
    /// a ticket and waits.
    ticket: Option<Ticket>,

    /// Config file to use instead of the default one. The identity is then read from (or created
    /// as) `secret.key` in the same directory, so give each instance its own directory.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
}

/// How long a ticket works.
pub const TICKET_TTL: Duration = Duration::from_mins(5);

/// The machine this one paired with, as shown to the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paired {
    pub id: EndpointId,
    pub name: Option<String>,
    /// `false` if it was in the config already, which was left unchanged.
    pub added: bool,
}

impl Paired {
    /// From a saved peer and whether it was new.
    pub fn new((peer, added): (Peer, bool)) -> Self {
        Self {
            id: peer.id,
            name: peer.name,
            added,
        }
    }
}

pub async fn run(args: Args) -> Result<()> {
    let paths = match args.config {
        Some(config_file) => Paths::beside_config(config_file),
        None => Paths::discover()?,
    };
    let daemon = control::connect(&paths)
        .await
        .context("cannot reach the running `clipbroad run`")?;
    let through_daemon = daemon.is_some();
    let paired = match (daemon, args.ticket) {
        (Some(daemon), ticket) => through(daemon, ticket.as_ref(), show_ticket).await?,
        (None, Some(ticket)) => Paired::new(join(&paths, &ticket).await?),
        (None, None) => Paired::new(host(&paths, TICKET_TTL, show_ticket).await?),
    };

    let name = paired.name.as_deref().unwrap_or("the other machine");
    if paired.added {
        println!("Paired with {name:?} ({}).", paired.id);
    } else {
        println!(
            "Paired with {name:?} ({}), which was already in {}: left it unchanged.",
            paired.id,
            paths.config_file.display()
        );
    }
    if through_daemon {
        println!("`clipbroad run` syncs with it from now on.");
    } else {
        println!("Start `clipbroad run` to sync with it.");
    }
    Ok(())
}

fn show_ticket(ticket: &Ticket) {
    let minutes = TICKET_TTL.as_secs() / 60;
    println!(
        "On the other machine, run within {minutes} minutes:\n\n    clipbroad pair {ticket}\n\n\
         The ticket works once. Waiting..."
    );
}

/// Pairs through the running daemon: shows the ticket it hosts through `show`, or has it join
/// `ticket`, and waits for the result.
pub async fn through<S: control::Stream>(
    mut daemon: S,
    ticket: Option<&Ticket>,
    show: impl FnOnce(&Ticket),
) -> Result<Paired> {
    let request = match ticket {
        Some(ticket) => Request::Join(ticket.to_string()),
        None => Request::Host,
    };
    control::write(&mut daemon, &request).await?;
    let mut show = Some(show);
    loop {
        match control::read(&mut daemon).await? {
            Reply::Ticket(text) => {
                let ticket: Ticket = text.parse().context("the daemon sent an invalid ticket")?;
                if let Some(show) = show.take() {
                    show(&ticket);
                }
            }
            Reply::Paired(paired) => return Ok(paired),
            Reply::Failed(error) => anyhow::bail!(error),
        }
    }
}

/// Shows a ticket through `show`, waits up to `ttl` for the other machine and saves it. Returns
/// the peer and whether it was new.
async fn host(paths: &Paths, ttl: Duration, show: impl FnOnce(&Ticket)) -> Result<(Peer, bool)> {
    let (config, identity) = load(paths)?;
    let host = Host::bind(&identity, config.listen_port, hostname())
        .await
        .with_context(|| port_in_use(&config))?;
    check_reachable(&host)?;
    show(host.ticket());
    finish_host(host, paths, ttl, None).await
}

/// Starts hosting on the daemon's node. Show the ticket, then call [`finish_host`].
pub fn host_on(node: &Node) -> Result<Host> {
    let host = node.host_pairing(hostname())?;
    check_reachable(&host)?;
    Ok(host)
}

fn check_reachable(host: &Host) -> Result<()> {
    anyhow::ensure!(
        !host.ticket().addrs.is_empty(),
        "this machine has no network address that the other machine could use"
    );
    Ok(())
}

/// Waits up to `ttl` for the other machine and saves it to the config, and to `node` if the
/// daemon hosts. Returns the peer and whether it was new.
pub async fn finish_host(
    host: Host,
    paths: &Paths,
    ttl: Duration,
    node: Option<&Node>,
) -> Result<(Peer, bool)> {
    let pending = host.accept(ttl).await?;
    let peer = to_peer(pending.peer());
    // Saved before confirming, so the other machine only saves this one if this one saved it.
    // On an error, `pending` is dropped, which rejects the request.
    let added = save(paths, &peer)?;
    if let Some(node) = node {
        node.add_peer(&peer);
    }
    pending.confirm().await?;
    Ok((peer, added))
}

/// Pairs with the machine that showed `ticket` and saves it. Returns the peer and whether it
/// was new.
async fn join(paths: &Paths, ticket: &Ticket) -> Result<(Peer, bool)> {
    let (config, identity) = load(paths)?;
    let paired = match pair::join(&identity, config.listen_port, ticket, hostname()).await {
        Err(PairError::Start(e)) => return Err(anyhow::Error::new(e).context(port_in_use(&config))),
        result => result?,
    };
    let peer = to_peer(&paired);
    let added = save(paths, &peer)?;
    Ok((peer, added))
}

/// Pairs with the machine that showed `ticket` from the daemon's node, saves it and starts
/// syncing with it. Returns the peer and whether it was new.
pub async fn join_on(node: &Node, paths: &Paths, ticket: &Ticket) -> Result<(Peer, bool)> {
    let paired = node.join_pairing(ticket, hostname()).await?;
    let peer = to_peer(&paired);
    let added = save(paths, &peer)?;
    node.add_peer(&peer);
    Ok((peer, added))
}

fn load(paths: &Paths) -> Result<(Config, IdentityKey)> {
    let config = Config::load_or_default(&paths.config_file)?;
    let identity = IdentityKey::load_or_generate(&paths.secret_key_file)?;
    Ok((config, identity))
}

fn save(paths: &Paths, peer: &Peer) -> Result<bool> {
    Config::append_peer(&paths.config_file, peer).context("could not save the new peer")
}

fn to_peer(paired: &PairedPeer) -> Peer {
    Peer {
        id: paired.id,
        name: paired.name.clone(),
        addrs: paired.addrs.clone(),
    }
}

/// Without a reachable daemon, pairing listens on the normal port itself. A daemon that holds
/// the port but did not answer may be an older version, without the control channel.
fn port_in_use(config: &Config) -> String {
    format!(
        "cannot use port {}; if an older `clipbroad run` is running, restart it or stop it \
         while pairing",
        config.listen_port
    )
}

/// This machine's short hostname, which the other machine saves as this peer's name.
fn hostname() -> Option<String> {
    #[cfg(unix)]
    let name = rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned();
    #[cfg(not(unix))]
    let name = std::env::var("COMPUTERNAME").ok()?;
    // macOS adds `.local`; some Linux machines use their full domain name.
    let short = name.split('.').next().unwrap_or_default();
    (!short.is_empty()).then(|| short.to_owned())
}

#[cfg(test)]
mod tests;
