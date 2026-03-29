//! `bundle push <image:tag>` — push the most recently built `LocalImage` to an
//! OCI-compliant registry.
//!
//! The image must have been built first with `bundle build`.  The locally stored
//! manifest and blobs are read from `~/.cache/mcpm/built/`, then uploaded to
//! the target registry using the `oci-client` client.
//!
//! ## Authentication
//!
//! Credentials are resolved in this order (highest priority first):
//!
//! 1. `{REGISTRY}_USERNAME` / `{REGISTRY}_PASSWORD` env vars
//!    (e.g. `GHCR_IO_USERNAME` / `GHCR_IO_PASSWORD` for `ghcr.io`)
//! 2. `REGISTRY_USERNAME` / `REGISTRY_PASSWORD` generic env vars
//! 3. `~/.docker/config.json` basic-auth entries
//! 4. Anonymous (will fail for private registries)

use anyhow::{bail, Context, Result};

use crate::registry::client::{parse_ref, McpmRegistryClient};
use crate::registry::types::LocalCache;

// ── Arguments ─────────────────────────────────────────────────────────────────

/// Arguments accepted by `bundle push`.
#[derive(Debug, Clone)]
pub struct PushArgs {
    /// The fully-qualified image reference to push to, including tag.
    ///
    /// Examples:
    /// - `ghcr.io/someauthor/essentials:v2.20.1`
    /// - `docker.io/myorg/my-server-bundle:latest`
    pub image_ref: String,
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Run `bundle push <image:tag>`.
pub async fn run(args: PushArgs) -> Result<()> {
    let image_ref = args.image_ref.trim();

    // ── Validate the reference ────────────────────────────────────────────────
    let reference = parse_ref(image_ref)
        .with_context(|| format!("invalid image reference: '{}'", image_ref))?;

    // Require an explicit tag or digest — refuse "latest" ambiguity only when
    // the user supplies nothing at all (parse_ref defaults to "latest").
    // We accept "latest" explicitly; this is just a guard against a totally
    // empty tag field.
    if reference.tag().is_none() && reference.digest().is_none() {
        bail!(
            "image reference '{}' has no tag or digest.\n\
             Provide an explicit tag, e.g. `{}:latest`.",
            image_ref,
            image_ref
        );
    }

    eprintln!("[push] target: {}", image_ref);

    // ── Load the locally built image ──────────────────────────────────────────
    let cache = LocalCache::open().context("opening local cache")?;

    let image = cache
        .load_built_image()
        .context("loading built image from local cache (have you run `bundle build`?)")?;

    eprintln!(
        "[push] loaded built image: {} layer(s), {} new blob(s)",
        image.manifest.layers().len(),
        // Subtract config blob from the count so the output isn't confusing.
        image.new_blobs.len().saturating_sub(1),
    );

    // ── Show what we are about to push ────────────────────────────────────────
    eprintln!(
        "[push] manifest digest (pre-push): {}",
        short(&image.manifest.config().digest().to_string())
    );
    eprintln!("[push] layers:");
    for (i, layer) in image.manifest.layers().iter().enumerate() {
        let digest = layer.digest().to_string();
        let is_new = image.has_blob(&digest);
        eprintln!(
            "[push]   [{}/{}] {} ({} bytes) {}",
            i + 1,
            image.manifest.layers().len(),
            short(&digest),
            layer.size(),
            if is_new {
                "(new)"
            } else {
                "(inherited, skipping upload)"
            },
        );
    }

    // ── Push ──────────────────────────────────────────────────────────────────
    eprintln!("[push] pushing to {}…", image_ref);

    let client = McpmRegistryClient::new();

    let manifest_url = client
        .push_image(image_ref, &image)
        .await
        .with_context(|| format!("pushing image to {}", image_ref))?;

    // ── Summary ───────────────────────────────────────────────────────────────
    println!();
    println!("✓ Push complete");
    println!("  image   : {}", image_ref);
    println!("  manifest: {}", manifest_url);
    println!();
    println!("Pull with: docker pull {}", image_ref);
    println!("Reference in bundle.toml:\n  my-bundle = \"{}\"", image_ref);

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Return a short human-readable form of a digest.
fn short(digest: &str) -> String {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    format!("sha256:{}", &hex[..hex.len().min(12)])
}
