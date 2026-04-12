//! `bundle inspect` — display metadata for a bundle image from any source.
//!
//! Three source forms are supported:
//!
//! | Reference | Source |
//! |---|---|
//! | `ghcr.io/org/plugin:tag` | OCI registry (network) |
//! | `oci:./path` or `oci:/abs` | Local OCI Image Layout directory |
//! | `myplugin:latest` | Local cache (tagged with `bundle build -t`) |

use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use oci_spec::image::{ImageConfiguration, ImageIndex};

use crate::bundle::annotations;
use crate::registry::client::{has_explicit_registry, McpmRegistryClient};
use crate::registry::types::{ImageManifest, LocalCache};

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run(image: String) -> Result<()> {
    if let Some(path_str) = image.strip_prefix("oci:") {
        inspect_oci_dir(&image, Path::new(path_str))
    } else if has_explicit_registry(&image) {
        inspect_remote(&image).await
    } else {
        if !image.contains(':') {
            bail!(
                "local tag '{}' must include a name and a tag separated by ':', \
                 e.g. '{}:latest'",
                image,
                image,
            );
        }
        inspect_local_tag(&image)
    }
}

// ── Source-specific loaders ───────────────────────────────────────────────────

fn inspect_local_tag(tag: &str) -> Result<()> {
    let cache = LocalCache::open().context("opening local cache")?;

    if !cache.has_manifest(tag) {
        bail!(
            "local tag '{}' not found in cache.\n\
             Run `bundle build -t {}` to create it, or use a fully-qualified registry reference.",
            tag,
            tag,
        );
    }

    let (manifest_json, digest) = cache
        .load_manifest(tag)
        .with_context(|| format!("loading manifest for '{}'", tag))?;

    let manifest =
        ImageManifest::from_reader(manifest_json.as_slice()).context("parsing manifest")?;

    let config_digest = manifest.config().digest().to_string();
    let config_bytes = cache
        .load_blob(&config_digest)
        .with_context(|| format!("loading config blob {}", config_digest))?;

    let config = ImageConfiguration::from_reader(config_bytes.as_slice())
        .context("parsing image configuration")?;

    print_image_info(tag, "local cache", &digest, &manifest, &config);
    Ok(())
}

fn inspect_oci_dir(original_ref: &str, dir: &Path) -> Result<()> {
    if !dir.exists() {
        bail!(
            "OCI layout directory not found: {}\n\
             Hint: run `bundle build -t image:tag` to create one.",
            dir.display()
        );
    }

    let index_bytes = fs::read(dir.join("index.json"))
        .with_context(|| format!("reading index.json in {}", dir.display()))?;
    let index = ImageIndex::from_reader(index_bytes.as_slice()).context("parsing index.json")?;

    let manifests = index.manifests();
    if manifests.is_empty() {
        bail!("index.json in {} contains no manifests", dir.display());
    }

    let manifest_desc = &manifests[0];
    let manifest_digest = manifest_desc.digest().to_string();
    let manifest_hex = manifest_digest
        .strip_prefix("sha256:")
        .unwrap_or(&manifest_digest);

    let manifest_bytes = fs::read(dir.join("blobs/sha256").join(manifest_hex))
        .with_context(|| format!("reading manifest blob {}", manifest_digest))?;
    let manifest =
        ImageManifest::from_reader(manifest_bytes.as_slice()).context("parsing manifest")?;

    let config_digest = manifest.config().digest().to_string();
    let config_hex = config_digest
        .strip_prefix("sha256:")
        .unwrap_or(&config_digest);

    let config_bytes = fs::read(dir.join("blobs/sha256").join(config_hex))
        .with_context(|| format!("reading config blob {}", config_digest))?;
    let config = ImageConfiguration::from_reader(config_bytes.as_slice())
        .context("parsing image configuration")?;

    print_image_info(
        original_ref,
        "oci layout",
        &manifest_digest,
        &manifest,
        &config,
    );
    Ok(())
}

async fn inspect_remote(image_ref: &str) -> Result<()> {
    let client = McpmRegistryClient::new();

    let (manifest, digest) = client
        .pull_manifest(image_ref)
        .await
        .with_context(|| format!("pulling manifest for {}", image_ref))?;

    let config_bytes = client
        .pull_blob_bytes(image_ref, manifest.config())
        .await
        .context("fetching image config")?;

    let config = ImageConfiguration::from_reader(config_bytes.as_slice())
        .context("parsing image configuration")?;

    print_image_info(image_ref, "registry", &digest, &manifest, &config);
    Ok(())
}

// ── Display ───────────────────────────────────────────────────────────────────

fn print_image_info(
    reference: &str,
    source: &str,
    digest: &str,
    manifest: &ImageManifest,
    config: &ImageConfiguration,
) {
    println!("Image:    {reference}");
    println!("Digest:   {digest}");
    println!("Source:   {source}");

    // Platform
    let os = serde_json::to_string(config.os())
        .map(|s| s.trim_matches('"').to_string())
        .unwrap_or_else(|_| "unknown".into());
    let arch = serde_json::to_string(config.architecture())
        .map(|s| s.trim_matches('"').to_string())
        .unwrap_or_else(|_| "unknown".into());
    println!();
    println!("Platform: {os} / {arch}");

    // Layers
    let layers = manifest.layers();
    println!();
    println!("Layers ({}):", layers.len());
    for (i, layer) in layers.iter().enumerate() {
        println!(
            "  [{}/{}]  {}  {}",
            i + 1,
            layers.len(),
            short(layer.digest().as_ref()),
            format_bytes(layer.size()),
        );
    }

    // Labels from the OCI image config
    if let Some(cfg) = config.config() {
        if let Some(labels) = cfg.labels() {
            if !labels.is_empty() {
                println!();
                println!("Labels:");
                let mut sorted: Vec<(&String, &String)> = labels.iter().collect();
                sorted.sort_by_key(|(k, _)| k.as_str());
                for (k, v) in &sorted {
                    println!("  {k} = {v}");
                }
            }
        }
    }

    // Bundle-specific managed-config annotation
    if let Some(anns) = manifest.annotations() {
        if let Some(encoded) = anns.get(annotations::MANAGED_KEYS_ANNOTATION) {
            if let Ok(keys_map) = annotations::decode(encoded) {
                if !keys_map.is_empty() {
                    println!();
                    println!("Managed configs:");
                    let mut paths: Vec<&String> = keys_map.keys().collect();
                    paths.sort_unstable();
                    for path in paths {
                        let keys = &keys_map[path];
                        println!("  {path}");
                        println!("    → {}", keys.join(", "));
                    }
                }
            }
        }
    }

    println!();
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn short(digest: &str) -> String {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    format!("sha256:{}", &hex[..hex.len().min(12)])
}

fn format_bytes(bytes: u64) -> String {
    const MIB: f64 = 1_048_576.0;
    const KIB: f64 = 1_024.0;
    let b = bytes as f64;
    if b >= MIB {
        format!("{:.2} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}
