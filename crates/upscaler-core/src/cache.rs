//! Content-addressed keys for previews and export jobs.

use sha2::{Digest, Sha256};

/// A short hex digest of `parts`.
pub fn key(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0u8]);
    }
    h.finalize()
        .iter()
        .take(12)
        .map(|b| format!("{b:02x}"))
        .collect()
}
