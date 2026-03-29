//! Registry client — thin wrapper around `oci_client::Client` that adds
//! credential resolution (env-vars → docker `config.json` → anonymous).
//!
//! All OCI image types (`ImageManifest`, `Descriptor`, `MediaType`) come from
//! the `oci-spec` crate.  `oci-client` is used purely for network transport:
//! push / pull blobs, push manifest as raw bytes, resolve tags, etc.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use oci_client::client::{ClientConfig, ClientProtocol};
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference, RegistryOperation};
use oci_spec::image::ImageManifest;
use serde::Deserialize;
use tokio::io::AsyncWrite;

use crate::registry::types::{Descriptor, LocalCache, LocalImage};

// ── McpmRegistryClient ────────────────────────────────────────────────────────

/// Wraps an `oci_client::Client` with mcpm-specific auth resolution and
/// convenience methods for the operations mcpm needs.
#[derive(Clone)]
pub struct McpmRegistryClient {
    inner: Client,
}

impl McpmRegistryClient {
    /// Construct a new client using HTTPS with rustls TLS.
    pub fn new() -> Self {
        let cfg = ClientConfig {
            protocol: ClientProtocol::Https,
            ..Default::default()
        };
        McpmRegistryClient {
            inner: Client::new(cfg),
        }
    }

    /// Construct a client that will talk plain HTTP (for local registries /
    /// testing).
    #[allow(dead_code)]
    pub fn new_http() -> Self {
        let cfg = ClientConfig {
            protocol: ClientProtocol::Http,
            ..Default::default()
        };
        McpmRegistryClient {
            inner: Client::new(cfg),
        }
    }

    // ── auth ──────────────────────────────────────────────────────────────────

    /// Resolve registry credentials for `image_ref`.
    ///
    /// Resolution order (highest priority first):
    ///
    /// 1. Environment variables:
    ///    - `REGISTRY_USERNAME` + `REGISTRY_PASSWORD`
    ///    - `MCPM_USERNAME` + `MCPM_PASSWORD`
    /// 2. Per-registry env vars derived from the registry hostname, e.g. for
    ///    `ghcr.io`: `GHCR_IO_USERNAME` + `GHCR_IO_PASSWORD`.
    /// 3. `~/.docker/config.json` basic-auth entries.
    /// 4. `RegistryAuth::Anonymous`.
    pub fn auth_for(image_ref: &str) -> RegistryAuth {
        // Parse the registry hostname from the image reference.
        let registry = registry_host_of(image_ref);

        // 1. Generic env-var credentials.
        if let Some(auth) = env_auth("REGISTRY") {
            return auth;
        }
        if let Some(auth) = env_auth("MCPM") {
            return auth;
        }

        // 2. Per-registry env-var credentials  (e.g. GHCR_IO_USERNAME).
        let prefix = registry.to_uppercase().replace('.', "_").replace('-', "_");
        if let Some(auth) = env_auth(&prefix) {
            return auth;
        }

        // 3. Docker config.json.
        if let Some(auth) = docker_config_auth(&registry) {
            return auth;
        }

        // 4. Fall back to anonymous.
        RegistryAuth::Anonymous
    }

    // ── public operations ─────────────────────────────────────────────────────

    /// Fetch the manifest for `image_ref` and return `(manifest, digest)`.
    ///
    /// `image_ref` may be a tag reference or a digest reference.
    ///
    /// The manifest is pulled as raw bytes and parsed with `oci-spec`'s
    /// `ImageManifest::from_reader` so the return type is the spec-native type.
    pub async fn pull_manifest(&self, image_ref: &str) -> Result<(ImageManifest, String)> {
        let reference = parse_ref(image_ref)?;
        let auth = Self::auth_for(image_ref);

        let (raw, digest) = self
            .inner
            .pull_manifest_raw(
                &reference,
                &auth,
                &[oci_client::manifest::OCI_IMAGE_MEDIA_TYPE],
            )
            .await
            .with_context(|| format!("pulling manifest for {}", image_ref))?;

        let manifest = ImageManifest::from_reader(raw.as_ref())
            .with_context(|| format!("parsing manifest for {}", image_ref))?;

        Ok((manifest, digest))
    }

    /// List all tags available for the repository identified by `image_ref`.
    ///
    /// The registry and repository are extracted from `image_ref`; the tag
    /// portion is ignored.  Returns the raw tag strings exactly as the
    /// registry returns them (e.g. `["latest", "v2.4.0", "v2.4.5"]`).
    pub async fn list_tags(&self, image_ref: &str) -> Result<Vec<String>> {
        let reference = parse_ref(image_ref)?;
        let auth = Self::auth_for(image_ref);

        let response = self
            .inner
            .list_tags(&reference, &auth, None, None)
            .await
            .with_context(|| format!("listing tags for {}", image_ref))?;

        Ok(response.tags)
    }

