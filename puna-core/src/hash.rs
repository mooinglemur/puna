//! Hex and SHA-256, in one place.
//!
//! Small enough to be rewritten inline anywhere it is needed, which is exactly why it is here: a
//! generation directory's name, a room's `spec_hash` and the ingest's content address are all "the
//! digest, in lowercase hex", and three implementations of that would be three chances for one of
//! them to be uppercase or to drop a leading zero.

use sha2::{Digest, Sha256};

/// Lowercase, two characters per byte, no separators.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            // Infallible: writing to a String cannot fail, and `?` here would put a Result in the
            // signature of a function that has nothing to report.
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// SHA-256 of `input`, as 64 hex characters.
pub fn sha256_hex(input: &[u8]) -> String {
    hex(&Sha256::digest(input))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_keeps_leading_zeros_and_stays_lowercase() {
        assert_eq!(hex(&[0x00, 0x0a, 0xff]), "000aff");
        assert_eq!(hex(&[]), "");
    }

    /// The published vector, so a dependency swap cannot quietly change what a content address
    /// means: every generation directory on disk is named by this function.
    #[test]
    fn sha256_matches_the_known_answer_for_abc() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(sha256_hex(b"").len(), 64);
    }
}
