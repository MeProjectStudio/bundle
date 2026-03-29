//! `bundle pull` — resolve all bundle tags in `bundle.toml` to sha256 digests,
//! `bundle pull` — like `docker compose pull`.
//!
//! Resolves all bundle tags in `bundle.toml` to sha256 digests (optionally
//! resolving semver range expressions first) and downloads every layer blob
//! to the local cache.  Writes `bundle.lock`.
//!
//! ## No filesystem changes
//!
//! `bundle pull` **only** updates the local blob cache (`~/.cache/bundle/`)
//! and the `bundle.lock` file.  It never extracts layers onto the server
//! directory.  Use `bundle apply` to also update the server FS, or
//! `bundle run` to do everything at once.
//!
//! ## Docker Compose analogy
//!
//! | Command        | Compose equivalent         | What it does                     |
//! |----------------|----------------------------|----------------------------------|
//! | `bundle pull`  | `docker compose pull`      | cache update only, no FS changes |
//! | `bundle apply` | pull + install (no start)  | pull + extract layers onto FS    |
//! | `bundle run`   | `docker compose up`        | pull + apply to FS + start server|
//!
//! ## What it does
//!
//! 1. Read `bundle.toml` → `bundles` map (`name → image:tag`).
//! 2. For each bundle (semver ranges are resolved to concrete tags first):
//!    a. Fetch the manifest from the registry.
//!    b. Record the manifest digest in `bundle.lock`.
//!    c. Download every layer blob (and config blob) not already in the
//!    local cache (`~/.cache/bundle/blobs/sha256/`).
//!    d. Store the manifest JSON in the local cache keyed by image ref.
//! 3. Write `bundle.lock`.
//!
//! ## Caching
//!
//! Blobs that are already cached are skipped (content-addressed, so a cache
//! hit means the data is definitely correct).  Run `bundle pull` again at any
//! time to refresh or re-verify without touching the server filesystem.
//!
//! ## Lock file
//!
//! After a successful pull `bundle.lock` is rewritten from scratch so that
//! removed bundles are pruned automatically.  Entries are sorted for
//! deterministic diffs.

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;

use crate::project::config::ProjectConfig;
use crate::project::lock::LockFile;
use crate::registry::client::McpmRegistryClient;
use crate::registry::semver as sv;
use crate::registry::types::{Descriptor, LocalCache};

// ── Entry point ───────────────────────────────────────────────────────────────

/// Run `bundle pull`.
pub async fn run() -> Result<()> {
    // ── Load project config ───────────────────────────────────────────────────
    let config =
        ProjectConfig::load().context("reading bundle.toml (run `bundle init` to create one)")?;

    if config.bundles.is_empty() {
        println!("No bundles declared in bundle.toml — nothing to pull.");
        return Ok(());
    }

    eprintln!(
        "[pull] found {} bundle(s) in bundle.toml",
        config.bundles.len()
    );

    // ── Open cache and registry client ────────────────────────────────────────
    let cache = LocalCache::open().context("opening local cache")?;
    let client = McpmRegistryClient::new();

    // ── Process each bundle ───────────────────────────────────────────────────
    let mut new_lock = LockFile::default();

    // Sort bundles for deterministic output.
    let mut bundles: Vec<(&String, &String)> = config.bundles.iter().collect();
    bundles.sort_by_key(|(name, _)| name.as_str());

    for (name, image_ref) in bundles {
        println!();
        println!("  bundle: {} ({})", name, image_ref);

        // Resolve semver range → concrete tag before pulling, so the lock file
        // records the exact resolved tag (e.g. "v2.4.5") not the range ("2.4").
        let resolved_ref = if sv::is_range(image_ref) {
            match resolve_semver(&client, image_ref).await {
                Ok(r) => {
                    if r != *image_ref {
                        println!("    semver {} → {}", image_ref, r);
                    }
                    r
                }
                Err(e) => {
                    eprintln!("    ! semver resolution failed for {}: {:#}", image_ref, e);
                    eprintln!("    ! falling back to literal tag");
                    image_ref.clone()
                }
            }
        } else {
            image_ref.clone()
        };

        match pull_bundle(&client, &cache, name, &resolved_ref).await {
            Ok(digest) => {
                println!("    ✓ digest: {}", short(&digest));
                // Always key the lock entry by the original image_ref (which
                // may be a range like "2.4") so that apply can look it up.
                // The resolved_ref is recorded as the value so the exact tag
                // is visible in the lock file alongside the digest.
                if resolved_ref != *image_ref {
                    // Store as "resolved_ref@digest" so the lock captures both.
                    new_lock.set_digest(image_ref.clone(), format!("{}@{}", resolved_ref, digest));
                } else {
                    new_lock.set_digest(image_ref.clone(), digest);
                }
            }
            Err(e) => {
                // Surface the error but continue with remaining bundles so the
                // operator can see all failures at once.
                eprintln!("    ✗ error pulling '{}': {:#}", resolved_ref, e);
                // Re-use any existing pinned digest so the lock file doesn't
                // regress for the bundles that *did* succeed in a prior pull.
                let existing_lock = LockFile::load().unwrap_or_default();
                if let Some(existing_digest) = existing_lock.get_digest(image_ref) {
                    eprintln!("    ! keeping previous digest: {}", short(existing_digest));
                    new_lock.set_digest(image_ref.clone(), existing_digest.to_string());
                }
            }
        }
    }

    // ── Write bundle.lock ───────────────────────────────────────────────────────
    new_lock.save().context("writing bundle.lock")?;
    println!();
    println!(
        "✓ bundle.lock written ({} bundle(s))",
        new_lock.bundles.len()
    );

    Ok(())
}