    /// Resolve `image_ref` (tag form) to its content digest without
    /// downloading the full manifest body.
    #[allow(dead_code)]
    pub async fn resolve_digest(&self, image_ref: &str) -> Result<String> {
        let reference = parse_ref(image_ref)?;
        let auth = Self::auth_for(image_ref);

        let digest = self
            .inner
            .fetch_manifest_digest(&reference, &auth)
            .await
            .with_context(|| format!("fetching manifest digest for {}", image_ref))?;

        Ok(digest)
    }

    /// Pull a single layer blob and stream it into `out`.
    ///
    /// `descriptor` is an `oci-spec` [`Descriptor`]; it is bridged to the
    /// `oci-client` descriptor type internally.
    pub async fn pull_blob(
        &self,
        image_ref: &str,
        descriptor: &Descriptor,
        out: impl AsyncWrite + Unpin,
    ) -> Result<()> {
        let reference = parse_ref(image_ref)?;
        let auth = Self::auth_for(image_ref);

        // Bridge oci-spec Descriptor → oci-client OciDescriptor.
        let oci_desc = oci_client::manifest::OciDescriptor {
            media_type: descriptor.media_type().to_string(),
            digest: descriptor.digest().to_string(),
            size: descriptor.size() as i64,
            urls: None,
            annotations: None,
        };

        // Ensure we are authenticated before pulling a blob.
        self.inner
            .auth(&reference, &auth, RegistryOperation::Pull)
            .await
            .with_context(|| format!("authenticating to registry for {}", image_ref))?;

        self.inner
            .pull_blob(&reference, &oci_desc, out)
            .await
            .with_context(|| format!("pulling blob {} from {}", descriptor.digest(), image_ref))?;

        Ok(())
    }

    /// Push all blobs + manifest for a locally-built `LocalImage` to the
    /// registry under the given `image_ref` (must include a tag or digest).
    ///
    /// Only blobs stored in `image.new_blobs` are uploaded; inherited blobs
    /// are assumed to already exist in the target registry.
    ///
    /// ## Strategy
    ///
    /// 1. Authenticate.
    /// 2. Upload each new blob individually via `push_blob`.
    /// 3. Upload the config blob via `push_blob`.
    /// 4. Serialise the `oci-spec` `ImageManifest` to JSON and upload via
    ///    `push_manifest_raw` — no cross-library type bridging required.
    pub async fn push_image(&self, image_ref: &str, image: &LocalImage) -> Result<String> {
        let reference = parse_ref(image_ref)?;
        let auth = Self::auth_for(image_ref);

        // Authenticate once up front.
        self.inner
            .auth(&reference, &auth, RegistryOperation::Push)
            .await
            .with_context(|| format!("authenticating to push to {}", image_ref))?;

        // Push every new layer blob.
        for descriptor in image.manifest.layers() {
            let digest = descriptor.digest().to_string();
            if let Some(data) = image.get_blob(&digest) {
                eprintln!("[push]   uploading layer {}", short_digest(&digest));
                self.inner
                    .push_blob(&reference, data.to_vec(), &digest)
                    .await
                    .with_context(|| format!("pushing layer blob {} to {}", digest, image_ref))?;
            }
            // Blobs not in new_blobs are inherited — assumed to exist in the registry already.
        }

        // Push the config blob.
        let config_digest = image.manifest.config().digest().to_string();
        eprintln!("[push]   uploading config {}", short_digest(&config_digest));
        self.inner
            .push_blob(&reference, image.config_data.clone(), &config_digest)
            .await
            .with_context(|| format!("pushing config blob to {}", image_ref))?;

        // Serialise the oci-spec ImageManifest to JSON and push as raw bytes.
        let manifest_json = image
            .manifest
            .to_string()
            .context("serialising manifest for push")?;

        let content_type =
            http::header::HeaderValue::from_static("application/vnd.oci.image.manifest.v1+json");
        let manifest_url = self
            .inner
            .push_manifest_raw(&reference, manifest_json.into_bytes(), content_type)
            .await
            .with_context(|| format!("pushing manifest to {}", image_ref))?;

        Ok(manifest_url)
    }

    /// Download all layer blobs referenced by `manifest` into `cache`,
    /// skipping those already present.
    ///
    /// `image_ref` is used to scope authentication; it should match the
    /// registry and repository of the blobs.
    pub async fn fetch_layers_to_cache(
        &self,
        image_ref: &str,
        manifest: &ImageManifest,
        cache: &LocalCache,
    ) -> Result<()> {
        for descriptor in manifest.layers() {
            let digest = descriptor.digest().to_string();

            if cache.has_blob(&digest) {
                eprintln!("  layer {} already cached, skipping", short_digest(&digest));
                continue;
            }

            eprintln!(
                "  pulling layer {} ({} bytes)",
                short_digest(&digest),
                descriptor.size()
            );

            let raw = pull_blob_to_vec(self, image_ref, descriptor).await?;

            let actual_digest = crate::util::digest::sha256_digest(&raw);
            if actual_digest != digest {
                bail!(
                    "layer digest mismatch for {}: expected {}, got {}",
                    image_ref,
                    digest,
                    actual_digest
                );
            }

            cache
                .store_blob(&raw)
                .with_context(|| format!("caching layer {}", digest))?;
        }

        Ok(())
    }

