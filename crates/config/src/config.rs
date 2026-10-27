use std::{
    collections::HashSet,
    fs, io,
    net::SocketAddr,
    num::{NonZeroU16, NonZeroU64},
    path::Path,
};

use clipbroad_core::protocol::MAX_FRAME_LEN;
use iroh::EndpointId;
use serde::Deserialize;

use crate::Error;

/// Contents of `config.toml`. Every field is optional in the file; missing ones take the default.
///
/// See `docs/config.example.toml` for a commented example.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// UDP port the node listens on. Fixed so that peers can reach it through static addresses;
    /// non-zero because port 0 means "any free port", which changes on every start.
    pub listen_port: NonZeroU16,
    /// Clips up to this size (in bytes) travel inside the clip message itself. At most
    /// [`Config::MAX_INLINE_MAX_BYTES`].
    pub inline_max_bytes: NonZeroU64,
    /// Paired machines.
    pub peers: Vec<Peer>,
}

/// A paired machine.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Peer {
    /// The peer's public identity, written in the file as 64 lowercase hexadecimal characters.
    pub id: EndpointId,
    /// Human-friendly label, only used in logs and status output.
    pub name: Option<String>,
    /// Addresses to dial directly, for networks where LAN discovery does not reach,
    /// such as a Tailscale address (`100.x.y.z:port`).
    #[serde(default)]
    pub addrs: Vec<SocketAddr>,
}

/// Why a config text was rejected.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ParseError {
    /// Not valid TOML, or a field has the wrong name or type. The message points at the line.
    #[error(transparent)]
    Toml(#[from] toml::de::Error),

    /// Valid TOML, but a value is not acceptable (repeated peer id, `inline_max_bytes` too large).
    #[error("{0}")]
    Invalid(String),
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_port: Self::DEFAULT_LISTEN_PORT,
            inline_max_bytes: Self::DEFAULT_INLINE_MAX_BYTES,
            peers: Vec::new(),
        }
    }
}

impl Config {
    pub const DEFAULT_LISTEN_PORT: NonZeroU16 = NonZeroU16::new(42_420).expect("non-zero");
    /// 8 MiB: enough for text and typical screenshots.
    pub const DEFAULT_INLINE_MAX_BYTES: NonZeroU64 =
        NonZeroU64::new(8 * 1024 * 1024).expect("non-zero");
    /// Largest accepted `inline_max_bytes`: a clip must fit in one protocol frame, so this is the
    /// frame limit minus 64 KiB for the clip's metadata (which takes a few hundred bytes).
    pub const MAX_INLINE_MAX_BYTES: u64 = (MAX_FRAME_LEN - 64 * 1024) as u64;

