use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::bundle::build::build;
use crate::registry::client::{parse_ref, require_explicit_registry, McpmRegistryClient};
use crate::registry::types::LocalCache;

macro_rules! log {
    ($($t:tt)*) => { crate::progress!("build", $($t)*) };
}

pub struct BuildArgs {
    pub build_args: Vec<(String, String)>,
    pub tags: Vec<String>,
    pub context: Option<PathBuf>,
    pub bundlefile: Option<PathBuf>,
}

pub async fn run(args: BuildArgs) -> Result<()> {
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
             Run `bundle init` to scaffold one, or pass a build context directory:\
             \n    bundle build .\
             \n    bundle build path/to/my-plugin/",
            bundlefile_path.display()
        );
    }

    log!("Bundlefile: {}", bundlefile_path.display());

    // Validate all tags before spending time building.
    for tag in &args.tags {
        require_explicit_registry(tag)
            .with_context(|| format!("invalid tag '{}' for -t/--tag", tag))?;
        parse_ref(tag).with_context(|| format!("invalid tag: '{}'", tag))?;
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
                    log!("  manifest: {}", url);
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

fn short(digest: &str) -> String {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    format!("sha256:{}", &hex[..hex.len().min(12)])
}