    /// Fetch the image config blob into the cache.
    #[allow(dead_code)]
    pub async fn fetch_config_to_cache(
        &self,
        image_ref: &str,
        manifest: &ImageManifest,
        cache: &LocalCache,
    ) -> Result<()> {
        let descriptor = manifest.config();
        let digest = descriptor.digest().to_string();
        if cache.has_blob(&digest) {
            return Ok(());
        }

        let raw = pull_blob_to_vec(self, image_ref, descriptor).await?;
        cache.store_blob(&raw)?;
        Ok(())
    }
}

impl Default for McpmRegistryClient {
    fn default() -> Self {
        Self::new()
    }
}

// ── Internal blob-download helper ─────────────────────────────────────────────

/// Download a single blob (described by an `oci-spec` [`Descriptor`]) into an
/// in-memory `Vec<u8>`.  Used by `fetch_layers_to_cache` and
/// `fetch_config_to_cache`.
async fn pull_blob_to_vec(
    client: &McpmRegistryClient,
    image_ref: &str,
    descriptor: &Descriptor,
) -> Result<Vec<u8>> {


    let mut writer = AsyncVecWriter::new();
    client.pull_blob(image_ref, descriptor, &mut writer).await?;
    // pull_blob writes directly; no explicit flush needed for AsyncVecWriter,
    // but call it anyway for correctness.
    tokio::io::AsyncWriteExt::flush(&mut writer)
        .await
        .context("flushing blob download buffer")?;
    Ok(writer.into_inner())
}

// ── AsyncVecWriter ────────────────────────────────────────────────────────────

/// A minimal `AsyncWrite` that accumulates bytes into a `Vec<u8>`.
struct AsyncVecWriter {
    buf: Vec<u8>,
}

impl AsyncVecWriter {
    fn new() -> Self {
        AsyncVecWriter { buf: Vec::new() }
    }

    fn into_inner(self) -> Vec<u8> {
        self.buf
    }
}

