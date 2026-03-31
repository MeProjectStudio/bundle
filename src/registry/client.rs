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

    /// Resolve credentials for `image_ref`.
    ///
    /// Order: env vars → containers auth.json → docker config.json → anonymous.
    /// The full image reference (not just hostname) is used for prefix matching
    /// so that keys like `registry.example.com/private` take precedence over
    /// `registry.example.com`.
    pub fn auth_for(image_ref: &str) -> RegistryAuth {
        let registry = registry_host_of(image_ref);
        let prefix = registry.to_uppercase().replace(['.', '-'], "_");

        if let Some(a) = env_auth("REGISTRY") {
            return a;
        }
        if let Some(a) = env_auth("MCPM") {
            return a;
        }
        if let Some(a) = env_auth(&prefix) {
            return a;
        }

        // containers/auth.json — checked before docker config.json.
        if let Some(path) = containers_auth_path() {
            if let Some(a) = auth_from_file(&path, image_ref) {
                return a;
            }
        }

        if let Some(path) = docker_config_path() {
            if let Some(a) = auth_from_file(&path, image_ref) {
                return a;
            }
        }

        RegistryAuth::Anonymous
    }

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

/// Return `true` if `image_ref` contains an explicit registry hostname.
///
/// OCI clients silently normalise bare references such as `myplugin:latest`
/// to `docker.io/library/myplugin:latest`.  Use this helper before any push
/// operation to ensure the user has been explicit about the destination.
///
/// Detection rule: the component before the first `/` is treated as a
/// registry hostname when it contains a `.` (e.g. `ghcr.io`), a `:` (e.g.
/// `localhost:5000`), or is exactly `localhost`.
///
/// | Reference                                       | Has registry? |
/// |-------------------------------------------------|---------------|
/// | `myplugin:latest`                               | No            |
/// | `me/myplugin:latest`                            | No            |
/// | `ghcr.io/me/myplugin:latest`                   | Yes           |
/// | `registry.example.com/plugins/myplugin:v1`    | Yes           |
/// | `localhost:5000/myplugin:latest`               | Yes           |
/// | `docker.io/me/myplugin:latest`                 | Yes (explicit)|
pub fn has_explicit_registry(image_ref: &str) -> bool {
    match image_ref.find('/') {
        None => false, // no slash → bare image name, no registry component
        Some(pos) => {
            let before_slash = &image_ref[..pos];
            // A registry hostname contains a dot (ghcr.io, registry.example.com),
            // a colon (localhost:5000), or is literally "localhost".
            before_slash.contains('.') || before_slash.contains(':') || before_slash == "localhost"
        }
    }
}

/// Validate that `image_ref` contains an explicit registry hostname.
///
/// Returns a descriptive error when no registry is present so that bare
/// references (e.g. `myplugin:latest`) are caught *before* they silently
/// reach Docker Hub.
pub fn require_explicit_registry(image_ref: &str) -> Result<()> {
    if has_explicit_registry(image_ref) {
        return Ok(());
    }

    // Extract just the image/repo portion for the suggestion.
    let image_part = image_ref
        .split(':')
        .next()
        .unwrap_or(image_ref)
        .split('/')
        .next_back()
        .unwrap_or(image_ref);

    bail!(
        "image reference '{}' has no explicit registry.\n\
         \n\
         `bundle push` and `bundle build -t` require a fully-qualified\n\
         reference that includes the registry hostname, for example:\n\
         \n    bundle push registry.example.com/plugins/{}:latest\n\
         \n    bundle push ghcr.io/myorg/{}:latest\n\
         \n\
         No default registry is assumed — be explicit about where to push.",
        image_ref,
        image_part,
        image_part,
    )
}

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

/// Shared structure for both `containers/auth.json` and `~/.docker/config.json`.
#[derive(Debug, serde::Serialize, Deserialize, Default)]
pub struct AuthFile {
    #[serde(default)]
    pub auths: HashMap<String, AuthEntry>,
}

