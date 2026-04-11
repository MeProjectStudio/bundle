use std::time::Duration;

use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tokio::io::AsyncWriteExt;

use crate::project::config::ProjectConfig;
use crate::project::lock::LockFile;
use crate::registry::client::{has_explicit_registry, McpmRegistryClient};
use crate::registry::semver as sv;
use crate::registry::types::{Descriptor, LocalCache};

// ── Progress-bar styles ───────────────────────────────────────────────────────

fn downloading_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{prefix:.bold.dim}  {spinner:.cyan}  [{bar:40.cyan/blue}]  {bytes} / {total_bytes}",
    )
    .expect("valid indicatif template")
    .progress_chars("━╾─")
}

fn pull_complete_style() -> ProgressStyle {
    ProgressStyle::with_template("{prefix:.bold.dim}  {msg:.green}")
        .expect("valid indicatif template")
}

fn already_exists_style() -> ProgressStyle {
    ProgressStyle::with_template("{prefix:.bold.dim}  {msg:.dim}")
        .expect("valid indicatif template")
}

fn error_style() -> ProgressStyle {
    ProgressStyle::with_template("{prefix:.bold.dim}  {msg:.red}")
        .expect("valid indicatif template")
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Run `bundle server pull`.
pub async fn run() -> Result<()> {
    let config =
        ProjectConfig::load().context("reading bundle.toml (run `bundle init` to create one)")?;

    if config.bundles.is_empty() {
        println!("No bundles declared in bundle.toml — nothing to pull.");
        return Ok(());
    }

    let mp = MultiProgress::new();
    let client = McpmRegistryClient::new();
    let cache = LocalCache::open().context("opening local cache")?;

    let mut new_lock = LockFile::default();
    let mut bundles = config.bundles.clone();
    bundles.sort();

    for image_ref in &bundles {
        // ── Local tags: bare names with no registry hostname ──────────────
        if !has_explicit_registry(image_ref) {
            mp.println(format!("\nChecking local  {image_ref}"))?;
            if cache.has_manifest(image_ref) {
                let (_, digest) = cache
                    .load_manifest(image_ref)
                    .with_context(|| format!("loading local manifest for '{}'", image_ref))?;
                mp.println(format!("  Digest: {digest}"))?;
                mp.println(String::from("  Status: Up to date (local)"))?;
                new_lock.set_digest(image_ref.clone(), digest);
            } else {
                mp.println(format!(
                    "  ✗ not found in local cache\n\
                     \n\
                     hint: run `bundle build -t {image_ref}` to build it locally"
                ))?;
                // Keep the previous lock entry if there is one.
                let existing_lock = LockFile::load().unwrap_or_default();
                if let Some(existing_digest) = existing_lock.get_digest(image_ref) {
                    mp.println(format!(
                        "  ! keeping previous digest: {}",
                        short(existing_digest)
                    ))?;
                    new_lock.set_digest(image_ref.clone(), existing_digest.to_string());
                }
            }
            continue;
        }
        // ── Registry refs: existing pull logic ────────────────────────────
        mp.println(format!("\nPulling {image_ref}"))?;

        let resolved_ref = if sv::is_range(image_ref) {
            match resolve_semver(&client, image_ref).await {
                Ok(r) => {
                    if r != *image_ref {
                        mp.println(format!("  semver {image_ref} → {r}"))?;
                    }
                    r
                }
                Err(e) => {
                    mp.println(format!(
                        "  ! semver resolution failed for {image_ref}: {e:#}"
                    ))?;
                    mp.println(String::from("  ! falling back to literal tag"))?;
                    image_ref.clone()
                }
            }
        } else {
            image_ref.clone()
        };

        match pull_bundle(&client, &cache, &resolved_ref, &mp).await {
            Ok(digest) => {
                mp.println(format!("  Digest: {digest}"))?;
                mp.println(format!(
                    "  Status: Downloaded newer image for {resolved_ref}"
                ))?;
                if resolved_ref != *image_ref {
                    new_lock.set_digest(image_ref.clone(), format!("{resolved_ref}@{digest}"));
                } else {
                    new_lock.set_digest(image_ref.clone(), digest);
                }
            }
            Err(e) => {
                mp.println(format!("  ✗ error pulling '{resolved_ref}': {e:#}"))?;
                let existing_lock = LockFile::load().unwrap_or_default();
                if let Some(existing_digest) = existing_lock.get_digest(image_ref) {
                    mp.println(format!(
                        "  ! keeping previous digest: {}",
                        short(existing_digest)
                    ))?;
                    new_lock.set_digest(image_ref.clone(), existing_digest.to_string());
                }
            }
        }
    }

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
async fn resolve_semver(client: &McpmRegistryClient, image_ref: &str) -> Result<String> {
    let tag = sv::tag_of(image_ref)
        .ok_or_else(|| anyhow::anyhow!("could not extract tag from {}", image_ref))?;

    let all_tags = client
        .list_tags(image_ref)
        .await
        .with_context(|| format!("listing tags for {}", image_ref))?;

    let resolved_tag = sv::resolve(tag, &all_tags)
        .with_context(|| format!("resolving semver range {:?} for {}", tag, image_ref))?;

    Ok(sv::rewrite_tag(image_ref, &resolved_tag))
}

// ── Per-image pull ────────────────────────────────────────────────────────────

/// Pull one bundle: fetch its manifest, cache all layer + config blobs with
/// live per-blob progress bars, and return the manifest digest.
async fn pull_bundle(
    client: &McpmRegistryClient,
    cache: &LocalCache,
    image_ref: &str,
    mp: &MultiProgress,
) -> Result<String> {
    let (manifest, digest) = client
        .pull_manifest(image_ref)
        .await
        .with_context(|| format!("fetching manifest for {}", image_ref))?;

    // Cache the manifest JSON.
    let manifest_json = manifest
        .to_string()
        .context("serialising manifest")?
        .into_bytes();
    cache
        .store_manifest(image_ref, &manifest_json, &digest)
        .with_context(|| format!("caching manifest for {}", image_ref))?;

    // Pull each layer blob.
    let layer_count = manifest.layers().len();
    for (idx, layer) in manifest.layers().iter().enumerate() {
        pull_blob_with_progress(client, cache, image_ref, layer, mp)
            .await
            .with_context(|| format!("layer {}/{} of {}", idx + 1, layer_count, image_ref))?;
    }

    // Pull the config blob.
    let cfg_desc = manifest.config().clone();
    pull_blob_with_progress(client, cache, image_ref, &cfg_desc, mp)
        .await
        .context("config blob")?;

    Ok(digest)
}

// ── Per-blob pull with progress bar ──────────────────────────────────────────

/// Download (or confirm cached) a single OCI blob, showing a live progress bar.
///
/// The bar prefix is the 12-char short hex of the blob digest, mirroring the
/// Docker CLI style.  On completion the bar is replaced with a one-line
/// summary: **"Pull complete"** or **"Already exists"**.
async fn pull_blob_with_progress(
    client: &McpmRegistryClient,
    cache: &LocalCache,
    image_ref: &str,
    descriptor: &Descriptor,
    mp: &MultiProgress,
) -> Result<()> {
    let digest = descriptor.digest().to_string();
    let prefix = short_hex(&digest);

    let pb = mp.add(ProgressBar::new(descriptor.size()));
    pb.set_style(downloading_style());
    pb.set_prefix(prefix);
    pb.enable_steady_tick(Duration::from_millis(80));

    // Fast path: blob already cached.
    if cache.has_blob(&digest) {
        pb.set_style(already_exists_style());
        pb.finish_with_message("Already exists");
        return Ok(());
    }

    // Stream the blob through a ProgressWriter so every chunk advances the bar.
    let mut writer = ProgressWriter::new(pb.clone());
    client
        .pull_blob(image_ref, descriptor, &mut writer)
        .await
        .with_context(|| format!("pulling blob {} from {}", digest, image_ref))?;
    writer
        .flush()
        .await
        .context("flushing blob download buffer")?;
    let raw = writer.into_inner();

    // Verify digest before persisting.
    let actual = crate::util::digest::sha256_digest(&raw);
    if actual != digest {
        pb.set_style(error_style());
        pb.abandon_with_message("digest mismatch");
        anyhow::bail!("digest mismatch: expected {}, got {}", digest, actual);
    }

    cache
        .store_blob(&raw)
        .with_context(|| format!("storing blob {}", digest))?;

    pb.set_style(pull_complete_style());
    pb.finish_with_message("Pull complete");

    Ok(())
}

// ── Progress-aware AsyncWrite sink ───────────────────────────────────────────

/// An in-memory `AsyncWrite` that accumulates bytes into a `Vec<u8>` and
/// increments a `ProgressBar` by the number of bytes written in each chunk.
struct ProgressWriter {
    buf: Vec<u8>,
    pb: ProgressBar,
}

impl ProgressWriter {
    fn new(pb: ProgressBar) -> Self {
        ProgressWriter {
            buf: Vec::new(),
            pb,
        }
    }

    fn into_inner(self) -> Vec<u8> {
        self.buf
    }
}

impl tokio::io::AsyncWrite for ProgressWriter {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.buf.extend_from_slice(buf);
        self.pb.inc(buf.len() as u64);
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

/// Returns `"sha256:abc123456789"` (prefix + first 12 hex chars).
fn short(digest: &str) -> String {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    format!("sha256:{}", &hex[..hex.len().min(12)])
}

/// Returns just the first 12 hex chars of a digest (no `"sha256:"` prefix).
/// Used as the progress-bar prefix to mirror Docker's layer ID display.
fn short_hex(digest: &str) -> String {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    hex[..hex.len().min(12)].to_string()
}
