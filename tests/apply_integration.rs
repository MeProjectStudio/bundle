//! Integration tests: build local OCI images from Bundlefiles, apply them to
//! a temporary server directory, and verify the resulting filesystem layout.
//!
//! Each test:
//!   1. Creates a temporary build context with fake jar / config files.
//!   2. Parses and builds a Bundlefile into a `LocalImage`.
//!   3. Stores the image in an isolated local cache under a bare tag.
//!   4. Calls `apply_bundles` against a fresh server directory.
//!   5. Asserts the expected files are (or are not) present.

mod common;

use std::collections::HashMap;
use std::path::Path;

use tempfile::TempDir;

use bundle::apply::overlay::apply_bundles;
use bundle::bundle::build::build;
use bundle::registry::types::{ImageManifest, LocalCache};
use bundle::util::digest::sha256_digest;

// ── Shared helper ─────────────────────────────────────────────────────────────

/// Build the Bundlefile at `ctx/Bundlefile`, store every blob in `cache`,
/// register the manifest under `tag`, and return the parsed manifest.
async fn build_and_store(ctx: &Path, tag: &str, cache: &LocalCache) -> ImageManifest {
    let bf_path = ctx.join("Bundlefile");
    let image = build(&bf_path, &HashMap::new())
        .await
        .unwrap_or_else(|e| panic!("build failed: {e:#}"));

    cache.store_built_image(&image).expect("store_built_image");

    let manifest_bytes = image
        .manifest
        .to_string()
        .expect("serialise manifest")
        .into_bytes();
    let digest = sha256_digest(&manifest_bytes);
    cache
        .store_manifest(tag, &manifest_bytes, &digest)
        .expect("store_manifest");

    image.manifest
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// COPY with a glob pattern strips the non-wildcard prefix so the jar lands
/// flat under `plugins/`, not nested inside `plugins/build/libs/`.
#[tokio::test]
async fn copy_glob_strips_source_prefix() {
    let ctx = TempDir::new().unwrap();
    common::write_file(
        ctx.path(),
        "build/libs/mcmetrics-exporter-velocity-0.5.0.jar",
        b"fake-jar",
    );
    common::bundlefile(
        ctx.path(),
        "FROM scratch\nCOPY build/libs/mcmetrics-exporter-velocity-*.jar plugins/\n",
    );

    let cache_dir = TempDir::new().unwrap();
    let cache = LocalCache::open_at(cache_dir.path()).expect("open cache");
    let manifest = build_and_store(ctx.path(), "mcmetrics:latest", &cache).await;

    let server_dir = TempDir::new().unwrap();
    apply_bundles(
        &[("mcmetrics:latest".to_string(), manifest)],
        &cache,
        server_dir.path(),
        false,
        &[],
        false,
    )
    .await
    .expect("apply_bundles");

    assert!(
        server_dir
            .path()
            .join("plugins/mcmetrics-exporter-velocity-0.5.0.jar")
            .exists(),
        "jar must land flat in plugins/ after prefix strip"
    );
    assert!(
        !server_dir
            .path()
            .join("plugins/build/libs/mcmetrics-exporter-velocity-0.5.0.jar")
            .exists(),
        "build/libs/ prefix must not be reproduced under plugins/"
    );
}

/// ADD with an explicit destination places the file at exactly that path and
/// preserves its content byte-for-byte.
#[tokio::test]
async fn add_local_jar_exact_destination_and_content() {
    let ctx = TempDir::new().unwrap();
    common::write_file(ctx.path(), "jars/luckperms-5.4.jar", b"lp-fake-content");
    common::bundlefile(
        ctx.path(),
        "FROM scratch\nADD ./jars/luckperms-5.4.jar plugins/luckperms-5.4.jar\n",
    );

    let cache_dir = TempDir::new().unwrap();
    let cache = LocalCache::open_at(cache_dir.path()).expect("open cache");
    let manifest = build_and_store(ctx.path(), "luckperms:latest", &cache).await;

    let server_dir = TempDir::new().unwrap();
    apply_bundles(
        &[("luckperms:latest".to_string(), manifest)],
        &cache,
        server_dir.path(),
        false,
        &[],
        false,
    )
    .await
    .expect("apply_bundles");

    let dest = server_dir.path().join("plugins/luckperms-5.4.jar");
    assert!(dest.exists(), "jar must be present at declared destination");
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        b"lp-fake-content",
        "jar content must be byte-for-byte identical to the source"
    );
}

