use std::fmt::Write as FmtWrite;
use std::io::{self, Read};
use std::path::Path;

use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

/// Compute the sha256 digest of raw bytes, returning a string like `"sha256:abcdef..."`.
pub fn sha256_digest(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    let mut s = String::with_capacity(7 + 64);
    s.push_str("sha256:");
    for byte in hash.iter() {
        write!(s, "{:02x}", byte).unwrap();
    }
    s
}

/// Compute the sha256 digest of a file on disk, returning `"sha256:abcdef..."`.
#[allow(dead_code)]
pub fn sha256_digest_file(path: &Path) -> Result<String> {
    let data = std::fs::read(path)
        .with_context(|| format!("reading file for digest: {}", path.display()))?;
    Ok(sha256_digest(&data))
}

/// Compute the sha256 digest of the *uncompressed* content of a gzip-compressed blob.
/// This is used for OCI `diff_id` values, which are sha256 of the uncompressed tar.
#[allow(dead_code)]
pub fn sha256_diff_id(compressed_data: &[u8]) -> Result<String> {
    let mut decoder = GzDecoder::new(compressed_data);
    let mut buf = Vec::new();
    decoder
        .read_to_end(&mut buf)
        .context("decompressing gzip data for diff_id calculation")?;
    Ok(sha256_digest(&buf))
}

/// Verify that `data` matches `expected_digest` (format: `"sha256:abcdef..."`).
/// Returns `Ok(())` on success, or an error describing the mismatch.
#[allow(dead_code)]
pub fn verify_digest(data: &[u8], expected: &str) -> Result<()> {
    let actual = sha256_digest(data);
    if actual != expected {
        bail!("digest mismatch: expected {}, got {}", expected, actual);
    }
    Ok(())
}

/// Parse a digest string like `"sha256:abcdef..."` into `(algorithm, hex)`.
/// Returns an error if the format is unrecognised.
#[allow(dead_code)]
pub fn parse_digest(digest: &str) -> Result<(&str, &str)> {
    let (algo, hex) = digest
        .split_once(':')
        .with_context(|| format!("malformed digest (expected `algo:hex`): {}", digest))?;
    Ok((algo, hex))
}

/// Return the hex portion of a `"sha256:..."` digest, suitable for use as a filename.
#[allow(dead_code)]
pub fn digest_hex(digest: &str) -> Result<&str> {
    let (algo, hex) = parse_digest(digest)?;
    if algo != "sha256" {
        bail!(
            "unsupported digest algorithm '{}'; only sha256 is supported",
            algo
        );
    }
    Ok(hex)
}

/// A streaming SHA256 hasher that can be fed chunks incrementally.
#[allow(dead_code)]
pub struct StreamingHasher {
    inner: Sha256,
    bytes_written: u64,
}

#[allow(dead_code)]
impl StreamingHasher {
    pub fn new() -> Self {
        Self {
            inner: Sha256::new(),
            bytes_written: 0,
        }
    }
}

impl Default for StreamingHasher {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(dead_code)]
impl StreamingHasher {
    pub fn update(&mut self, data: &[u8]) {
        sha2::Digest::update(&mut self.inner, data);
        self.bytes_written += data.len() as u64;
    }

    /// Finalise and return the digest string (`"sha256:..."`) plus total byte count.
    pub fn finish(self) -> (String, u64) {
        let hash = self.inner.finalize();
        let mut s = String::with_capacity(7 + 64);
        s.push_str("sha256:");
        for byte in hash.iter() {
            write!(s, "{:02x}", byte).unwrap();
        }
        (s, self.bytes_written)
    }
}

impl io::Write for StreamingHasher {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_digest() {
        // SHA256 of empty input is known
        let d = sha256_digest(b"");
        assert_eq!(
            d,
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn verify_ok() {
        let data = b"hello world";
        let digest = sha256_digest(data);
        assert!(verify_digest(data, &digest).is_ok());
    }

    #[test]
    fn verify_mismatch() {
        let data = b"hello world";
        let result = verify_digest(data, "sha256:000000");
        assert!(result.is_err());
    }

    #[test]
    fn digest_hex_ok() {
        let d = "sha256:abcdef1234";
        assert_eq!(digest_hex(d).unwrap(), "abcdef1234");
    }

    #[test]
    fn streaming_hasher_matches_oneshot() {
        let data = b"the quick brown fox";
        let expected = sha256_digest(data);

        let mut hasher = StreamingHasher::new();
        hasher.update(&data[..5]);
        hasher.update(&data[5..10]);
        hasher.update(&data[10..]);
        let (got, size) = hasher.finish();

        assert_eq!(got, expected);
        assert_eq!(size, data.len() as u64);
    }
}
