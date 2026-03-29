//! `bundle build` — parse the Bundlefile, fetch/copy sources, pack OCI layers,
//! and write the resulting `LocalImage` to the local cache for `bundle push`.
//!
//! ## The OCI manifest IS the lock file
//!
//! `bundle build` does **not** read or write `bundle.lock`.  The output OCI
//! image manifest content-addresses every layer by sha256 digest, making it
//! a complete record of exactly what was built.
//!
//! For explicit URL verification at build time, use:
//!
//! ```text
//! ADD --checksum=sha256:<hex>  https://example.com/Plugin.jar  plugins/Plugin.jar
//! ```
//!
//! `bundle.lock` is only written by `bundle server pull` and only tracks
//! the server-side `image:tag → manifest-digest` mapping.
//!
//! ## Build context and tags
//!
//! Mirrors `docker build`:
//!
//! ```sh
//! bundle build .                                        # build from current dir
//! bundle build -t ghcr.io/me/plugin:latest .           # build + push one tag
//! bundle build -t ghcr.io/me/plugin:latest \
//!              -t ghcr.io/me/plugin:nightly .          # build + push two tags
//! bundle build --bundlefile path/to/Bundlefile         # explicit Bundlefile
//! ```
//!
//! The built image is always stored in `~/.cache/bundle/built/` regardless of
//! whether tags are provided — you can push to additional tags later with
//! `bundle push <IMAGE:TAG>`.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::bundle::build::build;
use crate::registry::client::{parse_ref, McpmRegistryClient};
use crate::registry::types::LocalCache;

// ── Arguments ─────────────────────────────────────────────────────────────────

/// Arguments accepted by `bundle build`.
#[derive(Debug, Clone)]
pub struct BuildArgs {
    /// `--build-arg KEY=VAL` pairs supplied on the command line.
    pub build_args: Vec<(String, String)>,

    /// Tags to push to after a successful build (`-t`/`--tag`, repeatable).
    ///
    /// When non-empty the built image is pushed to every listed tag using the
    /// same logic as `bundle push`.  The image is stored in the local cache
    /// regardless so you can push to more tags later.
    pub tags: Vec<String>,

    /// Build context directory.  The Bundlefile is looked up inside this
    /// directory.  `None` defaults to the current working directory.
    pub context: Option<PathBuf>,

    /// Explicit path to the Bundlefile, overriding context-based lookup.
    pub bundlefile: Option<PathBuf>,
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Run `bundle build`.
pub async fn run(args: BuildArgs) -> Result<()> {
    // ── Resolve Bundlefile path ───────────────────────────────────────────────
    // Priority:
    //   1. --bundlefile <FILE>  — explicit path to the Bundlefile
    //   2. <PATH>/Bundlefile    — Bundlefile inside the build context directory
    //   3. ./Bundlefile         — Bundlefile in the current working directory
    let bundlefile_path = if let Some(explicit) = args.bundlefile {
        explicit
    } else {
        let context = args
            .context
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        context.join("Bundlefile")
    };

    if !bundlefile_path.exists() {
        anyhow::bail!(
            "Bundlefile not found at {}.\n\
             \n  Run `bundle init` to scaffold one, or pass a build context directory:\
             \n    bundle build .\
             \n    bundle build path/to/my-plugin/",
            bundlefile_path.display()
        );
    }

    eprintln!("[build] using Bundlefile: {}", bundlefile_path.display());

    // ── Validate tags up front ────────────────────────────────────────────────
    // Check all tags are valid OCI references before we spend time building.
    for tag in &args.tags {
        parse_ref(tag).with_context(|| format!("invalid tag: '{}'", tag))?;
    }

    // ── Convert --build-arg pairs to a HashMap ────────────────────────────────
    let mut overrides: HashMap<String, String> = HashMap::new();
    for (k, v) in &args.build_args {
        overrides.insert(k.clone(), v.clone());
    }
    if !overrides.is_empty() {
        eprintln!("[build] build-arg overrides:");
        for (k, v) in &overrides {
            eprintln!("[build]   {}={}", k, v);
        }
    }

    // ── Build ─────────────────────────────────────────────────────────────────
    eprintln!("[build] starting build…");
    let image = build(&bundlefile_path, &overrides)
        .await
        .context("building bundle image")?;

    // ── Store in local cache ──────────────────────────────────────────────────
    let cache = LocalCache::open().context("opening local cache")?;
    cache
        .store_built_image(&image)
        .context("storing built image in local cache")?;

    // ── Summary ───────────────────────────────────────────────────────────────
    let total_size: u64 = image.new_blobs.values().map(|b| b.len() as u64).sum();

    println!();
    println!("✓ Build complete");
    println!("  layers      : {}", image.manifest.layers().len());
    println!(
        "  new blobs   : {} ({} bytes total)",
        image.new_blobs.len().saturating_sub(1),
        total_size
    );
    println!(
        "  config      : {}",
        short(&image.manifest.config().digest().to_string())
    );
    if let Some(ref ann) = image.manifest.annotations() {
        if let Some(mk) = ann.get(crate::bundle::annotations::MANAGED_KEYS_ANNOTATION) {
            if let Ok(keys) = crate::bundle::annotations::decode(mk) {
                println!("  managed configs : {} file(s)", keys.len());
            }
        }
    }

    // ── Push to all specified tags ────────────────────────────────────────────
    if args.tags.is_empty() {
        println!();
        println!("Run `bundle push <IMAGE:TAG>` to publish.");
    } else {
        println!();
        println!("Pushing to {} tag(s)…", args.tags.len());

        let client = McpmRegistryClient::new();
        let mut push_errors: Vec<(String, anyhow::Error)> = Vec::new();

        for tag in &args.tags {
            eprint!("  {} … ", tag);
            match client.push_image(tag, &image).await {
                Ok(url) => {
                    println!("✓");
                    eprintln!("    manifest: {}", url);
                }
                Err(e) => {
                    println!("✗");
                    push_errors.push((tag.clone(), e));
                }
            }
        }

        if !push_errors.is_empty() {
            println!();
            for (tag, err) in &push_errors {
                eprintln!("error pushing to '{}': {:#}", tag, err);
            }
            anyhow::bail!(
                "{} of {} tag(s) failed to push",
                push_errors.len(),
                args.tags.len()
            );
        }

        println!();
        println!("✓ Pushed to {} tag(s).", args.tags.len());
        println!("  Use `bundle push <IMAGE:TAG>` to push to additional tags.");
    }

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Return a short human-readable form of a digest (`"sha256:abcdef12…"`).
fn short(digest: &str) -> String {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    format!("sha256:{}", &hex[..hex.len().min(12)])
}
