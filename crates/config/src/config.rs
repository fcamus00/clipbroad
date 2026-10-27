use std::{
    collections::HashSet,
    fmt::Write as _,
    fs,
    io::{self, Write as _},
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

    /// Adds `peer` to the config file at `path` by appending a `[[peers]]` block, so the rest of
    /// the file (comments included) stays as it is. Creates the file and its directory if needed.
    ///
    /// Returns `false`, without touching the file, if a peer with the same id is already there.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the file cannot be read or written, [`Error::Config`] if the current file
    /// is invalid (it is left unchanged).
    pub fn append_peer(path: &Path, peer: &Peer) -> Result<bool, Error> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(Error::io(path, e)),
        };
        let invalid = |source| Error::Config {
            path: path.to_owned(),
            source,
        };
        let current = Self::from_toml_str(&text).map_err(invalid)?;
        if current.peers.iter().any(|p| p.id == peer.id) {
            return Ok(false);
        }

        let mut block = String::new();
        if !text.is_empty() {
            block.push_str(if text.ends_with('\n') { "\n" } else { "\n\n" });
        }
        block.push_str("[[peers]]\n");
        if let Some(name) = &peer.name {
            // `Value`'s display quotes and escapes the string.
            let _ = writeln!(block, "name = {}", toml::Value::from(name.as_str()));
        }
        let _ = writeln!(block, "id = \"{}\"", peer.id);
        let addrs: Vec<String> = peer.addrs.iter().map(|a| format!("\"{a}\"")).collect();
        let _ = writeln!(block, "addrs = [{}]", addrs.join(", "));
        // Check the result before writing anything.
        Self::from_toml_str(&(text + &block)).map_err(invalid)?;

        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
        }
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut file| file.write_all(block.as_bytes()))
            .map_err(|e| Error::io(path, e))?;
        Ok(true)
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

    fn peer(name: Option<&str>) -> Peer {
        Peer {
            id: ID_A.parse().expect("valid id"),
            name: name.map(str::to_owned),
            addrs: vec!["192.168.1.20:42420".parse().expect("valid")],
        }
    }

    #[test]
    fn append_peer_creates_file_and_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("clipbroad").join("config.toml");

        assert!(Config::append_peer(&path, &peer(Some("laptop"))).expect("added"));

        let text = fs::read_to_string(&path).expect("file created");
        assert_eq!(
            text,
            format!(
                "[[peers]]\nname = \"laptop\"\nid = \"{ID_A}\"\naddrs = [\"192.168.1.20:42420\"]\n"
            )
        );
        let config = Config::load_or_default(&path).expect("valid");
        assert_eq!(config.peers, [peer(Some("laptop"))]);
    }

    #[test]
    fn append_peer_keeps_comments_and_skips_duplicates() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        let original = "# my port\nlisten_port = 5000";
        fs::write(&path, original).expect("write");

        assert!(Config::append_peer(&path, &peer(None)).expect("added"));
        let text = fs::read_to_string(&path).expect("read");
        assert!(
            text.starts_with("# my port\nlisten_port = 5000\n\n[[peers]]\n"),
            "{text}"
        );
        assert!(!text.contains("name"), "{text}");

        assert!(!Config::append_peer(&path, &peer(Some("again"))).expect("no error"));
        assert_eq!(fs::read_to_string(&path).expect("read"), text);
        let config = Config::load_or_default(&path).expect("valid");
        assert_eq!(config.listen_port.get(), 5000);
        assert_eq!(config.peers, [peer(None)]);
    }

    #[test]
    fn append_peer_escapes_the_name() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        let name = "evil\"\n[[peers]]\\";

        assert!(Config::append_peer(&path, &peer(Some(name))).expect("added"));
        let config = Config::load_or_default(&path).expect("valid");
        assert_eq!(config.peers, [peer(Some(name))]);
    }

    #[test]
    fn append_peer_leaves_an_invalid_file_alone() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        fs::write(&path, "listen_prot = 5000\n").expect("write");

        let err = Config::append_peer(&path, &peer(None)).expect_err("invalid file");
        assert!(matches!(err, Error::Config { .. }), "{err}");
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            "listen_prot = 5000\n"
        );
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
