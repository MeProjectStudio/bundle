use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::apply::overlay::{apply_bundles, print_changes};
use crate::project::config::ProjectConfig;
use crate::project::lock::LockFile;
use crate::registry::client::McpmRegistryClient;
use crate::registry::types::{ImageManifest, LocalCache};

/// Arguments accepted by `bundle apply`.
#[derive(Debug, Clone, Default)]
pub struct ApplyArgs {
    /// If set, use this directory as the server root instead of `$PWD`.
    pub server_dir: Option<PathBuf>,

    /// If `true`, print what would change without writing anything (dry-run).
    /// This is the internal flag; the `bundle diff` subcommand sets it to `true`.
    pub dry_run: bool,

    /// Skip the automatic `bundle pull` step and use whatever is already in
    /// the local cache.  Useful for offline workflows or CI environments where
    /// a separate pull step has already been run.
    pub no_pull: bool,

    /// If `true`, skip files that would override paths listed in
    /// `server.deny-override` rather than failing hard.  The dangerous files
    /// are never written; the operation just continues instead of aborting.
    pub ignore_dangerous_override_attempts: bool,
}

/// Run `bundle apply` (or `bundle diff` when `args.dry_run` is `true`).
pub async fn run(args: ApplyArgs) -> Result<()> {
    let server_dir = match &args.server_dir {
        Some(d) => d.clone(),
        None => std::env::current_dir().context("getting current directory")?,
    };

    let action = if args.dry_run { "diff" } else { "apply" };

    eprintln!("[{}] server directory: {}", action, server_dir.display());

    // Always pull before applying so the cache and bundle.lock are fresh.
    // This matches the behaviour of `bundle run` (pull → apply → exec) and
    // means users never have to remember to run pull separately.
    // Skip only when --no-pull is explicitly requested.
    if !args.no_pull {
        eprintln!(
            "[{}] pulling bundles to refresh cache and bundle.lock…",
            action
        );
        crate::cmd::pull::run()
            .await
            .context("auto-pull failed (use --no-pull to skip)")?;
    } else {
        eprintln!("[{}] skipping pull (--no-pull)", action);
    }

    let project = ProjectConfig::load_from(&server_dir).with_context(|| {
        format!(
            "reading bundle.toml in {} (run `bundle init` to create one)",
            server_dir.display()
        )
    })?;

    if project.bundles.is_empty() {
        println!(
            "No bundles declared in bundle.toml — nothing to {}.",
            action
        );
        return Ok(());
    }

    eprintln!(
        "[{}] {} bundle(s) declared in bundle.toml",
        action,
        project.bundles.len()
    );

    let lock_path = server_dir.join(crate::project::lock::LOCK_FILE_NAME);
    let lock = LockFile::load_from(&lock_path).context("loading bundle.lock")?;

    if lock.bundles.is_empty() {
        eprintln!(
            "[{}] warning: bundle.lock is absent or empty after pull — \
             registry may be unreachable or bundle.toml has no bundles.",
            action
        );
    }

    let cache = LocalCache::open().context("opening local cache")?;

    let mut bundle_refs = project.bundles.clone();
    bundle_refs.sort();

    let mut bundles_to_apply: Vec<(String, ImageManifest)> = Vec::new();

    for image_ref in &bundle_refs {
        // Resolve to a pinned digest if the lock file has one.
        let resolved_ref = if let Some(digest) = lock.get_digest(image_ref) {
            // Use the digest-pinned form for reproducibility.
            let base = image_ref.split(':').next().unwrap_or(image_ref);
            // `registry/repo@sha256:…` form.
            let _at_digest = if digest.starts_with("sha256:") {
                format!("{}@{}", base.trim_end_matches('/'), digest)
            } else {
                image_ref.clone()
            };
            // Still use the tag form for cache lookup since we stored the
            // manifest under the tag key.
            image_ref.clone()
        } else {
            image_ref.clone()
        };

        eprintln!("[{}] resolving {} → {}", action, image_ref, resolved_ref);

        // Try to load from cache first.
        let manifest = if cache.has_manifest(&resolved_ref) {
            eprintln!("[{}]   loading from local cache", action);
            let (manifest_json, _digest) = cache
                .load_manifest(&resolved_ref)
                .with_context(|| format!("loading cached manifest for {}", resolved_ref))?;

            ImageManifest::from_reader(manifest_json.as_slice())
                .with_context(|| format!("parsing cached manifest for {}", resolved_ref))?
        } else {
            // Not cached — try to pull from the registry.
            eprintln!("[{}]   not in cache, pulling from registry…", action);
            pull_bundle_to_cache(&resolved_ref, &cache)
                .await
                .with_context(|| {
                    format!(
                        "pulling bundle '{}' — run `bundle pull` first",
                        resolved_ref
                    )
                })?
        };

        // Verify all layer blobs are cached.
        let mut missing: Vec<String> = Vec::new();
        for layer in manifest.layers() {
            if !cache.has_blob(layer.digest().as_ref()) {
                missing.push(layer.digest().to_string());
            }
        }
        if !missing.is_empty() {
            eprintln!(
                "[{}]   {} layer blob(s) missing from cache for '{}', pulling…",
                action,
                missing.len(),
                image_ref
            );
            pull_layers_to_cache(&resolved_ref, &manifest, &cache)
                .await
                .with_context(|| {
                    format!(
                        "fetching missing layer blobs for bundle '{}' — run `bundle pull`",
                        image_ref
                    )
                })?;
        }

        bundles_to_apply.push((resolved_ref, manifest));
    }

    if bundles_to_apply.is_empty() {
        println!("No bundles to {}.", action);
        return Ok(());
    }

    eprintln!(
        "[{}] {} {} bundle(s) onto {}…",
        action,
        if args.dry_run { "diffing" } else { "applying" },
        bundles_to_apply.len(),
        server_dir.display()
    );

    let changes = apply_bundles(
        &bundles_to_apply,
        &cache,
        &server_dir,
        args.dry_run,
        &project.server.deny_override,
        args.ignore_dangerous_override_attempts,
    )
    .await
    .context("applying bundle layers")?;

    println!();
    if args.dry_run {
        println!("Diff (what `bundle apply` would do, based on current registry state):");
    } else {
        println!("Apply result:");
    }
    println!();
    print_changes(&changes);

    if !args.dry_run && !changes.is_empty() {
        println!();
        println!(
            "✓ Applied {} bundle(s) to {}",
            bundles_to_apply.len(),
            server_dir.display()
        );
    }

    Ok(())
}

