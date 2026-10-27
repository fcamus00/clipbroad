//! `clipbroad pair`: adds a peer to the config by passing a ticket from one machine to the other.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clipbroad_config::{Config, IdentityKey, Paths, Peer};
use clipbroad_net::pair::{self, Host, PairError, PairedPeer, Ticket};

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
const TICKET_TTL: Duration = Duration::from_mins(5);

pub async fn run(args: Args) -> Result<()> {
    let paths = match args.config {
        Some(config_file) => Paths::beside_config(config_file),
        None => Paths::discover()?,
    };
    let (peer, added) = match args.ticket {
        Some(ticket) => join(&paths, &ticket).await?,
        None => host(&paths, TICKET_TTL, show_ticket).await?,
    };

    let name = peer.name.as_deref().unwrap_or("the other machine");
    if added {
        println!("Paired with {name:?} ({}).", peer.id);
    } else {
        println!(
            "Paired with {name:?} ({}), which was already in {}: left it unchanged.",
            peer.id,
            paths.config_file.display()
        );
    }
    println!("Start `clipbroad run` (again) on both machines to sync with it.");
    Ok(())
}

fn show_ticket(ticket: &Ticket) {
    let minutes = TICKET_TTL.as_secs() / 60;
    println!(
        "On the other machine, stop `clipbroad run` if it is running, then run within {minutes} \
         minutes:\n\n    clipbroad pair {ticket}\n\nThe ticket works once. Waiting..."
    );
}

/// Shows a ticket through `show`, waits up to `ttl` for the other machine and saves it. Returns
/// the peer and whether it was new.
async fn host(paths: &Paths, ttl: Duration, show: impl FnOnce(&Ticket)) -> Result<(Peer, bool)> {
    let (config, identity) = load(paths)?;
    let host = Host::bind(&identity, config.listen_port, hostname())
        .await
        .with_context(|| port_in_use(&config))?;
    anyhow::ensure!(
        !host.ticket().addrs.is_empty(),
        "this machine has no network address that the other machine could use"
    );
    show(host.ticket());

    let pending = host.accept(ttl).await?;
    let peer = to_peer(pending.peer());
    // Saved before confirming, so the other machine only saves this one if this one saved it.
    // On an error, `pending` is dropped, which rejects the request.
    let added = save(paths, &peer)?;
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

/// Pairing listens on the normal port, so it cannot run next to `clipbroad run`.
fn port_in_use(config: &Config) -> String {
    format!(
        "cannot use port {}; if `clipbroad run` is running, stop it while pairing",
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