// ── Semver resolution ─────────────────────────────────────────────────────────

/// Resolve a semver range tag in `image_ref` to a concrete tag by listing all
/// tags from the registry and picking the highest matching stable version.
///
/// Returns a new image reference with the resolved tag substituted in, e.g.
/// `"ghcr.io/author/essentials:2.4"` → `"ghcr.io/author/essentials:v2.4.5"`.
async fn resolve_semver(client: &McpmRegistryClient, image_ref: &str) -> Result<String> {
    let tag = sv::tag_of(image_ref)
        .ok_or_else(|| anyhow::anyhow!("could not extract tag from {}", image_ref))?;

    eprintln!("[pull]   listing tags to resolve semver range {:?}", tag);

    let all_tags = client
        .list_tags(image_ref)
        .await
        .with_context(|| format!("listing tags for {}", image_ref))?;

    eprintln!("[pull]   {} tag(s) available", all_tags.len());

    let resolved_tag = sv::resolve(tag, &all_tags)
        .with_context(|| format!("resolving semver range {:?} for {}", tag, image_ref))?;

    Ok(sv::rewrite_tag(image_ref, &resolved_tag))
}

// ── Per-bundle pull ───────────────────────────────────────────────────────────

/// Pull one bundle: fetch its manifest, cache all layer blobs, and return
/// the manifest digest.
async fn pull_bundle(
    client: &McpmRegistryClient,
    cache: &LocalCache,
    _name: &str,
    image_ref: &str,
) -> Result<String> {
    // ── Fetch manifest ────────────────────────────────────────────────────────
    eprintln!("[pull]   fetching manifest for {}", image_ref);

    let (manifest, digest) = client
        .pull_manifest(image_ref)
        .await
        .with_context(|| format!("fetching manifest for {}", image_ref))?;

    // Serialise and cache the manifest via oci-spec.
    let manifest_json = manifest
        .to_string()
        .context("serialising manifest")?
        .into_bytes();
    cache
        .store_manifest(image_ref, &manifest_json, &digest)
        .with_context(|| format!("caching manifest for {}", image_ref))?;

    eprintln!("[pull]   manifest digest: {}", short(&digest));
    eprintln!("[pull]   {} layer(s) to check", manifest.layers().len());

    // ── Download layers ───────────────────────────────────────────────────────
    for (idx, layer) in manifest.layers().iter().enumerate() {
        let layer_digest = layer.digest().to_string();
        eprint!(
            "[pull]   layer [{}/{}] {} ({} bytes) … ",
            idx + 1,
            manifest.layers().len(),
            short(&layer_digest),
            layer.size()
        );

        if cache.has_blob(&layer_digest) {
            eprintln!("cached ✓");
            continue;
        }

        // Download into a Vec<u8>, then store in the blob cache.
        let raw = download_blob(client, image_ref, layer)
            .await
            .with_context(|| {
                format!(
                    "downloading layer {} of {}",
                    short(&layer_digest),
                    image_ref
                )
            })?;

        // Verify digest.
        let actual = crate::util::digest::sha256_digest(&raw);
        if actual != layer_digest {
            anyhow::bail!(
                "digest mismatch for layer {}: expected {}, got {}",
                idx + 1,
                layer_digest,
                actual
            );
        }

        cache
            .store_blob(&raw)
            .with_context(|| format!("storing layer blob {}", layer_digest))?;

        eprintln!("done ✓ ({} bytes)", raw.len());
    }

    // ── Download config blob ──────────────────────────────────────────────────
    {
        let cfg_desc = manifest.config();
        let cfg_digest = cfg_desc.digest().to_string();
        eprint!("[pull]   config {} … ", short(&cfg_digest));

        if cache.has_blob(&cfg_digest) {
            eprintln!("cached ✓");
        } else {
            let raw = download_blob(client, image_ref, cfg_desc)
                .await
                .context("downloading image config blob")?;

            let actual = crate::util::digest::sha256_digest(&raw);
            if actual != cfg_digest {
                anyhow::bail!(
                    "config blob digest mismatch: expected {}, got {}",
                    cfg_digest,
                    actual
                );
            }

            cache.store_blob(&raw).context("storing config blob")?;
            eprintln!("done ✓ ({} bytes)", raw.len());
        }
    }

    Ok(digest)
}

// ── Download helper ───────────────────────────────────────────────────────────

/// Download a single OCI blob (layer or config) and return the raw bytes.
async fn download_blob(
    client: &McpmRegistryClient,
    image_ref: &str,
    descriptor: &Descriptor,
) -> Result<Vec<u8>> {
    let digest = descriptor.digest().to_string();
    let mut writer = AsyncVecWriter::new();
    client
        .pull_blob(image_ref, descriptor, &mut writer)
        .await
        .with_context(|| format!("pulling blob {} from {}", digest, image_ref))?;
    writer
        .flush()
        .await
        .context("flushing blob download buffer")?;
    Ok(writer.into_inner())
}

// ── AsyncVecWriter ────────────────────────────────────────────────────────────

/// A minimal `tokio::io::AsyncWrite` impl that accumulates bytes into a Vec.
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

// ── Helpers ───────────────────────────────────────────────────────────────────

fn short(digest: &str) -> String {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    format!("sha256:{}", &hex[..hex.len().min(12)])
}