/// Pull a bundle manifest from the registry and store it in the local cache.
/// Returns the parsed `ImageManifest`.
async fn pull_bundle_to_cache(image_ref: &str, cache: &LocalCache) -> Result<ImageManifest> {
    // Bare names are local-only — there is no registry to fall back to.
    if !crate::registry::client::has_explicit_registry(image_ref) {
        anyhow::bail!(
            "local image '{}' is not in the cache.\n\
             Run `bundle build -t {}` then `bundle server pull` to populate it.",
            image_ref,
            image_ref,
        );
    }

    let client = McpmRegistryClient::new();

    let (manifest, digest) = client
        .pull_manifest(image_ref)
        .await
        .with_context(|| format!("pulling manifest from {}", image_ref))?;

    let manifest_json = manifest
        .to_string()
        .context("serialising pulled manifest")?
        .into_bytes();

    cache
        .store_manifest(image_ref, &manifest_json, &digest)
        .with_context(|| format!("caching manifest for {}", image_ref))?;

    // Also pull all layer blobs.
    client
        .fetch_layers_to_cache(image_ref, &manifest, cache)
        .await
        .with_context(|| format!("fetching layer blobs for {}", image_ref))?;

    Ok(manifest)
}

/// Pull any layer blobs that are missing from the cache for an already-loaded
/// manifest.
async fn pull_layers_to_cache(
    image_ref: &str,
    manifest: &ImageManifest,
    cache: &LocalCache,
) -> Result<()> {
    // Bare names are local-only.
    if !crate::registry::client::has_explicit_registry(image_ref) {
        anyhow::bail!(
            "layer blobs for local image '{}' are missing from the cache.\n\
             Re-run `bundle build -t {}` to rebuild, then `bundle server pull`.",
            image_ref,
            image_ref,
        );
    }

    let client = McpmRegistryClient::new();
    client
        .fetch_layers_to_cache(image_ref, manifest, cache)
        .await
        .with_context(|| format!("fetching layer blobs for {}", image_ref))?;
    Ok(())
}
