macro_rules! log {
    ($($t:tt)*) => { crate::progress!("pull", $($t)*) };
}

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;

use crate::project::config::ProjectConfig;
use crate::project::lock::LockFile;
use crate::registry::client::McpmRegistryClient;
use crate::registry::semver as sv;
use crate::registry::types::{Descriptor, LocalCache};

/// Run `bundle pull`.
pub async fn run() -> Result<()> {
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

    let cache = LocalCache::open().context("opening local cache")?;
    let client = McpmRegistryClient::new();

    let mut new_lock = LockFile::default();

    let mut bundles = config.bundles.clone();
    bundles.sort();

    for image_ref in &bundles {
        println!();
        println!("  bundle: {}", image_ref);

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

        match pull_bundle(&client, &cache, &resolved_ref).await {
            Ok(digest) => {
                println!("    ✓ digest: {}", short(&digest));
                if resolved_ref != *image_ref {
                    new_lock.set_digest(image_ref.clone(), format!("{}@{}", resolved_ref, digest));
                } else {
                    new_lock.set_digest(image_ref.clone(), digest);
                }
            }
            Err(e) => {
                eprintln!("    ✗ error pulling '{}': {:#}", resolved_ref, e);
                let existing_lock = LockFile::load().unwrap_or_default();
                if let Some(existing_digest) = existing_lock.get_digest(image_ref) {
                    eprintln!("    ! keeping previous digest: {}", short(existing_digest));
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

/// Resolve a semver range tag in `image_ref` to a concrete tag by listing all
/// tags from the registry and picking the highest matching stable version.
///
/// Returns a new image reference with the resolved tag substituted in, e.g.
/// `"ghcr.io/author/essentials:2.4"` → `"ghcr.io/author/essentials:v2.4.5"`.
async fn resolve_semver(client: &McpmRegistryClient, image_ref: &str) -> Result<String> {
    let tag = sv::tag_of(image_ref)
        .ok_or_else(|| anyhow::anyhow!("could not extract tag from {}", image_ref))?;

    log!("  listing tags to resolve semver range {:?}", tag);

    let all_tags = client
        .list_tags(image_ref)
        .await
        .with_context(|| format!("listing tags for {}", image_ref))?;

    log!("  {} tag(s) available", all_tags.len());

    let resolved_tag = sv::resolve(tag, &all_tags)
        .with_context(|| format!("resolving semver range {:?} for {}", tag, image_ref))?;

    Ok(sv::rewrite_tag(image_ref, &resolved_tag))
}

/// Pull one bundle: fetch its manifest, cache all layer blobs, and return
/// the manifest digest.
async fn pull_bundle(
    client: &McpmRegistryClient,
    cache: &LocalCache,
    image_ref: &str,
) -> Result<String> {
    log!("  fetching manifest for {}", image_ref);

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

    log!("  manifest digest: {}", short(&digest));
    log!("  {} layer(s) to check", manifest.layers().len());

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

fn short(digest: &str) -> String {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    format!("sha256:{}", &hex[..hex.len().min(12)])
}
