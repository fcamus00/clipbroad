//! `clipbroad remove`: takes a machine out of the mesh for good.
//!
//! If `clipbroad run` is running with the same config, it removes the machine, disconnects from
//! it and tells the other members right away. Otherwise `remove` changes the members file, and
//! the daemon tells the others when it starts.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clipbroad_config::Paths;

use crate::control::{self, Reply, Request};
use crate::{mesh, pair};

#[derive(Debug, clap::Args)]
pub struct Args {
    /// The machine's name, as `clipbroad status` shows it, or the first characters of its id.
    machine: String,

    /// Config file to use instead of the default one, as given to `run --config`.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
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
    let removed = if let Some(mut daemon) = daemon {
        control::write(&mut daemon, &Request::Remove(args.machine)).await?;
        match control::read(&mut daemon).await? {
            Reply::Removed(label) => label,
            Reply::Failed(error) => bail!(error),
            _ => bail!("the daemon answered something else"),
        }
    } else {
        let (_, _, mesh) = pair::load(&paths)?;
        mesh::label(&mesh.remove(&args.machine, None)?)
    };
    println!(
        "Removed {removed} from the mesh, for good. The other members stop syncing with it as \
         they hear of it. To join again, that machine needs a new identity: run \
         `clipbroad reset` on it and pair it again."
    );
    if !through_daemon {
        println!("Start `clipbroad run` to tell the other members.");
    }
    Ok(())
}
