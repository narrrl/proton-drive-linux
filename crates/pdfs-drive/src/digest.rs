//! The SHA-1 digests Drive keys content on.

use std::io::Read;

use pdfs_core::{CoreError, CoreResult};
use sha1::{Digest, Sha1};

/// The lowercase-hex SHA-1 of everything `reader` yields — the digest Proton's
/// photo duplicate detection is keyed on.
pub fn sha1_of(reader: &mut (dyn Read + Send)) -> CoreResult<String> {
    let mut hasher = Sha1::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|e| CoreError::invalid(format!("read failed: {e}")))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_lower(&hasher.finalize()))
}

/// `bytes` as lowercase hex.
pub fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_of_an_empty_reader_is_the_well_known_digest() {
        let mut empty: &[u8] = &[];
        assert_eq!(
            sha1_of(&mut empty).unwrap(),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
    }
}
