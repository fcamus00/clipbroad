//! Cache of files received from peers: `<dir>/<hash>/<name>`, written only once verified.
//!
//! A file being received goes to a temporary file inside its `<hash>` directory, hashed as it is
//! written ([`Incoming`]). Only when its size and BLAKE3 hash match the announcement is it renamed
//! to its final name, so a path in the cache always holds a verified file. A transfer that fails
//! or is cancelled deletes its temporary file; leftovers from a crash are deleted when the cache
//! is opened.
//!
//! [`FileCache::clean`] keeps the cache within `file_cache_max_bytes` and `file_cache_max_days`,
//! deleting the oldest files first (by modification time, which a cache hit refreshes). It never
//! deletes the files of the list it is given, which is the newest received clip and so what the
//! clipboard points to.
//!
//! On Unix the directories are created `0700` and the files `0600`, since copied files may be
//! private.

use std::{
    collections::HashSet,
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use clipbroad_config::Config;
use clipbroad_core::{ContentHash, FileEntry, FileList};
use tempfile::NamedTempFile;
use tokio::io::AsyncWriteExt as _;

/// Temporary files are `.clipbroad-<random>.part` inside the file's `<hash>` directory.
const TEMP_PREFIX: &str = ".clipbroad-";
const TEMP_SUFFIX: &str = ".part";

/// The cache directory and its limits. Cheap to clone.
#[derive(Debug, Clone)]
pub struct FileCache {
    dir: PathBuf,
    max_file_bytes: u64,
    max_bytes: u64,
    max_age: Duration,
}

/// What [`FileCache::clean`] deleted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cleaned {
    pub files: usize,
    pub bytes: u64,
}

/// Why a received file was not stored.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    #[error("could not write to the file cache")]
    Io(#[from] io::Error),
    #[error("peer sent more bytes than the {announced} it announced")]
    TooManyBytes { announced: u64 },
    #[error("stream ended after {got} of the {announced} announced bytes")]
    Truncated { got: u64, announced: u64 },
    #[error("file content does not match the announced hash")]
    HashMismatch,
}

impl FileCache {
    /// Opens the cache in `dir` with the limits in `config`, creating the directory if needed and
    /// deleting temporary files left by a crash. Blocks on the file system.
    ///
    /// Only one process may use a cache directory: opening it deletes the other one's transfers.
    ///
    /// # Errors
    ///
    /// If the directory cannot be created or read.
    pub fn open(dir: PathBuf, config: &Config) -> io::Result<Self> {
        create_private_dir(&dir)?;
        for hash_dir in hash_dirs(&dir)? {
            for file in fs::read_dir(&hash_dir)? {
                let path = file?.path();
                if is_temp(&path) {
                    remove_if_present(&path)?;
                }
            }
        }
        Ok(Self {
            dir,
            max_file_bytes: config.file_max_bytes.get(),
            max_bytes: config.file_cache_max_bytes.get(),
            max_age: Duration::from_secs(u64::from(config.file_cache_max_days.get()) * 86_400),
        })
    }

    /// The cache directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Largest file accepted, in bytes (`file_max_bytes`).
    #[must_use]
    pub const fn max_file_bytes(&self) -> u64 {
        self.max_file_bytes
    }

    /// Largest size of the whole cache, and of all the files of one clip, in bytes
    /// (`file_cache_max_bytes`).
    #[must_use]
    pub const fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Where `entry` is, or will be, once verified: `<dir>/<hash>/<name>`. The name is safe to
    /// join because [`FileEntry`]s only reach this crate inside a checked [`FileList`].
    #[must_use]
    pub fn path_of(&self, entry: &FileEntry) -> PathBuf {
        self.dir.join(entry.hash.to_string()).join(&entry.name)
    }

