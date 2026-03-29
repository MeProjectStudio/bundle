//! OCI manifest, config, and descriptor types used throughout mcpm.
//!
//! Both image construction and registry transport are handled through the
//! `oci-spec` and `oci-client` crates respectively. `oci-client` already
//! depends on `oci-spec`, so they share the same underlying types.
//!
//! ## Type ownership
//!
//! | Concern                  | Crate      | Types used                                    |
//! |--------------------------|------------|-----------------------------------------------|
//! | Image config             | `oci-spec` | `ImageConfiguration`, `RootFs`, `Arch`, `Os`  |
//! | Image manifest           | `oci-spec` | `ImageManifest`, `Descriptor`, `MediaType`    |
//! | Registry push / pull     | `oci-client` | `Client`, `Reference`, `RegistryAuth`       |

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

//
// All OCI image-construction types come from `oci-spec`. Callers import them
// via `crate::registry::types::*` so the crate boundary is a single import.

pub use oci_spec::image::{
    // Config
    Arch,
    // Manifest + descriptor
    Descriptor,
    ImageConfiguration,
    ImageConfigurationBuilder,
    ImageManifest,
    ImageManifestBuilder,
    MediaType,
    Os,
    RootFsBuilder,
    SCHEMA_VERSION,
};

/// Construct a minimal spec-compliant OCI image config for an mcpm bundle.
///
/// `diff_ids` are the sha256 digests of the **uncompressed** layer tarballs,
/// in the same order as the manifest's `layers` array.
pub fn build_image_config(diff_ids: Vec<String>) -> Result<ImageConfiguration> {
    let rootfs = RootFsBuilder::default()
        .typ("layers".to_owned())
        .diff_ids(diff_ids)
        .build()
        .context("building OCI RootFs")?;

    ImageConfigurationBuilder::default()
        .architecture(Arch::Amd64)
        .os(Os::Linux)
        .rootfs(rootfs)
        .build()
        .context("building OCI ImageConfiguration")
}

/// Serialise an [`ImageConfiguration`] to canonical JSON bytes.
///
/// The result is suitable for hashing (to produce the config digest) and for
/// passing to [`oci_client::client::Config::oci_v1`].
pub fn image_config_to_bytes(config: &ImageConfiguration) -> Result<Vec<u8>> {
    config
        .to_string()
        .map(|s| s.into_bytes())
        .context("serialising OCI image config to JSON")
}


/// A fully assembled OCI image that has been built locally by `bundle build`.
///
/// Contains everything needed for `bundle push`:
/// - The `ImageManifest` (spec-compliant, with layer descriptors and
///   `bundle.managed-keys` annotations already baked in).
/// - The raw bytes of the image config blob.
/// - All **new** layer blobs introduced by this build (keyed by their
///   `"sha256:<hex>"` digest).  Blobs inherited from base images are **not**
///   stored here — they are assumed to already exist in the target registry.
#[derive(Debug, Clone)]
pub struct LocalImage {
    /// The fully-formed, spec-compliant OCI image manifest.
    pub manifest: ImageManifest,

    /// The raw JSON bytes of the image config blob (so callers can push it).
    pub config_data: Vec<u8>,

    /// New layer blobs introduced by this build.
    ///
    /// Key: `"sha256:<hex>"` digest of the compressed tar blob.
    /// Value: the compressed (gzip) tar bytes.
    pub new_blobs: HashMap<String, Vec<u8>>,
}

impl LocalImage {
    /// Return a sorted list of all layer digests referenced by the manifest.
    #[allow(dead_code)]
    pub fn all_layer_digests(&self) -> Vec<String> {
        self.manifest
            .layers()
            .iter()
            .map(|d| d.digest().to_string())
            .collect()
    }

    /// Return `true` if a layer blob with the given digest is stored locally
    /// (i.e., it was produced by this build and not inherited from a base).
    pub fn has_blob(&self, digest: &str) -> bool {
        self.new_blobs.contains_key(digest)
    }

