//! Node configuration and identity: the TOML config file, the secret key file and where both live.
//!
//! ```
//! use clipbroad_config::Config;
//!
//! let config = Config::from_toml_str("listen_port = 4000").expect("valid config");
//! assert_eq!(config.listen_port, 4000);
//! assert_eq!(config.peers, []);
//! ```

mod config;
mod identity;
mod paths;

use std::{io, path::PathBuf};

pub use config::{Config, ParseError, Peer};
pub use identity::IdentityKey;
pub use paths::Paths;

/// Errors from loading the config or the identity.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// No home directory, so the per-OS config and data directories are unknown.
    #[error("could not determine the home directory to locate clipbroad's files")]
    NoHomeDir,

    /// A file or directory could not be read or written.
    #[error("could not access {}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// The config file exists but is not valid.
    #[error("invalid config file {}", path.display())]
    Config {
        path: PathBuf,
        #[source]
        source: ParseError,
    },

    /// The secret key file does not hold exactly 32 bytes.
    #[error(
        "secret key file {} has {len} bytes, expected 32; it is corrupt. Delete it to generate a \
         new identity (paired machines will then need to pair again)",
        path.display()
    )]
    KeyLength { path: PathBuf, len: usize },

    /// The secret key file can be read by other users (Unix only).
    #[error(
        "secret key file {} is accessible by other users (mode {mode:o}); run `chmod 600 {}`",
        path.display(),
        path.display()
    )]
    KeyPermissions { path: PathBuf, mode: u32 },

    /// The OS random number generator failed.
    #[error("could not generate a random secret key")]
    Random(#[source] getrandom::Error),
}

impl Error {
    fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
