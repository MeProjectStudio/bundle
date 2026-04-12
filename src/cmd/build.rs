use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::bundle::build::build;
use crate::registry::client::{has_explicit_registry, parse_ref, McpmRegistryClient};
use crate::registry::types::LocalCache;
use crate::util::digest::sha256_digest;

macro_rules! log {
    ($($t:tt)*) => { crate::progress!("build", $($t)*) };
}

pub struct BuildArgs {
    pub build_args: Vec<(String, String)>,
    pub tags: Vec<String>,
    pub context: Option<PathBuf>,
    /// Explicit path to the Bundlefile. Overrides `context`.
    pub file: Option<PathBuf>,
}

pub async fn run(args: BuildArgs) -> Result<()> {
    let bundlefile_path = if let Some(explicit) = args.file {
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
             Run `bundle init` to scaffold one, or pass a build context directory:\
             \n    bundle build .\
             \n    bundle build path/to/my-plugin/",
            bundlefile_path.display()
        );
    }

    log!("Bundlefile: {}", bundlefile_path.display());

    // Validate tags before spending time building.
    // Bare names (no registry hostname) are local-only — validate format only.
    // Registry refs are validated fully and will be pushed after the build.
    for tag in &args.tags {
        if has_explicit_registry(tag) {
            parse_ref(tag).with_context(|| format!("invalid tag: '{}'", tag))?;
        } else if tag.is_empty() {
            anyhow::bail!("tag must not be empty");
        } else if !tag.contains(':') {
            anyhow::bail!(
                "local tag '{}' must include a name and a tag separated by ':', \
                 e.g. '{}:latest'",
                tag,
                tag,
            );
        }
    }

    let mut overrides: HashMap<String, String> = HashMap::new();
    for (k, v) in &args.build_args {
        overrides.insert(k.clone(), v.clone());
    }
    if !overrides.is_empty() {
        log!("build-arg overrides:");
        for (k, v) in &overrides {
            log!("  {}={}", k, v);
        }
    }

    log!("starting build…");
    let image = build(&bundlefile_path, &overrides)
        .await
        .context("building bundle image")?;

    let cache = LocalCache::open().context("opening local cache")?;
    cache
        .store_built_image(&image)
        .context("storing built image in local cache")?;

    // ── Local tags ────────────────────────────────────────────────────────────
    // Store the manifest under each bare tag name so `bundle server pull` can
    // find it without a registry round-trip.
    let local_tags: Vec<&String> = args
        .tags
        .iter()
        .filter(|t| !has_explicit_registry(t))
        .collect();
    if !local_tags.is_empty() {
        let manifest_bytes = image
            .manifest
            .to_string()
            .context("serialising manifest for local tag")?
            .into_bytes();
        let manifest_digest = sha256_digest(&manifest_bytes);
        for tag in &local_tags {
            cache
                .store_manifest(tag, &manifest_bytes, &manifest_digest)
                .with_context(|| format!("storing local tag '{}'", tag))?;
        }
    }

    let total_size: u64 = image.new_blobs.values().map(|b| b.len() as u64).sum();

    println!();
    println!("✓ Build complete");
    println!("  layers    : {}", image.manifest.layers().len());
    println!(
        "  new blobs : {} ({} bytes total)",
        image.new_blobs.len().saturating_sub(1),
        total_size
    );
    println!(
        "  config    : {}",
        short(image.manifest.config().digest().as_ref())
    );
    if let Some(ref ann) = image.manifest.annotations() {
        if let Some(mk) = ann.get(crate::bundle::annotations::MANAGED_KEYS_ANNOTATION) {
            if let Ok(keys) = crate::bundle::annotations::decode(mk) {
                println!("  managed configs : {} file(s)", keys.len());
            }
        }
    }

    // ── Summary ───────────────────────────────────────────────────────────────
    let remote_tags: Vec<&String> = args
        .tags
        .iter()
        .filter(|t| has_explicit_registry(t))
        .collect();

    if !local_tags.is_empty() {
        println!();
        for tag in &local_tags {
            println!("  ✓ tagged locally: {}", tag);
        }
        println!("  Use `bundle server pull` to lock the digest in bundle.lock.");
    }

    if remote_tags.is_empty() && local_tags.is_empty() {
        println!();
        println!("Run `bundle push <IMAGE:TAG>` to publish to a registry.");
        println!("Run `bundle build -t <NAME>` to tag locally for `bundle server pull`.");
    } else if !remote_tags.is_empty() {
        println!();
        println!("Pushing to {} registry tag(s)…", remote_tags.len());

        let client = McpmRegistryClient::new();
        let mut push_errors: Vec<(String, anyhow::Error)> = Vec::new();

        for tag in &remote_tags {
            eprint!("  {} … ", tag);
            match client.push_image(tag, &image).await {
                Ok(url) => {
                    println!("✓");
                    log!("  manifest: {}", url);
                }
                Err(e) => {
                    println!("✗");
                    push_errors.push((tag.to_string(), e));
                }
            }
        }

        if !push_errors.is_empty() {
            println!();
            for (tag, err) in &push_errors {
                eprintln!("error pushing to '{}': {:#}", tag, err);
            }
            anyhow::bail!(
                "{} of {} registry tag(s) failed to push",
                push_errors.len(),
                remote_tags.len()
            );
        }

        println!();
        println!("✓ Pushed to {} registry tag(s).", remote_tags.len());
        println!("  Use `bundle push <IMAGE:TAG>` to push to additional tags.");
    }

    Ok(())
}

fn short(digest: &str) -> String {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    format!("sha256:{}", &hex[..hex.len().min(12)])
}