    /// Retrieve the raw bytes of a locally-stored blob by digest.
    pub fn get_blob(&self, digest: &str) -> Option<&[u8]> {
        self.new_blobs.get(digest).map(Vec::as_slice)
    }

    /// The digest of the config blob (`"sha256:<hex>"`).
    #[allow(dead_code)]
    pub fn config_digest(&self) -> String {
        self.manifest.config().digest().to_string()
    }
}


/// Filesystem cache for OCI blobs and manifests.
///
/// Layout under `base_dir` (default: `~/.cache/mcpm/`):
/// ```text
/// blobs/
///   sha256/
///     <hex>          ← raw blob bytes (layers, image configs, manifests)
/// manifests/
///   <escaped-ref>    ← "<digest>\n<manifest-json>"
/// built/
///   manifest.json    ← manifest of the most recent `bundle build`
///   push_blobs.json  ← list of blob digests that need uploading on push
/// ```
#[derive(Debug, Clone)]
pub struct LocalCache {
    pub base_dir: PathBuf,
}

impl LocalCache {
    /// Open (or create) the default cache directory (`~/.cache/mcpm/`).
    pub fn open() -> Result<Self> {
        let base = dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from(".bundle-cache"))
            .join("bundle");
        std::fs::create_dir_all(&base)
            .with_context(|| format!("creating bundle cache directory: {}", base.display()))?;
        Ok(LocalCache { base_dir: base })
    }

    /// Open a cache rooted at `dir` (useful for tests / `--cache-dir` flag).
    #[allow(dead_code)]
    pub fn open_at(dir: impl Into<PathBuf>) -> Result<Self> {
        let base = dir.into();
        std::fs::create_dir_all(&base)
            .with_context(|| format!("creating bundle cache directory: {}", base.display()))?;
        Ok(LocalCache { base_dir: base })
    }


    fn blob_path(&self, digest: &str) -> PathBuf {
        // digest is "sha256:<hex>" — strip the prefix for the filename.
        let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
        self.base_dir.join("blobs").join("sha256").join(hex)
    }

    /// Write `data` into the blob store, computing its sha256 digest.
    /// Returns the digest string `"sha256:<hex>"`.
    pub fn store_blob(&self, data: &[u8]) -> Result<String> {
        use sha2::{Digest as ShaDigest, Sha256};
        let hash = Sha256::digest(data);
        let hex = hex::encode(hash);
        let digest = format!("sha256:{}", hex);

        let path = self.blob_path(&digest);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating blob directory: {}", parent.display()))?;
        }
        // Write only if not already present (blobs are content-addressed).
        if !path.exists() {
            std::fs::write(&path, data).with_context(|| format!("writing blob {}", digest))?;
        }
        Ok(digest)
    }

    /// Read a blob by digest.  Returns an error if the blob is not cached.
    pub fn load_blob(&self, digest: &str) -> Result<Vec<u8>> {
        let path = self.blob_path(digest);
        std::fs::read(&path).with_context(|| {
            format!(
                "reading blob {} from cache (path: {})",
                digest,
                path.display()
            )
        })
    }

    /// Return `true` if the blob is already in the local cache.
    pub fn has_blob(&self, digest: &str) -> bool {
        self.blob_path(digest).exists()
    }


    fn manifest_path(&self, image_ref: &str) -> PathBuf {
        // Escape characters that are invalid in filenames.
        let escaped = image_ref
            .replace('/', "_")
            .replace(':', "__")
            .replace('@', "@@");
        self.base_dir
            .join("manifests")
            .join(format!("{}.json", escaped))
    }

    /// Store a manifest together with its content digest.
    ///
    /// The file on disk is stored as `<digest>\n<json-bytes>` so both pieces
    /// survive a single atomic write.  Pass the raw serialised manifest bytes
    /// (from [`ImageManifest::to_string`]).
    pub fn store_manifest(
        &self,
        image_ref: &str,
        manifest_json: &[u8],
        digest: &str,
    ) -> Result<()> {
        let path = self.manifest_path(image_ref);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating manifests directory: {}", parent.display()))?;
        }
        let mut content = format!("{}\n", digest).into_bytes();
        content.extend_from_slice(manifest_json);
        std::fs::write(&path, &content)
            .with_context(|| format!("writing manifest for {}", image_ref))?;
        Ok(())
    }

    /// Load a previously cached manifest.
    ///
    /// Returns `(manifest_json_bytes, digest)`.  Parse the bytes with
    /// [`ImageManifest::from_reader`].
    pub fn load_manifest(&self, image_ref: &str) -> Result<(Vec<u8>, String)> {
        let path = self.manifest_path(image_ref);
        let content = std::fs::read(&path).with_context(|| {
            format!(
                "reading cached manifest for {} (not yet pulled?)",
                image_ref
            )
        })?;

        // First line is the digest, rest is JSON.
        let newline_pos = content
            .iter()
            .position(|&b| b == b'\n')
            .with_context(|| format!("corrupt manifest cache entry for {}", image_ref))?;
        let digest = String::from_utf8(content[..newline_pos].to_vec())
            .with_context(|| format!("non-UTF8 digest in manifest cache for {}", image_ref))?;
        let json = content[newline_pos + 1..].to_vec();
        Ok((json, digest))
    }

    /// Return `true` if a manifest for `image_ref` is cached.
    pub fn has_manifest(&self, image_ref: &str) -> bool {
        self.manifest_path(image_ref).exists()
    }


    fn built_dir(&self) -> PathBuf {
        self.base_dir.join("built")
    }

    /// Persist the output of `bundle build` so that `bundle push` can retrieve it.
    pub fn store_built_image(&self, image: &LocalImage) -> Result<()> {
        let dir = self.built_dir();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating built-image dir: {}", dir.display()))?;

        // Persist all new blobs into the blob store.
        for (digest, data) in &image.new_blobs {
            let path = self.blob_path(digest);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if !path.exists() {
                std::fs::write(&path, data)
                    .with_context(|| format!("storing built blob {}", digest))?;
            }
        }

        // Store config blob.
        self.store_blob(&image.config_data)
            .context("storing image config blob")?;

        // Persist the manifest JSON via oci-spec's own serialiser.
        let manifest_json = image
            .manifest
            .to_string()
            .context("serialising built image manifest")?;
        std::fs::write(dir.join("manifest.json"), manifest_json.as_bytes())
            .context("writing built/manifest.json")?;

        // Store config data separately for fast access.
        std::fs::write(dir.join("config.json"), &image.config_data)
            .context("writing built/config.json")?;

        // Record which digests are "new" (must be pushed).
        let push_list: Vec<&str> = image.new_blobs.keys().map(String::as_str).collect();
        let push_json = serde_json::to_vec(&push_list).context("serialising push_blobs list")?;
        std::fs::write(dir.join("push_blobs.json"), &push_json)
            .context("writing built/push_blobs.json")?;

        Ok(())
    }

    /// Load the most recently built image from the cache.
    pub fn load_built_image(&self) -> Result<LocalImage> {
        let dir = self.built_dir();

        let manifest_file = dir.join("manifest.json");
        let manifest_bytes = std::fs::read(&manifest_file)
            .context("reading built/manifest.json (have you run `bundle build`?)")?;
        let manifest = ImageManifest::from_reader(manifest_bytes.as_slice())
            .context("parsing built/manifest.json")?;

        let config_data =
            std::fs::read(dir.join("config.json")).context("reading built/config.json")?;

        let push_json =
            std::fs::read(dir.join("push_blobs.json")).context("reading built/push_blobs.json")?;
        let push_digests: Vec<String> =
            serde_json::from_slice(&push_json).context("parsing built/push_blobs.json")?;

        let mut new_blobs = HashMap::new();
        for digest in push_digests {
            let data = self
                .load_blob(&digest)
                .with_context(|| format!("loading built blob {} from cache", digest))?;
            new_blobs.insert(digest, data);
        }

        Ok(LocalImage {
            manifest,
            config_data,
            new_blobs,
        })
    }

    /// The path used for blobs — exposed for tests.
    #[allow(dead_code)]
    pub fn blobs_dir(&self) -> PathBuf {
        self.base_dir.join("blobs").join("sha256")
    }
}


