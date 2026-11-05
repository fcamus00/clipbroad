//! `clipbroad reset`: gives this machine a new identity, so it can join a mesh again after it was
//! removed (see [`crate::mesh`]).
//!
//! It deletes `secret.key` and `members.toml`, and the peers in `config.toml`, which only know the
//! old identity; the next start creates a new key. The rest of `config.toml` stays as it is. If
//! `clipbroad run` is running with the same config, `reset` asks it over the control channel, and
//! the daemon deletes them once it has stopped and starts again.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clipbroad_config::{Config, IdentityKey, Paths};
use iroh::{EndpointId, SecretKey};

use crate::control::{self, Reply, Request};

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Config file to use instead of the default one, as given to `run --config`.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
}

pub async fn run(args: Args) -> Result<()> {
    let paths = match args.config {
        Some(config_file) => Paths::beside_config(config_file),
        None => Paths::discover()?,
    };
    let old = identity(&paths);
    let daemon = control::connect(&paths)
        .await
        .context("cannot reach the running `clipbroad run`")?;
    if let Some(mut daemon) = daemon {
        control::write(&mut daemon, &Request::Reset).await?;
        match control::read(&mut daemon).await? {
            Reply::Quitting => {}
            Reply::Failed(error) => bail!(error),
            _ => bail!("the daemon answered something else"),
        }
        println!("`clipbroad run` restarts with a new identity, without peers.");
    } else {
        let peers = wipe(&paths)?;
        if peers > 0 {
            println!(
                "Deleted the {peers} peers in {}.",
                paths.config_file.display()
            );
        }
        println!("The next `clipbroad run` starts with a new identity, without peers.");
    }
    if let Some(old) = old {
        println!(
            "The old identity was {old}. If no member removed it yet, remove it from one of \
             them with `clipbroad remove {}`.",
            &old.to_string()[..10]
        );
    }
    println!("Pair this machine with any member of the mesh to join it again.");
    Ok(())
}

/// Deletes the identity, the members file and the peers in the config file, which would become
/// members again on the next start. Missing files are fine. Returns how many peers the config
/// file had.
pub fn wipe(paths: &Paths) -> Result<usize> {
    remove(&paths.secret_key_file)?;
    remove(&paths.members_file())?;
    Ok(Config::remove_peers(&paths.config_file)?)
}

/// The current identity, if there is a valid one.
fn identity(paths: &Paths) -> Option<EndpointId> {
    let key = IdentityKey::load(&paths.secret_key_file).ok()?;
    Some(SecretKey::from_bytes(&key.to_bytes()).public())
}

fn remove(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(error).with_context(|| format!("cannot delete {}", path.display()))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wipe_deletes_the_identity_the_members_and_the_config_peers() {
        let dir = tempfile::tempdir().expect("temp dir");
        let paths = Paths::beside_config(dir.path().join("config.toml"));
        let peer = SecretKey::from_bytes(&[1; 32]).public();
        let config = format!("listen_port = 5000\n\n[[peers]]\nid = \"{peer}\"\n");
        fs::write(&paths.config_file, config).expect("write");
        fs::write(paths.members_file(), "").expect("write");
        let old = IdentityKey::load_or_generate(&paths.secret_key_file).expect("created");
        assert!(identity(&paths).is_some());

        assert_eq!(wipe(&paths).expect("deleted"), 1);
        assert!(!paths.secret_key_file.exists());
        assert!(!paths.members_file().exists());
        let config = Config::load_or_default(&paths.config_file).expect("valid");
        assert_eq!(config.peers, []);
        assert_eq!(config.listen_port.get(), 5000);
        let new = IdentityKey::load_or_generate(&paths.secret_key_file).expect("created");
        assert_ne!(old, new);

        // Nothing to delete is fine too.
        fs::remove_file(&paths.secret_key_file).expect("delete");
        assert_eq!(wipe(&paths).expect("nothing to delete"), 0);
    }
}