    /// Parses and validates a config from TOML text.
    ///
    /// # Errors
    ///
    /// [`ParseError::Toml`] for syntax errors, unknown fields, wrong types or out-of-range values
    /// (including zero where a non-zero number is required) and malformed peer ids, and
    /// [`ParseError::Invalid`] for a repeated peer id or an `inline_max_bytes` above
    /// [`Config::MAX_INLINE_MAX_BYTES`].
    pub fn from_toml_str(text: &str) -> Result<Self, ParseError> {
        let config: Self = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    /// Reads the config file at `path`, or returns the defaults if it does not exist.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the file exists but cannot be read, [`Error::Config`] if it is invalid.
    pub fn load_or_default(path: &Path) -> Result<Self, Error> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(Error::io(path, e)),
        };
        Self::from_toml_str(&text).map_err(|source| Error::Config {
            path: path.to_owned(),
            source,
        })
    }

    fn validate(&self) -> Result<(), ParseError> {
        if self.inline_max_bytes.get() > Self::MAX_INLINE_MAX_BYTES {
            return Err(ParseError::Invalid(format!(
                "inline_max_bytes is {}, but the maximum is {} (16 MiB minus 64 KiB)",
                self.inline_max_bytes,
                Self::MAX_INLINE_MAX_BYTES
            )));
        }
        let mut seen = HashSet::new();
        for peer in &self.peers {
            if !seen.insert(peer.id) {
                return Err(ParseError::Invalid(format!(
                    "peer id {} appears more than once",
                    peer.id
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;

    const EXAMPLE: &str = include_str!("../../../docs/config.example.toml");
    const ID_A: &str = "ae58ff8833241ac82d6ff7611046ed67b5072d142c588d0063e942d9a75502b6";

    /// Joins an error and all its causes, the way the daemon prints them.
    fn chain(err: &dyn std::error::Error) -> String {
        let mut text = err.to_string();
        let mut source = err.source();
        while let Some(cause) = source {
            text.push_str(": ");
            text.push_str(&cause.to_string());
            source = cause.source();
        }
        text
    }

    #[test]
    fn empty_file_gives_defaults() {
        let config = Config::from_toml_str("").expect("empty config is valid");
        assert_eq!(config, Config::default());
        assert_eq!(config.listen_port.get(), 42_420);
        assert_eq!(config.inline_max_bytes.get(), 8 * 1024 * 1024);
        assert_eq!(config.peers, []);
    }

    #[test]
    fn documented_example_parses() {
        let config = Config::from_toml_str(EXAMPLE).expect("docs/config.example.toml is valid");
        assert_eq!(config.listen_port.get(), 42_420);
        assert_eq!(config.inline_max_bytes.get(), 8_388_608);
        assert_eq!(config.peers.len(), 2);

        let laptop = &config.peers[0];
        assert_eq!(laptop.name.as_deref(), Some("laptop"));
        assert_eq!(laptop.id.to_string(), ID_A);
        assert_eq!(
            laptop.addrs,
            vec![
                "100.101.102.103:42420"
                    .parse::<SocketAddr>()
                    .expect("valid"),
                "[fd7a:115c:a1e0::1]:42420".parse().expect("valid"),
            ]
        );

        let desktop = &config.peers[1];
        assert_eq!(desktop.name, None);
        assert_eq!(desktop.addrs, []);
    }

    #[test]
    fn partial_file_keeps_other_defaults() {
        let config = Config::from_toml_str("listen_port = 5000").expect("valid");
        assert_eq!(config.listen_port.get(), 5000);
        assert_eq!(config.inline_max_bytes, Config::DEFAULT_INLINE_MAX_BYTES);
    }

    #[test]
    fn syntax_error_points_at_the_line() {
        let err = Config::from_toml_str("listen_port = 5000\npeers = [\n").expect_err("invalid");
        assert!(matches!(err, ParseError::Toml(_)));
        assert!(err.to_string().contains("line 2"), "{err}");
    }

    #[test]
    fn wrong_type_is_reported_with_field_name() {
        let err = Config::from_toml_str("listen_port = \"fast\"").expect_err("invalid");
        let text = err.to_string();
        assert!(text.contains("line 1"), "{text}");
        assert!(text.contains("listen_port"), "{text}");
    }

    #[test]
    fn out_of_range_port_is_rejected() {
        let err = Config::from_toml_str("listen_port = 70000").expect_err("invalid");
        assert!(err.to_string().contains("listen_port"), "{err}");
    }

    #[test]
    fn unknown_field_is_rejected() {
        let err = Config::from_toml_str("listen_prot = 5000").expect_err("typo");
        assert!(err.to_string().contains("listen_prot"), "{err}");
    }

    #[test]
    fn bad_peer_address_is_rejected() {
        let text = format!("[[peers]]\nid = \"{ID_A}\"\naddrs = [\"100.1.2.3\"]\n");
        let err = Config::from_toml_str(&text).expect_err("address without port");
        assert!(matches!(err, ParseError::Toml(_)), "{err}");
    }

    #[test]
    fn bad_peer_id_is_rejected() {
        let err = Config::from_toml_str("[[peers]]\nid = \"not-a-key\"\n").expect_err("invalid");
        assert!(matches!(err, ParseError::Toml(_)), "{err}");
        assert!(err.to_string().contains("line 2"), "{err}");
    }

    #[test]
    fn duplicate_peer_is_rejected() {
        let text = format!("[[peers]]\nid = \"{ID_A}\"\n[[peers]]\nid = \"{ID_A}\"\n");
        let err = Config::from_toml_str(&text).expect_err("duplicate");
        assert!(err.to_string().contains("more than once"), "{err}");
    }

    #[test]
    fn zero_values_are_rejected() {
        for text in ["inline_max_bytes = 0", "listen_port = 0"] {
            let err = Config::from_toml_str(text).expect_err("zero");
            assert!(err.to_string().contains("nonzero"), "{err}");
        }
    }

    #[test]
    fn inline_max_bytes_must_fit_in_a_frame() {
        let max = Config::MAX_INLINE_MAX_BYTES;
        let config = Config::from_toml_str(&format!("inline_max_bytes = {max}")).expect("valid");
        assert_eq!(config.inline_max_bytes.get(), max);

        let err = Config::from_toml_str(&format!("inline_max_bytes = {}", max + 1))
            .expect_err("too large");
        assert!(matches!(err, ParseError::Invalid(_)), "{err}");
        assert!(err.to_string().contains("inline_max_bytes"), "{err}");
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = Config::load_or_default(&dir.path().join("config.toml")).expect("defaults");
        assert_eq!(config, Config::default());
    }

    #[test]
    fn file_error_names_the_file_and_line() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        fs::write(&path, "listen_port = 5000\nlisten_port = 6000\n").expect("write");

        let err = Config::load_or_default(&path).expect_err("duplicate key");
        assert!(matches!(err, Error::Config { .. }));
        assert!(err.source().is_some());
        let text = chain(&err);
        assert!(text.contains("config.toml"), "{text}");
        assert!(text.contains("line 2"), "{text}");
    }

    #[test]
    fn file_is_loaded() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        fs::write(&path, EXAMPLE).expect("write");
        let config = Config::load_or_default(&path).expect("valid");
        assert_eq!(config.peers.len(), 2);
    }
}
