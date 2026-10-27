//! Platform-independent core: clipboard content model, wire protocol and sync rules.

mod content;
mod hash;

pub use content::ClipContent;
pub use hash::ContentHash;

/// Wire protocol version. Bump on any incompatible message change.
pub const PROTOCOL_VERSION: u16 = 1;