impl tokio::io::AsyncWrite for AsyncVecWriter {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.buf.extend_from_slice(buf);
        std::task::Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

// ── Reference parsing ─────────────────────────────────────────────────────────

/// Parse a string into an `oci_client::Reference`, providing a
/// descriptive error on failure.
pub fn parse_ref(image_ref: &str) -> Result<Reference> {
    image_ref
        .parse::<Reference>()
        .with_context(|| format!("invalid OCI image reference: '{}'", image_ref))
}

/// Extract the registry hostname from an image reference string.
///
/// Examples:
/// - `"ghcr.io/author/image:tag"` → `"ghcr.io"`
/// - `"docker.io/library/nginx:latest"` → `"docker.io"`
/// - `"nginx:latest"` (Docker Hub implicit) → `"index.docker.io"`
pub fn registry_host_of(image_ref: &str) -> String {
    // oci-client's `Reference::registry()` handles all edge cases.
    if let Ok(r) = image_ref.parse::<Reference>() {
        return r.registry().to_string();
    }
    // Fallback: first component before the first `/` if it looks like a hostname.
    if let Some(slash) = image_ref.find('/') {
        let potential_host = &image_ref[..slash];
        if potential_host.contains('.') || potential_host.contains(':') {
            return potential_host.to_string();
        }
    }
    "index.docker.io".to_string()
}

// ── Credential resolution helpers ─────────────────────────────────────────────

/// Try to build `RegistryAuth::Basic` from `{PREFIX}_USERNAME` and
/// `{PREFIX}_PASSWORD` environment variables.
fn env_auth(prefix: &str) -> Option<RegistryAuth> {
    let username = std::env::var(format!("{}_USERNAME", prefix)).ok()?;
    let password = std::env::var(format!("{}_PASSWORD", prefix)).ok()?;
    if username.is_empty() {
        return None;
    }
    Some(RegistryAuth::Basic(username, password))
}

// ── Docker config.json ────────────────────────────────────────────────────────

/// The `~/.docker/config.json` structure (only the fields we care about).
#[derive(Debug, Deserialize)]
struct DockerConfig {
    #[serde(default)]
    auths: HashMap<String, DockerAuthEntry>,
}

#[derive(Debug, Deserialize)]
struct DockerAuthEntry {
    /// Base64-encoded `"username:password"`.
    #[serde(default)]
    auth: String,
    /// Plain-text username (some tools write this instead of `auth`).
    #[serde(default)]
    username: String,
    /// Plain-text password.
    #[serde(default)]
    password: String,
}

/// Look up credentials for `registry` in the user's docker `config.json`.
///
/// Returns `None` if the file is absent, unreadable, or contains no entry for
/// the given registry.
fn docker_config_auth(registry: &str) -> Option<RegistryAuth> {
    let config_path = docker_config_path()?;
    let content = std::fs::read_to_string(&config_path).ok()?;
    let config: DockerConfig = serde_json::from_str(&content).ok()?;

    // Normalise: try both the bare hostname and the https:// prefixed form.
    let candidates = [
        registry.to_string(),
        format!("https://{}", registry),
        format!("https://{}/v1/", registry),
        format!("https://{}/v2/", registry),
    ];

    for key in &candidates {
        if let Some(entry) = config.auths.get(key.as_str()) {
            // Prefer the explicit username/password fields.
            if !entry.username.is_empty() {
                return Some(RegistryAuth::Basic(
                    entry.username.clone(),
                    entry.password.clone(),
                ));
            }
            // Fall back to base64-decoded `auth` field.
            if !entry.auth.is_empty() {
                if let Some(auth) = decode_docker_auth(&entry.auth) {
                    return Some(auth);
                }
            }
        }
    }

    None
}

/// Decode a base64-encoded `"username:password"` docker auth token.
fn decode_docker_auth(b64: &str) -> Option<RegistryAuth> {
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .ok()?;
    let s = String::from_utf8(decoded).ok()?;
    let (user, pass) = s.split_once(':')?;
    Some(RegistryAuth::Basic(user.to_string(), pass.to_string()))
}

/// Return the path to `~/.docker/config.json`, or `None` if the home
/// directory cannot be determined.
fn docker_config_path() -> Option<PathBuf> {
    // Honour DOCKER_CONFIG env override (used by CI systems).
    if let Ok(dir) = std::env::var("DOCKER_CONFIG") {
        return Some(PathBuf::from(dir).join("config.json"));
    }
    dirs::home_dir().map(|h| h.join(".docker").join("config.json"))
}

// ── short_digest helper ───────────────────────────────────────────────────────

/// Return a human-readable short form of a digest (`"sha256:abcdef12"`).
fn short_digest(digest: &str) -> String {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    let short = &hex[..hex.len().min(12)];
    format!("sha256:{}", short)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_host_ghcr() {
        assert_eq!(
            registry_host_of("ghcr.io/someauthor/essentials:v2"),
            "ghcr.io"
        );
    }

    #[test]
    fn registry_host_dockerhub_explicit() {
        // docker.io images resolve to index.docker.io via oci-client.
        let host = registry_host_of("docker.io/library/nginx:latest");
        assert!(host.contains("docker.io"), "unexpected host: {}", host);
    }

    #[test]
    fn registry_host_with_port() {
        assert_eq!(
            registry_host_of("localhost:5000/myimage:latest"),
            "localhost:5000"
        );
    }

    #[test]
    fn decode_docker_auth_valid() {
        // "user:pass" base64-encoded
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode("myuser:mypassword");
        match decode_docker_auth(&b64) {
            Some(RegistryAuth::Basic(u, p)) => {
                assert_eq!(u, "myuser");
                assert_eq!(p, "mypassword");
            }
            other => panic!("expected Basic auth, got {:?}", other),
        }
    }

    #[test]
    fn decode_docker_auth_colon_in_password() {
        use base64::Engine;
        // Password contains a colon — only the first colon is the separator.
        let b64 = base64::engine::general_purpose::STANDARD.encode("user:pa:ss:word");
        match decode_docker_auth(&b64) {
            Some(RegistryAuth::Basic(u, p)) => {
                assert_eq!(u, "user");
                assert_eq!(p, "pa:ss:word");
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn decode_docker_auth_invalid_b64() {
        assert!(decode_docker_auth("!!!not-base64!!!").is_none());
    }

    #[test]
    fn decode_docker_auth_no_colon() {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode("nocolon");
        assert!(decode_docker_auth(&b64).is_none());
    }

    #[test]
    fn short_digest_truncates() {
        let d = "sha256:abcdef123456789";
        let s = short_digest(d);
        assert!(s.starts_with("sha256:"));
        assert!(s.len() < d.len());
    }

    #[test]
    fn parse_ref_valid() {
        assert!(parse_ref("ghcr.io/author/image:latest").is_ok());
        assert!(parse_ref("localhost:5000/test:v1").is_ok());
    }

    #[test]
    fn new_client_is_default() {
        let _c = McpmRegistryClient::new();
        let _d = McpmRegistryClient::default();
    }
}