    /// Whether `entry` is already in the cache, verified again by size and hash in case the file
    /// was changed in place. A hit refreshes the file's modification time when it can, so
    /// cleaning keeps the files in use; a file that no longer matches is deleted. Blocks on the
    /// file system.
    ///
    /// # Errors
    ///
    /// If the file exists but cannot be read or deleted.
    pub(crate) fn contains(&self, entry: &FileEntry) -> io::Result<bool> {
        let path = self.path_of(entry);
        let mut file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        let mut hasher = blake3::Hasher::new();
        let size = io::copy(&mut file, &mut hasher)?;
        drop(file);
        if size == entry.size
            && ContentHash::from_bytes(*hasher.finalize().as_bytes()) == entry.hash
        {
            // Best effort: Windows refuses write access while an app has the file open.
            let _ = fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_modified(SystemTime::now()));
            return Ok(true);
        }
        remove_if_present(&path)?;
        Ok(false)
    }

    /// Starts receiving `entry` into a temporary file. Blocks briefly on the file system.
    ///
    /// # Errors
    ///
    /// If the directory or the temporary file cannot be created.
    pub(crate) fn begin(&self, entry: &FileEntry) -> io::Result<Incoming> {
        let final_path = self.path_of(entry);
        let dir = final_path.parent().unwrap_or(&self.dir);
        create_private_dir(dir)?;
        let temp = tempfile::Builder::new()
            .prefix(TEMP_PREFIX)
            .suffix(TEMP_SUFFIX)
            .tempfile_in(dir)?;
        let file = tokio::fs::File::from_std(temp.as_file().try_clone()?);
        Ok(Incoming {
            temp,
            file,
            hasher: blake3::Hasher::new(),
            written: 0,
            announced: entry.size,
            hash: entry.hash,
            final_path,
        })
    }

    /// Deletes files older than `file_cache_max_days`, then the oldest files until the cache is
    /// within `file_cache_max_bytes`. Files of `keep` are never deleted (they still count toward
    /// the size). Temporary files of running transfers are left alone. Blocks on the file system.
    ///
    /// # Errors
    ///
    /// If the cache cannot be read or a file cannot be deleted. Files deleted before the error
    /// stay deleted.
    pub fn clean(&self, keep: &FileList) -> io::Result<Cleaned> {
        let keep: HashSet<PathBuf> = keep.entries().iter().map(|e| self.path_of(e)).collect();
        let now = SystemTime::now();
        let mut total = 0_u64;
        let mut candidates = Vec::new();
        for hash_dir in hash_dirs(&self.dir)? {
            for file in fs::read_dir(&hash_dir)? {
                let file = file?;
                let path = file.path();
                let meta = file.metadata()?;
                if !meta.is_file() || is_temp(&path) {
                    continue;
                }
                total += meta.len();
                if !keep.contains(&path) {
                    candidates.push((meta.modified()?, meta.len(), path));
                }
            }
        }
        // Oldest first.
        candidates.sort();

        let mut cleaned = Cleaned::default();
        for (modified, size, path) in candidates {
            let expired = now.duration_since(modified).unwrap_or_default() > self.max_age;
            if !expired && total <= self.max_bytes {
                // Every later file is newer, so none of them is expired either.
                break;
            }
            remove_if_present(&path)?;
            total -= size;
            cleaned.files += 1;
            cleaned.bytes += size;
            // Only succeeds once the directory is empty.
            let _ = fs::remove_dir(path.parent().unwrap_or(&self.dir));
        }
        Ok(cleaned)
    }
}

/// A file being received. Feed it the bytes with [`write`](Self::write), then call
/// [`finish`](Self::finish). Dropping it deletes the temporary file.
#[derive(Debug)]
pub(crate) struct Incoming {
    /// Deletes the temporary file when dropped, unless it was persisted.
    temp: NamedTempFile,
    /// Async handle to the same file.
    file: tokio::fs::File,
    hasher: blake3::Hasher,
    written: u64,
    announced: u64,
    hash: ContentHash,
    final_path: PathBuf,
}

impl Incoming {
    /// Appends `bytes` to the file and the running hash.
    ///
    /// # Errors
    ///
    /// [`StoreError::TooManyBytes`] as soon as the total goes past the announced size (nothing
    /// past it is written), or [`StoreError::Io`].
    pub(crate) async fn write(&mut self, bytes: &[u8]) -> Result<(), StoreError> {
        let len = bytes.len() as u64;
        if len > self.announced - self.written {
            return Err(StoreError::TooManyBytes {
                announced: self.announced,
            });
        }
        self.hasher.update(bytes);
        self.file.write_all(bytes).await?;
        self.written += len;
        Ok(())
    }

