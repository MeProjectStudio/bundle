use anyhow::{bail, Context, Result};

use crate::registry::client::{
    has_explicit_registry, parse_ref, require_explicit_registry, McpmRegistryClient,
};
use crate::registry::types::LocalCache;

macro_rules! log {
    ($($t:tt)*) => { crate::progress!("push", $($t)*) };
}

pub struct PushArgs {
    /// Registry destination (must have explicit registry hostname).
    pub image_ref: String,
    /// Local source tag (`bundle build -t NAME`). `None` → load from `built/` slot.
    pub local_tag: Option<String>,
}

pub async fn run(args: PushArgs) -> Result<()> {
    let image_ref = args.image_ref.trim();

    require_explicit_registry(image_ref)
        .with_context(|| format!("invalid push target: '{}'", image_ref))?;

    let reference = parse_ref(image_ref)
        .with_context(|| format!("invalid image reference: '{}'", image_ref))?;

    if reference.tag().is_none() && reference.digest().is_none() {
        bail!(
            "image reference '{}' has no tag or digest.\n\
             Provide an explicit tag, e.g. `{}:latest`.",
            image_ref,
            image_ref
        );
    }

    log!("target: {}", image_ref);

    let cache = LocalCache::open().context("opening local cache")?;
    let image = match &args.local_tag {
        Some(tag) => {
            if has_explicit_registry(tag) {
                anyhow::bail!(
                    "'{}' looks like a registry reference, not a local tag.\n\
                     Local source must be a bare name, e.g.:\n\
                     \n    bundle push {} {}",
                    tag,
                    tag.split('/').next_back().unwrap_or(tag),
                    args.image_ref,
                );
            }
            if !tag.contains(':') {
                anyhow::bail!(
                    "local tag '{}' must include a name and a tag separated by ':', \
                     e.g. '{}:latest'",
                    tag,
                    tag,
                );
            }
            log!("source: local tag '{}'", tag);
            cache
                .load_local_image_by_tag(tag)
                .with_context(|| format!("loading local tag '{}'", tag))?
        }
        None => cache
            .load_built_image()
            .context("loading built image from local cache (have you run `bundle build`?)")?,
    };

    log!(
        "{} layer(s), {} new blob(s)",
        image.manifest.layers().len(),
        image.new_blobs.len().saturating_sub(1),
    );
    log!(
        "config: {}",
        short(image.manifest.config().digest().as_ref())
    );

    for (i, layer) in image.manifest.layers().iter().enumerate() {
        let digest = layer.digest().to_string();
        let is_new = image.has_blob(&digest);
        log!(
            "  [{}/{}] {} ({} bytes){}",
            i + 1,
            image.manifest.layers().len(),
            short(&digest),
            layer.size(),
            if is_new { "" } else { " (inherited)" },
        );
    }

    log!("pushing…");

    let client = McpmRegistryClient::new();
    let manifest_url = client
        .push_image(image_ref, &image)
        .await
        .with_context(|| format!("pushing image to {}", image_ref))?;

    println!();
    println!("✓ Push complete");
    println!("  image   : {}", image_ref);
    println!("  manifest: {}", manifest_url);

    Ok(())
}

fn short(digest: &str) -> String {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    format!("sha256:{}", &hex[..hex.len().min(12)])
}
