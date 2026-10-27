//! Platform-independent core: clipboard content model, wire protocol, sync rules and
//! sensitive-content markers.

mod content;
mod hash;
pub mod protocol;
pub mod sensitive;
pub mod sync;

pub use content::ClipContent;
pub use hash::ContentHash;
