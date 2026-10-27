//! Platform-independent core: clipboard content model, wire protocol, sync rules, file name rules
//! and sensitive-content markers.

mod content;
mod files;
mod hash;
pub mod protocol;
pub mod sensitive;
pub mod sync;

pub use content::{ClipContent, NotPng, Png};
pub use files::{FileEntry, FileList, FileListError, InvalidFileName, validate_file_name};
pub use hash::ContentHash;