#[derive(Debug, serde::Serialize, Deserialize, Default)]
pub struct AuthEntry {
    /// base64("username:password")
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub auth: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub username: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub password: String,
}

/// Look up credentials for `image_ref` in an auth file.
///
/// Tries keys from most-specific to least-specific so that
/// `registry.example.com/private` takes precedence over `registry.example.com`.
/// Also tries the conventional `https://` prefixed forms used by Docker.
pub fn auth_from_file(path: &std::path::Path, image_ref: &str) -> Option<RegistryAuth> {
    let content = std::fs::read_to_string(path).ok()?;
    let file: AuthFile = serde_json::from_str(&content).ok()?;

    // Build candidates from most-specific to least-specific.
    // Strip tag/digest so `registry/repo:tag` → `registry/repo`.
    let bare = image_ref
        .split('@')
        .next()
        .unwrap_or(image_ref)
        .split(':')
        .next()
        .unwrap_or(image_ref);

    let mut candidates: Vec<String> = Vec::new();
    // Add progressively shorter prefixes: "a/b/c", "a/b", "a"
    let mut s = bare.to_string();
    loop {
        candidates.push(s.clone());
        candidates.push(format!("https://{}", s));
        candidates.push(format!("https://{}/v1/", s));
        candidates.push(format!("https://{}/v2/", s));
        match s.rfind('/') {
            Some(pos) => s = s[..pos].to_string(),
            None => break,
        }
    }

    for key in &candidates {
        if let Some(entry) = file.auths.get(key.as_str()) {
            if !entry.username.is_empty() {
                return Some(RegistryAuth::Basic(
                    entry.username.clone(),
                    entry.password.clone(),
                ));
            }
            if !entry.auth.is_empty() {
                if let Some(a) = decode_auth_token(&entry.auth) {
                    return Some(a);
                }
            }
        }
    }
    None
}

/// Encode `username:password` as a base64 auth token.
pub fn encode_auth_token(username: &str, password: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", username, password))
}

/// Decode a base64 `"username:password"` auth token.
pub fn decode_auth_token(b64: &str) -> Option<RegistryAuth> {
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .ok()?;
    let s = String::from_utf8(decoded).ok()?;
    let (user, pass) = s.split_once(':')?;
    Some(RegistryAuth::Basic(user.to_string(), pass.to_string()))
}

/// Path to `${XDG_RUNTIME_DIR}/containers/auth.json` (Linux) or
/// `$HOME/.config/containers/auth.json` (other platforms).
/// Overridden by `REGISTRY_AUTH_FILE` environment variable.
pub fn containers_auth_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("REGISTRY_AUTH_FILE") {
        return Some(PathBuf::from(p));
    }
    #[cfg(unix)]
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        return Some(PathBuf::from(runtime).join("containers/auth.json"));
    }
    dirs::config_dir().map(|d| d.join("containers/auth.json"))
}

fn docker_config_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("DOCKER_CONFIG") {
        return Some(PathBuf::from(dir).join("config.json"));
    }
    dirs::home_dir().map(|h| h.join(".docker/config.json"))
}

