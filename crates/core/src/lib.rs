//! Platform-independent core: clipboard content model, wire protocol and sync rules.

mod content;
mod hash;
pub mod protocol;
pub mod sync;

pub use content::ClipContent;
pub use hash::ContentHash;
