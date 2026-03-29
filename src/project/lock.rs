use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The auto-generated lock file (`bundle.lock`).
///
/// Never hand-edited.  Only used by `bundle server` commands.  Maps each
/// OCI image reference (with tag) to its resolved sha256 manifest digest so
/// that `bundle server pull/apply/run` are fully reproducible.
///
/// The OCI image manifest produced by `bundle build` already serves as a
/// lock file for the build itself — every layer is content-addressed — so
/// `bundle.lock` only needs to track the server-side tag → digest mapping.
///
/// ```toml
/// [bundles]
/// "ghcr.io/someauthor/essentials:v2.20.1" = "sha256:abc123..."
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LockFile {
    /// Maps `"registry/repo:tag"` → `"sha256:<hex>"` manifest digest.
    #[serde(default)]
    pub bundles: HashMap<String, String>,
}

pub const LOCK_FILE_NAME: &str = "bundle.lock";

impl LockFile {
    /// Load `bundle.lock` from the current working directory.
    /// Returns a default (empty) lock file if none exists yet.
    pub fn load() -> Result<Self> {
        Self::load_from(Path::new(LOCK_FILE_NAME))
    }

    /// Load `bundle.lock` from an explicit path.
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading lock file {}", path.display()))?;
        let lock: Self = toml::from_str(&content)
            .with_context(|| format!("parsing lock file {}", path.display()))?;
        Ok(lock)
    }

    /// Persist this lock file to `bundle.lock` in the current working directory.
    pub fn save(&self) -> Result<()> {
        self.save_to(Path::new(LOCK_FILE_NAME))
    }

    /// Persist this lock file to an explicit path.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let mut doc = String::new();
        doc.push_str("# bundle.lock — auto-generated, do not edit by hand.\n");
        doc.push_str("# Commit this file alongside bundle.toml for reproducible builds.\n\n");

        // [bundles] section — sorted for deterministic diffs.
        doc.push_str("[bundles]\n");
        let mut sorted_bundles: Vec<(&String, &String)> = self.bundles.iter().collect();
        sorted_bundles.sort_by_key(|(k, _)| k.as_str());
        for (image_ref, digest) in sorted_bundles {
            doc.push_str(&format!(
                "{} = {}\n",
                toml_quote_key(image_ref),
                toml::Value::String(digest.clone())
            ));
        }

        std::fs::write(path, &doc)
            .with_context(|| format!("writing lock file {}", path.display()))?;
        Ok(())
    }

    /// Return the pinned digest for the given image reference, if present.
    pub fn get_digest(&self, image_ref: &str) -> Option<&str> {
        self.bundles.get(image_ref).map(String::as_str)
    }

    /// Record a resolved digest for an image reference.
    pub fn set_digest(&mut self, image_ref: impl Into<String>, digest: impl Into<String>) {
        self.bundles.insert(image_ref.into(), digest.into());
    }

    #[allow(dead_code)]
    pub fn replace_bundles(&mut self, bundles: HashMap<String, String>) {
        self.bundles = bundles;
    }
}

/// Produce a TOML bare key or quoted key as required.
/// Image references contain `/`, `:`, `.` etc. which are not valid in bare keys,
/// so we always quote them.
fn toml_quote_key(s: &str) -> String {
    // Use TOML literal string quoting (single quotes) when possible,
    // otherwise fall back to basic string (double quotes with escaping).
    if s.contains('\'') {
        // Need basic string – escape backslashes and double-quotes.
        let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{}\"", escaped)
    } else {
        format!("'{}'", s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn round_trip_empty() {
        let lock = LockFile::default();
        let tmp = NamedTempFile::new().unwrap();
        lock.save_to(tmp.path()).unwrap();
        let loaded = LockFile::load_from(tmp.path()).unwrap();
        assert!(loaded.bundles.is_empty());
    }

    #[test]
    fn round_trip_with_entries() {
        let mut lock = LockFile::default();
        lock.set_digest(
            "ghcr.io/someauthor/essentials:v2.20.1",
            "sha256:abc123deadbeef",
        );
        lock.set_digest("ghcr.io/me/my-server-bundle:latest", "sha256:cafebabe0000");

        let tmp = NamedTempFile::new().unwrap();
        lock.save_to(tmp.path()).unwrap();

        let loaded = LockFile::load_from(tmp.path()).unwrap();
        assert_eq!(
            loaded.get_digest("ghcr.io/someauthor/essentials:v2.20.1"),
            Some("sha256:abc123deadbeef")
        );
        assert_eq!(
            loaded.get_digest("ghcr.io/me/my-server-bundle:latest"),
            Some("sha256:cafebabe0000")
        );
    }

    #[test]
    fn missing_file_returns_default() {
        let loaded = LockFile::load_from(Path::new("/tmp/does-not-exist-bundle.lock")).unwrap();
        assert!(loaded.bundles.is_empty());
    }

    #[test]
    fn key_quoting_handles_single_quotes() {
        let k = "some'key";
        let quoted = toml_quote_key(k);
        assert!(quoted.starts_with('"'));
    }
}
