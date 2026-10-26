use std::fmt;

/// BLAKE3 hash of clipboard content.
///
/// Used to recognise the same content across machines and across clipboard events: anti-echo
/// compares the hash of what was just applied from the network with the hash of what the local
/// clipboard reports next, and the wire protocol will carry it with every clip.
///
/// The hash covers the content bytes only (see [`ClipContent::hash`](crate::ClipContent::hash)
/// for what those bytes are per content kind), so it equals the plain BLAKE3 digest of those bytes
/// and is stable across versions and platforms.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContentHash([u8; 32]);

impl ContentHash {
    /// Hashes `bytes` with BLAKE3.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    /// Wraps a digest that was computed elsewhere (for example, received from a peer).
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw 32-byte digest.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Lowercase hex, 64 characters (the same format as the `b3sum` tool).
impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ContentHash({self})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_blake3_reference_vector() {
        // Official BLAKE3 digest of the empty input.
        assert_eq!(
            ContentHash::of(b"").to_string(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    #[test]
    fn same_input_same_hash() {
        assert_eq!(ContentHash::of(b"hello"), ContentHash::of(b"hello"));
        assert_ne!(ContentHash::of(b"hello"), ContentHash::of(b"hello "));
    }

    #[test]
    fn round_trips_through_raw_bytes() {
        let hash = ContentHash::of(b"hello");
        assert_eq!(ContentHash::from_bytes(*hash.as_bytes()), hash);
    }

    #[test]
    fn debug_shows_hex() {
        let hash = ContentHash::of(b"");
        assert_eq!(format!("{hash:?}"), format!("ContentHash({hash})"));
    }
}