/// A compact on-disk record that associates a logical bundle name with the
/// digest of its manifest.  Written by `bundle pull` and read by `bundle apply`.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PulledBundle {
    /// The original image reference (with tag), e.g. `"ghcr.io/author/pkg:v1"`.
    pub image_ref: String,
    /// The manifest digest, e.g. `"sha256:abc123..."`.
    pub digest: String,
}


#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_cache() -> (TempDir, LocalCache) {
        let dir = TempDir::new().unwrap();
        let cache = LocalCache::open_at(dir.path()).unwrap();
        (dir, cache)
    }

    #[test]
    fn blob_round_trip() {
        let (_dir, cache) = temp_cache();
        let data = b"hello, mcpm blob!";
        let digest = cache.store_blob(data).unwrap();
        assert!(digest.starts_with("sha256:"));
        assert!(cache.has_blob(&digest));
        let loaded = cache.load_blob(&digest).unwrap();
        assert_eq!(loaded, data);
    }

    #[test]
    fn blob_idempotent() {
        let (_dir, cache) = temp_cache();
        let data = b"same data";
        let d1 = cache.store_blob(data).unwrap();
        let d2 = cache.store_blob(data).unwrap();
        assert_eq!(d1, d2);
    }

    #[test]
    fn manifest_round_trip() {
        let (_dir, cache) = temp_cache();
        let json = br#"{"schemaVersion":2}"#;
        let digest = "sha256:abcdef";
        cache
            .store_manifest("ghcr.io/test/img:v1", json, digest)
            .unwrap();
        assert!(cache.has_manifest("ghcr.io/test/img:v1"));
        let (loaded_json, loaded_digest) = cache.load_manifest("ghcr.io/test/img:v1").unwrap();
        assert_eq!(loaded_json, json);
        assert_eq!(loaded_digest, digest);
    }

    #[test]
    fn missing_blob_returns_error() {
        let (_dir, cache) = temp_cache();
        assert!(cache.load_blob("sha256:0000").is_err());
    }

    #[test]
    fn image_config_serialises() {
        let cfg = build_image_config(vec!["sha256:aaa".into(), "sha256:bbb".into()]).unwrap();
        let bytes = image_config_to_bytes(&cfg).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["os"], "linux");
        assert_eq!(v["architecture"], "amd64");
        assert_eq!(v["rootfs"]["type"], "layers");
        assert_eq!(v["rootfs"]["diff_ids"][0], "sha256:aaa");
    }

    #[test]
    fn image_manifest_round_trips_via_oci_spec() {
        use crate::util::digest::sha256_digest;

        let config_data = b"{}";
        let config_digest = sha256_digest(config_data);

        use std::str::FromStr as _;
        let hex = config_digest
            .strip_prefix("sha256:")
            .unwrap_or(&config_digest);
        let config_sha256 = oci_spec::image::Sha256Digest::from_str(hex).expect("valid sha256");
        let config_desc = Descriptor::new(
            MediaType::ImageConfig,
            config_data.len() as u64,
            config_sha256,
        );
        let manifest = ImageManifestBuilder::default()
            .schema_version(SCHEMA_VERSION)
            .media_type(MediaType::ImageManifest)
            .config(config_desc)
            .layers(vec![])
            .build()
            .expect("build manifest");

        let json = manifest.to_string().expect("serialise manifest");
        let parsed = ImageManifest::from_reader(json.as_bytes()).expect("parse manifest");
        // oci-spec Digest.to_string() includes the "sha256:" prefix.
        assert_eq!(parsed.config().digest().to_string(), config_digest);
    }
}
