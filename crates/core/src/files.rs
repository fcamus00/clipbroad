//! Files on the clipboard: the list a clip announces, and the rules every file name must follow.
//!
//! A files clip carries no file bytes. It announces a [`FileList`] (name, size and BLAKE3 hash of
//! each file), and the receiver fetches the bytes separately and checks them against the list.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ContentHash;

/// Longest accepted file name, in bytes (the limit of ext4, APFS and NTFS).
const MAX_NAME_LEN: usize = 255;

/// BLAKE3 key-derivation context of [`FileList::hash`]. Part of the protocol: do not change it.
const LIST_HASH_CONTEXT: &str = "clipbroad 2026-10 file list";

/// One announced file. Comes from the wire, so its fields are not trusted until it is part of a
/// [`FileList`].
///
/// `Debug` prints the name's length, never the name, so logging a clip cannot leak what was copied.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FileEntry {
    /// Plain file name, never a path (see [`validate_file_name`]).
    pub name: String,
    /// Size in bytes.
    pub size: u64,
    /// BLAKE3 of the file's bytes.
    pub hash: ContentHash,
}

impl fmt::Debug for FileEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileEntry")
            .field("name_len", &self.name.len())
            .field("size", &self.size)
            .field("hash", &self.hash)
            .finish()
    }
}

/// A checked, non-empty list of files, sorted by name.
///
/// Built only by [`FileList::new`], so every name is valid and unique and the sizes add up
/// without overflow. Sorting makes the list, and so its [`hash`](Self::hash), independent of the
/// order the OS reports the files in.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct FileList {
    entries: Vec<FileEntry>,
    total_size: u64,
}

impl FileList {
    /// Checks `entries` and sorts them by name.
    ///
    /// # Errors
    ///
    /// [`FileListError`] if the list is empty, a name is invalid or repeated, or the sizes add up
    /// to more than `u64::MAX`.
    pub fn new(mut entries: Vec<FileEntry>) -> Result<Self, FileListError> {
        if entries.is_empty() {
            return Err(FileListError::Empty);
        }
        entries.sort();
        let mut total_size = 0_u64;
        for (i, entry) in entries.iter().enumerate() {
            validate_file_name(&entry.name).map_err(FileListError::InvalidName)?;
            if i > 0 && entries[i - 1].name == entry.name {
                return Err(FileListError::DuplicateName);
            }
            total_size = total_size
                .checked_add(entry.size)
                .ok_or(FileListError::TooLarge)?;
        }
        Ok(Self {
            entries,
            total_size,
        })
    }

    /// The files, sorted by name.
    #[must_use]
    pub fn entries(&self) -> &[FileEntry] {
        &self.entries
    }

    /// Sum of the file sizes, in bytes.
    #[must_use]
    pub const fn total_size(&self) -> u64 {
        self.total_size
    }

    /// Gives back the files, sorted by name.
    #[must_use]
    pub fn into_entries(self) -> Vec<FileEntry> {
        self.entries
    }

    /// The hash of the list, used as the clip hash for anti-echo and on the wire.
    ///
    /// BLAKE3 in key-derivation mode (context `"clipbroad 2026-10 file list"`) over each entry in
    /// name order: the name's length in bytes (u64 little-endian), the name's UTF-8 bytes, the
    /// size (u64 little-endian) and the file's 32-byte hash. It covers what the files are, not
    /// where they live, so the same files give the same hash on every machine. The derived key
    /// keeps it apart from the plain BLAKE3 of a text or image. Part of the protocol: do not
    /// change it.
    #[must_use]
    pub fn hash(&self) -> ContentHash {
        let mut hasher = blake3::Hasher::new_derive_key(LIST_HASH_CONTEXT);
        for entry in &self.entries {
            hasher.update(&(entry.name.len() as u64).to_le_bytes());
            hasher.update(entry.name.as_bytes());
            hasher.update(&entry.size.to_le_bytes());
            hasher.update(entry.hash.as_bytes());
        }
        ContentHash::from_bytes(*hasher.finalize().as_bytes())
    }
}

/// `Debug` prints the count, total size and hash, never the names.
impl fmt::Debug for FileList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileList")
            .field("count", &self.entries.len())
            .field("total_size", &self.total_size)
            .field("hash", &self.hash())
            .finish()
    }
}

/// Why [`FileList::new`] rejected a list. Never includes a name: it comes from another machine
/// and could carry control characters into the logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FileListError {
    #[error("file list is empty")]
    Empty,
    #[error("invalid file name: {0}")]
    InvalidName(InvalidFileName),
    #[error("two files have the same name")]
    DuplicateName,
    #[error("file sizes add up to more than 2^64 bytes")]
    TooLarge,
}

/// Why a file name is not acceptable (see [`validate_file_name`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum InvalidFileName {
    #[error("empty")]
    Empty,
    #[error("longer than {MAX_NAME_LEN} bytes")]
    TooLong,
    #[error("contains a path separator or a character Windows does not allow")]
    ForbiddenChar,
    #[error("contains a control character")]
    ControlChar,
    #[error("ends with a dot or a space")]
    TrailingDotOrSpace,
    #[error("reserved device name on Windows")]
    ReservedOnWindows,
}