/// Return a human-readable short form of a digest (`"sha256:abcdef12"`).
fn short_digest(digest: &str) -> String {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    let short = &hex[..hex.len().min(12)];
    format!("sha256:{}", short)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_name_has_no_registry() {
        assert!(!has_explicit_registry("myplugin:latest"));
        assert!(!has_explicit_registry("myplugin"));
    }

    #[test]
    fn dockerhub_org_slash_has_no_registry() {
        // "me/plugin" looks like a Docker Hub org/repo shorthand — no hostname.
        assert!(!has_explicit_registry("me/plugin:latest"));
    }

    #[test]
    fn ghcr_has_explicit_registry() {
        assert!(has_explicit_registry("ghcr.io/me/plugin:latest"));
        assert!(has_explicit_registry("ghcr.io/me/plugin"));
    }

    #[test]
    fn custom_registry_has_explicit_registry() {
        assert!(has_explicit_registry(
            "registry.mprjct.ru/plugins/worldedit:v7.4.1"
        ));
    }

    #[test]
    fn localhost_has_explicit_registry() {
        assert!(has_explicit_registry("localhost/myimage:latest"));
        assert!(has_explicit_registry("localhost:5000/myimage:latest"));
    }

    #[test]
    fn explicit_docker_io_has_explicit_registry() {
        // User typed docker.io explicitly — that is allowed.
        assert!(has_explicit_registry("docker.io/me/plugin:latest"));
    }

    #[test]
    fn require_explicit_registry_ok_for_qualified_ref() {
        assert!(require_explicit_registry("ghcr.io/me/plugin:latest").is_ok());
        assert!(require_explicit_registry("registry.example.com/ns/img:v1").is_ok());
        assert!(require_explicit_registry("localhost:5000/img:latest").is_ok());
    }

    #[test]
    fn require_explicit_registry_err_for_bare_ref() {
        let err = require_explicit_registry("myplugin:latest").unwrap_err();
        let msg = format!("{:#}", err);
        assert!(msg.contains("no explicit registry"), "got: {}", msg);
        assert!(msg.contains("registry hostname"), "got: {}", msg);
    }

    #[test]
    fn require_explicit_registry_err_suggests_image_name() {
        let err = require_explicit_registry("myplugin:latest").unwrap_err();
        let msg = format!("{:#}", err);
        // The suggestion should mention the bare image name.
        assert!(msg.contains("myplugin"), "got: {}", msg);
    }

    #[test]
    fn auth_from_file_most_specific_wins() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut f = NamedTempFile::new().unwrap();
        write!(f, r#"{{"auths":{{"registry.example.com/private":{{"auth":"{}"}},"registry.example.com":{{"auth":"{}"}}}}}}"#,
            encode_auth_token("private-user", "private-pass"),
            encode_auth_token("generic-user", "generic-pass"),
        ).unwrap();

        // Most specific key wins.
        let auth = auth_from_file(f.path(), "registry.example.com/private/image:tag");
        assert!(matches!(auth, Some(RegistryAuth::Basic(u, _)) if u == "private-user"));

        // Falls back to less specific.
        let auth = auth_from_file(f.path(), "registry.example.com/public/image:tag");
        assert!(matches!(auth, Some(RegistryAuth::Basic(u, _)) if u == "generic-user"));
    }

    #[test]
    fn encode_decode_round_trip() {
        let token = encode_auth_token("alice", "s3cr3t");
        match decode_auth_token(&token) {
            Some(RegistryAuth::Basic(u, p)) => {
                assert_eq!(u, "alice");
                assert_eq!(p, "s3cr3t");
            }
            _ => panic!("expected Basic auth"),
        }
    }

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
    fn decode_auth_token_valid() {
        // "user:pass" base64-encoded
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode("myuser:mypassword");
        match decode_auth_token(&b64) {
            Some(RegistryAuth::Basic(u, p)) => {
                assert_eq!(u, "myuser");
                assert_eq!(p, "mypassword");
            }
            other => panic!("expected Basic auth, got {:?}", other),
        }
    }

    #[test]
    fn decode_auth_token_colon_in_password() {
        use base64::Engine;
        // Password contains a colon — only the first colon is the separator.
        let b64 = base64::engine::general_purpose::STANDARD.encode("user:pa:ss:word");
        match decode_auth_token(&b64) {
            Some(RegistryAuth::Basic(u, p)) => {
                assert_eq!(u, "user");
                assert_eq!(p, "pa:ss:word");
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn decode_auth_token_invalid_b64() {
        assert!(decode_auth_token("!!!not-base64!!!").is_none());
    }

    #[test]
    fn decode_auth_token_no_colon() {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode("nocolon");
        assert!(decode_auth_token(&b64).is_none());
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