    /// Checks the size and hash and, if both match, moves the file to its final path.
    ///
    /// # Errors
    ///
    /// [`StoreError::Truncated`] or [`StoreError::HashMismatch`] if the file does not match the
    /// announcement (the temporary file is deleted), or [`StoreError::Io`].
    pub(crate) async fn finish(mut self) -> Result<PathBuf, StoreError> {
        if self.written != self.announced {
            return Err(StoreError::Truncated {
                got: self.written,
                announced: self.announced,
            });
        }
        if ContentHash::from_bytes(*self.hasher.finalize().as_bytes()) != self.hash {
            return Err(StoreError::HashMismatch);
        }
        self.file.flush().await?;
        // Close the async handle before renaming: Windows refuses to rename some open files.
        drop(self.file);
        self.temp
            .persist(&self.final_path)
            .map_err(|e| StoreError::Io(e.error))?;
        Ok(self.final_path)
    }
}

/// The `<hash>` directories in the cache: only names of 64 lowercase hex characters, so nothing
/// else that ends up in the directory is ever deleted.
fn hash_dirs(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let is_hash = name.len() == 64
            && name
                .to_str()
                .is_some_and(|n| n.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        if is_hash && entry.file_type()?.is_dir() {
            dirs.push(entry.path());
        }
    }
    Ok(dirs)
}