/// Checks that `name` is a plain file name that every supported OS can create as-is, so a
/// received name can never point outside the directory it is written to.
///
/// Rejected: an empty name, more than 255 bytes, `/`, `\`, and the other characters Windows does
/// not allow (`:` `*` `?` `"` `<` `>` `|`), control characters (including NUL), a trailing dot or
/// space (this also rejects `.` and `..`), and Windows device names such as `CON` or `com1.txt`.
/// The same rules apply on every OS, so a name one machine accepts, all accept.
///
/// ```
/// use clipbroad_core::validate_file_name;
///
/// assert!(validate_file_name("report.pdf").is_ok());
/// assert!(validate_file_name("../secret").is_err());
/// ```
///
/// # Errors
///
/// [`InvalidFileName`] with the first rule `name` breaks.
pub fn validate_file_name(name: &str) -> Result<(), InvalidFileName> {
    if name.is_empty() {
        return Err(InvalidFileName::Empty);
    }
    if name.len() > MAX_NAME_LEN {
        return Err(InvalidFileName::TooLong);
    }
    if name.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|']) {
        return Err(InvalidFileName::ForbiddenChar);
    }
    if name.chars().any(char::is_control) {
        return Err(InvalidFileName::ControlChar);
    }
    if name.ends_with(['.', ' ']) {
        return Err(InvalidFileName::TrailingDotOrSpace);
    }
    if is_windows_device(name) {
        return Err(InvalidFileName::ReservedOnWindows);
    }
    Ok(())
}

/// Whether Windows treats `name` as a device: `CON`, `PRN`, `AUX`, `NUL`, `COM0`–`COM9` and
/// `LPT0`–`LPT9` (also with a superscript ¹²³ digit), in any case, alone or before an extension.
fn is_windows_device(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    let Some(rest) = upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"))
    else {
        return false;
    };
    let mut chars = rest.chars();
    matches!(
        (chars.next(), chars.next()),
        (Some('0'..='9' | '¹' | '²' | '³'), None)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, size: u64) -> FileEntry {
        FileEntry {
            name: name.to_owned(),
            size,
            hash: ContentHash::of(name.as_bytes()),
        }
    }

    #[test]
    fn plain_names_are_valid() {
        for name in [
            "a",
            "report.pdf",
            ".bashrc",
            "with space.txt",
            "naïve 🦀.md",
            "CONSOLE.txt",
            "com10",
            "x".repeat(255).as_str(),
        ] {
            assert_eq!(validate_file_name(name), Ok(()), "{name}");
        }
    }

    #[test]
    fn hostile_names_are_rejected() {
        use InvalidFileName as E;
        let cases = [
            ("", E::Empty),
            (&"x".repeat(256), E::TooLong),
            ("../etc/passwd", E::ForbiddenChar),
            ("/etc/passwd", E::ForbiddenChar),
            ("..\\windows", E::ForbiddenChar),
            ("C:evil", E::ForbiddenChar),
            ("file.txt:stream", E::ForbiddenChar),
            ("what?.txt", E::ForbiddenChar),
            ("a\0b", E::ControlChar),
            ("line\nbreak", E::ControlChar),
            ("esc\u{1b}[31m", E::ControlChar),
            (".", E::TrailingDotOrSpace),
            ("..", E::TrailingDotOrSpace),
            ("name.", E::TrailingDotOrSpace),
            ("name ", E::TrailingDotOrSpace),
            ("CON", E::ReservedOnWindows),
            ("nul.txt", E::ReservedOnWindows),
            ("Com1.tar.gz", E::ReservedOnWindows),
            ("lpt9", E::ReservedOnWindows),
            ("COM¹", E::ReservedOnWindows),
            ("aux .txt", E::ReservedOnWindows),
        ];
        for (name, expected) in cases {
            assert_eq!(validate_file_name(name), Err(expected), "{name:?}");
        }
    }

    #[test]
    fn list_is_sorted_and_hash_ignores_the_input_order() {
        let a = FileList::new(vec![entry("b.txt", 2), entry("a.txt", 1)]).expect("valid");
        let b = FileList::new(vec![entry("a.txt", 1), entry("b.txt", 2)]).expect("valid");
        assert_eq!(a, b);
        assert_eq!(a.hash(), b.hash());
        let names: Vec<_> = a.entries().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["a.txt", "b.txt"]);
        assert_eq!(a.total_size(), 3);
    }

    #[test]
    fn hash_covers_name_size_and_content() {
        let base = FileList::new(vec![entry("a", 1)]).expect("valid").hash();
        let renamed = FileList::new(vec![FileEntry {
            name: "b".to_owned(),
            ..entry("a", 1)
        }]);
        let resized = FileList::new(vec![FileEntry {
            size: 2,
            ..entry("a", 1)
        }]);
        let changed = FileList::new(vec![FileEntry {
            hash: ContentHash::of(b"other"),
            ..entry("a", 1)
        }]);
        for other in [renamed, resized, changed] {
            assert_ne!(other.expect("valid").hash(), base);
        }
    }

    #[test]
    fn hash_is_stable() {
        // Pinned: every version and machine must compute the same hash for the same files.
        let list = FileList::new(vec![entry("a.txt", 1)]).expect("valid");
        assert_eq!(
            list.hash().to_string(),
            "2b5e767624460b0e44f33d53573dfe3405553ba4caa95cbf3fc057c0c9a7be71"
        );
    }

    #[test]
    fn bad_lists_are_rejected() {
        assert_eq!(FileList::new(vec![]), Err(FileListError::Empty));
        assert_eq!(
            FileList::new(vec![entry("a", 1), entry("../a", 1)]),
            Err(FileListError::InvalidName(InvalidFileName::ForbiddenChar))
        );
        assert_eq!(
            FileList::new(vec![entry("a", 1), entry("a", 2)]),
            Err(FileListError::DuplicateName)
        );
        assert_eq!(
            FileList::new(vec![entry("a", u64::MAX), entry("b", 1)]),
            Err(FileListError::TooLarge)
        );
    }

    #[test]
    fn debug_does_not_show_names() {
        let list = FileList::new(vec![entry("secret-plans.pdf", 1)]).expect("valid");
        let text = format!("{list:?} {:?}", list.entries());
        assert!(!text.contains("secret"), "{text}");
    }
}
