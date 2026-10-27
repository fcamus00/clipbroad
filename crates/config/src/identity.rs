use std::{
    fmt, fs,
    io::{self, Write as _},
    path::Path,
};

use crate::Error;

/// The node's secret identity: the 32 bytes of an Ed25519 secret key.
///
/// The transport turns it into its own key type, e.g. `iroh::SecretKey::from_bytes(&key.to_bytes())`.
/// Any 32 bytes are a valid Ed25519 secret key, so a random value is enough to create one.
#[derive(Clone, PartialEq, Eq)]
pub struct IdentityKey([u8; 32]);

impl IdentityKey {
    /// Creates a new random key from the OS random number generator.
    ///
    /// # Errors
    ///
    /// [`Error::Random`] if the OS generator is unavailable.
    pub fn generate() -> Result<Self, Error> {
        let mut bytes = [0; 32];
        getrandom::fill(&mut bytes).map_err(Error::Random)?;
        Ok(Self(bytes))
    }

    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0
    }

    /// Reads the key stored at `path`, creating a new one there the first time.
    ///
    /// A new file holds the 32 raw bytes. On Unix it is created with mode `0600` (and missing parent
    /// directories with `0700`); on Windows it inherits the permissions of its directory, which
    /// under `%LOCALAPPDATA%` only the user, administrators and the system can read.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the file cannot be read or written, [`Error::KeyLength`] if it is corrupt,
    /// [`Error::KeyPermissions`] if on Unix other users can access it, and [`Error::Random`] if a
    /// new key cannot be generated.
    pub fn load_or_generate(path: &Path) -> Result<Self, Error> {
        match fs::symlink_metadata(path) {
            Ok(_) => return Self::load(path),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io(path, e)),
        }

        let key = Self::generate()?;
        match key.write_new(path) {
            Ok(()) => Ok(key),
            // Another process created it in the meantime, complete (see `write_new`): use theirs.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Self::load(path),
            Err(e) => Err(Error::io(path, e)),
        }
    }

    /// Reads an existing key file.
    ///
    /// # Errors
    ///
    /// Same as [`IdentityKey::load_or_generate`], except that a missing file is an [`Error::Io`].
    pub fn load(path: &Path) -> Result<Self, Error> {
        check_permissions(path)?;
        let bytes = fs::read(path).map_err(|e| Error::io(path, e))?;
        let bytes = <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| Error::KeyLength {
            path: path.to_owned(),
            len: bytes.len(),
        })?;
        Ok(Self(bytes))
    }

    /// Writes the key to `path`, which must not exist yet (`AlreadyExists` otherwise).
    ///
    /// The bytes go to a temporary file that is then moved into place without overwriting, so
    /// `path` appears complete or not at all. A second process starting at the same moment never
    /// reads a half-written key.
    fn write_new(&self, path: &Path) -> io::Result<()> {
        let dir = path.parent().unwrap_or(Path::new("."));
        create_private_dir(dir)?;
        // `tempfile` creates the file with mode 0600 on Unix and deletes it if we stop early.
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        tmp.write_all(&self.0)?;
        tmp.as_file().sync_all()?;
        tmp.persist_noclobber(path).map(drop).map_err(|e| e.error)
    }
}

impl fmt::Debug for IdentityKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("IdentityKey(<redacted>)")
    }
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

#[cfg(unix)]
fn check_permissions(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = fs::metadata(path)
        .map_err(|e| Error::io(path, e))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(Error::KeyPermissions {
            path: path.to_owned(),
            mode: mode & 0o777,
        });
    }
    Ok(())
}

/// Windows has no mode bits; access is governed by the directory's ACL (see `load_or_generate`).
#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)] // Same signature as the Unix version.
fn check_permissions(_path: &Path) -> Result<(), Error> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_run_generates_and_later_runs_reuse() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("nested").join("secret.key");

        let first = IdentityKey::load_or_generate(&path).expect("generated");
        assert_eq!(fs::read(&path).expect("key file exists").len(), 32);

        let second = IdentityKey::load_or_generate(&path).expect("loaded");
        assert_eq!(first, second);
    }

    #[test]
    fn generated_keys_differ() {
        let a = IdentityKey::generate().expect("random");
        let b = IdentityKey::generate().expect("random");
        assert_ne!(a, b);
    }

    #[test]
    fn bytes_round_trip() {
        let key = IdentityKey::from_bytes([7; 32]);
        assert_eq!(key.to_bytes(), [7; 32]);
    }

    #[test]
    fn debug_does_not_leak_the_key() {
        let key = IdentityKey::from_bytes([0xab; 32]);
        let text = format!("{key:?}");
        assert!(!text.contains("171"), "{text}");
        assert!(!text.to_lowercase().contains("ab, "), "{text}");
        assert!(text.contains("redacted"));
    }

    #[test]
    fn corrupt_key_file_is_rejected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("secret.key");
        IdentityKey::from_bytes([1; 32])
            .write_new(&path)
            .expect("write");
        // Append garbage while keeping the private permissions.
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(b"extra"))
            .expect("append");

        let err = IdentityKey::load_or_generate(&path).expect_err("corrupt");
        assert!(matches!(err, Error::KeyLength { len: 37, .. }), "{err}");
        assert!(err.to_string().contains("expected 32"), "{err}");
    }

    #[test]
    fn write_new_never_overwrites_and_cleans_up() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("secret.key");
        IdentityKey::from_bytes([1; 32])
            .write_new(&path)
            .expect("first write");

        let err = IdentityKey::from_bytes([2; 32])
            .write_new(&path)
            .expect_err("target exists");

        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&path).expect("key"), [1; 32]);
        let files = fs::read_dir(dir.path()).expect("list").count();
        assert_eq!(files, 1, "temporary file left behind");
    }

    #[test]
    fn missing_key_file_is_an_error_for_load() {
        let dir = tempfile::tempdir().expect("temp dir");
        let err = IdentityKey::load(&dir.path().join("secret.key")).expect_err("missing");
        assert!(matches!(err, Error::Io { .. }), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn key_file_and_new_dirs_are_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("temp dir");
        let parent = dir.path().join("clipbroad");
        let path = parent.join("secret.key");
        IdentityKey::load_or_generate(&path).expect("generated");

        let mode = |p: &Path| fs::metadata(p).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&parent), 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn key_readable_by_others_is_rejected() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("secret.key");
        IdentityKey::load_or_generate(&path).expect("generated");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");

        let err = IdentityKey::load_or_generate(&path).expect_err("too open");
        assert!(
            matches!(err, Error::KeyPermissions { mode: 0o644, .. }),
            "{err}"
        );
        assert!(err.to_string().contains("chmod 600"), "{err}");
    }
}