/// Two independently built and tagged images are both applied to the same
/// server directory; all files from both images must be present.
#[tokio::test]
async fn two_images_both_applied_to_server_dir() {
    let cache_dir = TempDir::new().unwrap();
    let cache = LocalCache::open_at(cache_dir.path()).expect("open cache");

    // Image 1 — Essentials
    let ctx1 = TempDir::new().unwrap();
    common::write_file(ctx1.path(), "essentials.jar", b"ess-bytes");
    common::bundlefile(
        ctx1.path(),
        "FROM scratch\nADD ./essentials.jar plugins/essentials.jar\n",
    );
    let m1 = build_and_store(ctx1.path(), "essentials:latest", &cache).await;

    // Image 2 — Vault
    let ctx2 = TempDir::new().unwrap();
    common::write_file(ctx2.path(), "vault.jar", b"vault-bytes");
    common::bundlefile(
        ctx2.path(),
        "FROM scratch\nADD ./vault.jar plugins/vault.jar\n",
    );
    let m2 = build_and_store(ctx2.path(), "vault:latest", &cache).await;

    let server_dir = TempDir::new().unwrap();
    apply_bundles(
        &[
            ("essentials:latest".to_string(), m1),
            ("vault:latest".to_string(), m2),
        ],
        &cache,
        server_dir.path(),
        false,
        &[],
        false,
    )
    .await
    .expect("apply_bundles");

    assert!(
        server_dir.path().join("plugins/essentials.jar").exists(),
        "essentials.jar must be present"
    );
    assert!(
        server_dir.path().join("plugins/vault.jar").exists(),
        "vault.jar must be present"
    );
}

/// A dry run reports changes but writes nothing to disk.
#[tokio::test]
async fn dry_run_does_not_write_any_files() {
    let ctx = TempDir::new().unwrap();
    common::write_file(ctx.path(), "plugin.jar", b"bytes");
    common::bundlefile(
        ctx.path(),
        "FROM scratch\nADD ./plugin.jar plugins/plugin.jar\n",
    );

    let cache_dir = TempDir::new().unwrap();
    let cache = LocalCache::open_at(cache_dir.path()).expect("open cache");
    let manifest = build_and_store(ctx.path(), "plugin:latest", &cache).await;

    let server_dir = TempDir::new().unwrap();
    apply_bundles(
        &[("plugin:latest".to_string(), manifest)],
        &cache,
        server_dir.path(),
        true, // dry_run = true
        &[],
        false,
    )
    .await
    .expect("apply_bundles dry run");

    assert!(
        !server_dir.path().join("plugins/plugin.jar").exists(),
        "dry run must not write any files to the server directory"
    );
}

/// ARG substitution with the bare $VAR form works end-to-end: the variable
/// is expanded before the COPY glob is resolved, so the destination path
/// contains the substituted value.
#[tokio::test]
async fn arg_bare_dollar_substitution_in_copy_dest() {
    let ctx = TempDir::new().unwrap();
    common::write_file(ctx.path(), "plugin.jar", b"bytes");
    common::bundlefile(
        ctx.path(),
        "ARG DEST=plugins\nFROM scratch\nADD ./plugin.jar $DEST/plugin.jar\n",
    );

    let cache_dir = TempDir::new().unwrap();
    let cache = LocalCache::open_at(cache_dir.path()).expect("open cache");
    let manifest = build_and_store(ctx.path(), "argtest:latest", &cache).await;

    let server_dir = TempDir::new().unwrap();
    apply_bundles(
        &[("argtest:latest".to_string(), manifest)],
        &cache,
        server_dir.path(),
        false,
        &[],
        false,
    )
    .await
    .expect("apply_bundles");

    assert!(
        server_dir.path().join("plugins/plugin.jar").exists(),
        "ARG DEST=plugins substituted via bare $DEST must route jar to plugins/"
    );
}