fn is_temp(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(TEMP_PREFIX) && name.ends_with(TEMP_SUFFIX))
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Creates `dir` and its missing parents, `0700` on Unix.
fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};

    use super::*;

    const DAY: Duration = Duration::from_hours(24);

    fn cache(dir: &Path, max_bytes: u64) -> FileCache {
        let config = Config {
            file_max_bytes: NonZeroU64::new(max_bytes).expect("non-zero"),
            file_cache_max_bytes: NonZeroU64::new(max_bytes).expect("non-zero"),
            file_cache_max_days: NonZeroU32::new(7).expect("non-zero"),
            ..Config::default()
        };
        FileCache::open(dir.join("files"), &config).expect("cache opens")
    }

    fn entry(name: &str, bytes: &[u8]) -> FileEntry {
        FileEntry {
            name: name.to_owned(),
            size: bytes.len() as u64,
            hash: ContentHash::of(bytes),
        }
    }

    fn list(entries: &[&FileEntry]) -> FileList {
        FileList::new(entries.iter().map(|&e| e.clone()).collect()).expect("valid list")
    }

    /// Stores `bytes` as `entry` and sets its modification time to `age` ago.
    async fn store(cache: &FileCache, entry: &FileEntry, bytes: &[u8], age: Duration) -> PathBuf {
        let mut incoming = cache.begin(entry).expect("begin");
        incoming.write(bytes).await.expect("write");
        let path = incoming.finish().await.expect("verified");
        let file = fs::File::options().write(true).open(&path).expect("open");
        file.set_modified(SystemTime::now() - age)
            .expect("set mtime");
        path
    }

    /// Every file under the cache directory, temporary ones included.
    fn files_in(cache: &FileCache) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for dir in fs::read_dir(cache.dir()).expect("read cache") {
            for file in fs::read_dir(dir.expect("entry").path()).expect("read hash dir") {
                files.push(file.expect("entry").path());
            }
        }
        files
    }

    #[tokio::test]
    async fn verified_file_lands_at_its_hash_and_name() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cache = cache(dir.path(), 1024);
        let entry = entry("notes.txt", b"hello world");

        let mut incoming = cache.begin(&entry).expect("begin");
        incoming.write(b"hello ").await.expect("write");
        incoming.write(b"world").await.expect("write");
        let path = incoming.finish().await.expect("verified");

        assert_eq!(
            path,
            cache.dir().join(entry.hash.to_string()).join("notes.txt")
        );
        assert_eq!(fs::read(&path).expect("read"), b"hello world");
        assert_eq!(files_in(&cache), [path]);
        assert!(cache.contains(&entry).expect("check"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cache_is_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("temp dir");
        let cache = cache(dir.path(), 1024);
        let path = store(&cache, &entry("a", b"a"), b"a", Duration::ZERO).await;
        let mode = |p: &Path| fs::metadata(p).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode(cache.dir()), 0o700);
        assert_eq!(mode(path.parent().expect("hash dir")), 0o700);
        assert_eq!(mode(&path), 0o600);
    }

    #[tokio::test]
    async fn bad_transfers_leave_nothing_behind() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cache = cache(dir.path(), 1024);
        let entry = entry("a.bin", b"0123456789");

        let mut wrong = cache.begin(&entry).expect("begin");
        wrong.write(b"9876543210").await.expect("write");
        assert!(matches!(
            wrong.finish().await,
            Err(StoreError::HashMismatch)
        ));

        let mut short = cache.begin(&entry).expect("begin");
        short.write(b"01234").await.expect("write");
        assert!(matches!(
            short.finish().await,
            Err(StoreError::Truncated {
                got: 5,
                announced: 10
            })
        ));

        let mut long = cache.begin(&entry).expect("begin");
        long.write(b"0123456789").await.expect("write");
        assert!(matches!(
            long.write(b"!").await,
            Err(StoreError::TooManyBytes { announced: 10 })
        ));
        drop(long);

        let dropped = cache.begin(&entry).expect("begin");
        assert_eq!(
            files_in(&cache).len(),
            1,
            "the temporary file exists meanwhile"
        );
        drop(dropped);

        assert_eq!(files_in(&cache), Vec::<PathBuf>::new());
        assert!(!cache.contains(&entry).expect("check"));
    }

    #[tokio::test]
    async fn file_changed_in_the_cache_is_not_a_hit() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cache = cache(dir.path(), 1024);
        let entry = entry("a.txt", b"original");
        let path = store(&cache, &entry, b"original", Duration::ZERO).await;

        fs::write(&path, b"edited!!").expect("edit in place");
        assert!(!cache.contains(&entry).expect("check"));
        assert!(!path.exists(), "the changed file is deleted");
    }

    #[tokio::test]
    async fn opening_deletes_leftover_temporary_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cache = cache(dir.path(), 1024);
        let entry = entry("a.txt", b"abc");
        let kept = store(&cache, &entry, b"abc", Duration::ZERO).await;
        let leftover = kept.with_file_name(".clipbroad-x1y2z3.part");
        fs::write(&leftover, b"ab").expect("write leftover");
        // Not ours: never touched.
        let foreign = cache.dir().join("notes");
        fs::create_dir(&foreign).expect("mkdir");
        fs::write(foreign.join(".clipbroad-x.part"), b"x").expect("write");

        let cache = self::cache(dir.path(), 1024);
        assert_eq!(files_in(&cache).len(), 2, "{:?}", files_in(&cache));
        assert!(kept.exists());
        assert!(!leftover.exists());
    }

    #[tokio::test]
    async fn clean_deletes_expired_then_oldest_but_keeps_the_current_clip() {
        let dir = tempfile::tempdir().expect("temp dir");
        // Room for 25 bytes.
        let cache = cache(dir.path(), 25);
        let expired = entry("expired", &[1; 5]);
        let old = entry("old", &[2; 10]);
        let newer = entry("newer", &[3; 10]);
        let current = entry("current", &[4; 10]);
        let paths = [
            store(&cache, &expired, &[1; 5], 8 * DAY).await,
            store(&cache, &old, &[2; 10], 3 * DAY).await,
            store(&cache, &newer, &[3; 10], DAY).await,
            // The oldest of all, but on the clipboard.
            store(&cache, &current, &[4; 10], 30 * DAY).await,
        ];

        let cleaned = cache.clean(&list(&[&current])).expect("clean");

        // 35 bytes: the expired file goes for its age, `old` to get within 25 bytes.
        assert_eq!(
            cleaned,
            Cleaned {
                files: 2,
                bytes: 15
            }
        );
        let exists: Vec<bool> = paths.iter().map(|p| p.exists()).collect();
        assert_eq!(exists, [false, false, true, true]);
        assert!(
            !paths[0].parent().expect("dir").exists(),
            "empty hash dir removed"
        );
        assert_eq!(
            cache.clean(&list(&[&current])).expect("clean"),
            Cleaned::default()
        );
    }

    #[tokio::test]
    async fn clean_leaves_running_transfers_alone() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cache = cache(dir.path(), 10);
        let current = entry("current", &[4; 10]);
        store(&cache, &current, &[4; 10], Duration::ZERO).await;
        let mut running = cache.begin(&entry("next", &[5; 10])).expect("begin");
        running.write(&[5; 10]).await.expect("write");

        assert_eq!(
            cache.clean(&list(&[&current])).expect("clean"),
            Cleaned::default()
        );
        running.finish().await.expect("still finishes");
    }
}
