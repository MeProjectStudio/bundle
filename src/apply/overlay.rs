//! Overlay — unpack cached bundle layers onto a live server directory.
//!
//! This module is the core of `bundle apply` and `bundle diff`.
//!
//! ## Layer extraction
//!
//! Every path in a layer tar archive is server-root-relative and is extracted
//! verbatim.  The Bundlefile author declares the destination for each
//! `PLUGIN` and `ADD` directive, so mcpm never hard-codes `plugins/` or
//! `mods/` — a mod jar declared as `mods/Sodium.jar` will land at
//! `<server-root>/mods/Sodium.jar` automatically.
//!
//! ## Config merge strategy
//!
//! For files listed in the `bundle.managed-keys` annotation:
//!   - Parse both the on-disk file and the incoming bundle file.
//!   - For each *managed key*: take the bundle's value.
//!   - For all other keys: keep the user's on-disk value.
//!   - Write the merged result.
//!
//! Files with unrecognised extensions (`.jar`, `.so`, …) are always
//! overwritten with the bundle version.
//!
//! Files whose extension *is* recognised as a config format but that have
//! **no** managed-key entry in the annotation are also always overwritten
//! (the bundle author chose not to declare any managed keys for them).
//!
//! ## Dry-run
//!
//! Pass `dry_run = true` to report what *would* change without touching disk.
//!
//! ## No remapping
//!
//! The old `bundles/jars/ → plugins/` remapping has been removed.  Layers are
//! extracted exactly as packed; the declared `dest` path in the Bundlefile is
//! what ends up on disk.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use oci_spec::image::ImageManifest;

use crate::apply::merge::{detect_format, merge_config};
use crate::bundle::annotations::{from_manifest_annotations, ManagedKeys};
use crate::registry::types::LocalCache;


/// A single file-level change produced by [`apply_bundles`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Server-root-relative path (e.g. `plugins/EssentialsX.jar`).
    pub path: String,
    /// What kind of change this is.
    pub kind: ChangeKind,
}

/// The kind of change a file went through during apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// The file did not exist on disk and was created from the bundle.
    Created,
    /// The file existed and was completely overwritten by the bundle version.
    Overwritten,
    /// The file existed and was merged (managed keys updated, rest kept).
    Merged,
    /// The file would have been created (dry-run only).
    WouldCreate,
    /// The file would have been overwritten (dry-run only).
    WouldOverwrite,
    /// The file would have been merged (dry-run only).
    WouldMerge,
    /// The file was deleted by an OCI whiteout entry.
    Deleted,
    /// The file would have been deleted (dry-run only).
    WouldDelete,
}

impl std::fmt::Display for ChangeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChangeKind::Created => write!(f, "created"),
            ChangeKind::Overwritten => write!(f, "overwritten"),
            ChangeKind::Merged => write!(f, "merged"),
            ChangeKind::WouldCreate => write!(f, "would create"),
            ChangeKind::WouldOverwrite => write!(f, "would overwrite"),
            ChangeKind::WouldMerge => write!(f, "would merge"),
            ChangeKind::Deleted => write!(f, "deleted"),
            ChangeKind::WouldDelete => write!(f, "would delete"),
        }
    }
}

impl ChangeKind {
    /// Return `true` for dry-run change kinds.
    #[allow(dead_code)]
    pub fn is_dry_run(self) -> bool {
        matches!(
            self,
            ChangeKind::WouldCreate
                | ChangeKind::WouldOverwrite
                | ChangeKind::WouldMerge
                | ChangeKind::WouldDelete
        )
    }
}


/// Apply (or diff) one or more bundles onto `server_dir`.
///
/// `bundles` is a list of `(image_ref, manifest)` pairs taken from `bundle.lock`
/// and the local cache.  Bundles are applied in order; later bundles can
/// overwrite files laid down by earlier ones.
///
/// If `dry_run` is `true` the function only reports what *would* change —
/// nothing is written to disk.
///
/// Returns a list of [`FileChange`] values describing every file that was
/// (or would be) modified.
pub async fn apply_bundles(
    bundles: &[(String, ImageManifest)],
    cache: &LocalCache,
    server_dir: &Path,
    dry_run: bool,
) -> Result<Vec<FileChange>> {
    let mut all_changes: Vec<FileChange> = Vec::new();

    for (image_ref, manifest) in bundles {
        eprintln!(
            "[apply] {} bundle: {}",
            if dry_run { "diffing" } else { "applying" },
            image_ref
        );

        // Decode the bundle.managed-keys annotation from this manifest.
        let managed_keys = from_manifest_annotations(manifest.annotations())
            .with_context(|| format!("decoding managed-keys annotation for {}", image_ref))?;

        // Apply layers in order.
        for (layer_idx, descriptor) in manifest.layers().iter().enumerate() {
            let digest = descriptor.digest().to_string();
            eprintln!(
                "[apply]   layer {}/{}: {}",
                layer_idx + 1,
                manifest.layers().len(),
                short_digest(&digest)
            );

            let compressed = cache.load_blob(&digest).with_context(|| {
                format!(
                    "loading layer blob {} for {} (run `bundle pull` first?)",
                    digest, image_ref
                )
            })?;

            let changes = apply_layer(&compressed, &managed_keys, server_dir, dry_run)
                .await
                .with_context(|| format!("applying layer {} of {}", layer_idx + 1, image_ref))?;

            all_changes.extend(changes);
        }
    }

    Ok(all_changes)
}


/// Apply a single gzip-compressed layer to `server_dir`.
///
/// All paths in the layer are server-root-relative and are extracted verbatim;
/// no remapping is performed.
async fn apply_layer(
    compressed: &[u8],
    managed_keys: &ManagedKeys,
    server_dir: &Path,
    dry_run: bool,
) -> Result<Vec<FileChange>> {
    use flate2::read::GzDecoder;
    use std::io::Read;
    use tar::Archive;

    let mut decoder = GzDecoder::new(compressed);
    let mut tar_bytes: Vec<u8> = Vec::new();
    decoder
        .read_to_end(&mut tar_bytes)
        .context("decompressing layer blob")?;

    let mut changes: Vec<FileChange> = Vec::new();
    let mut archive = Archive::new(tar_bytes.as_slice());

    for entry_result in archive.entries().context("iterating layer tar entries")? {
        let mut entry = entry_result.context("reading tar entry")?;
        let tar_path = entry
            .path()
            .context("reading tar entry path")?
            .to_path_buf();

        // All paths in the layer are already server-root-relative (as declared
        // in the Bundlefile PLUGIN dest / ADD dest).  Extract verbatim.
        let server_rel = strip_leading_slash(&tar_path);
        let server_rel_str = server_rel.to_string_lossy().to_string();

        if entry.header().entry_type() == tar::EntryType::Directory {
            if !dry_run {
                let dest = server_dir.join(&server_rel);
                std::fs::create_dir_all(&dest)
                    .with_context(|| format!("creating directory: {}", dest.display()))?;
            }
            continue;
        }

        // --- OCI whiteout entries ---
        if let Some(file_name) = server_rel.file_name() {
            let fname = file_name.to_string_lossy();

            if fname == ".wh..wh..opq" {
                // Opaque whiteout: delete the entire parent directory's contents.
                let parent = server_rel.parent().unwrap_or(Path::new(""));
                let dest_parent = server_dir.join(parent);
                if dest_parent.is_dir() {
                    let deleted = delete_directory_contents(&dest_parent, dry_run)?;
                    for p in deleted {
                        let kind = if dry_run {
                            ChangeKind::WouldDelete
                        } else {
                            ChangeKind::Deleted
                        };
                        changes.push(FileChange { path: p, kind });
                    }
                }
                continue;
            }

            if let Some(real_name) = fname.strip_prefix(".wh.") {
                // Regular whiteout: delete a specific file.
                let target = server_rel.parent().unwrap_or(Path::new("")).join(real_name);
                let dest = server_dir.join(&target);
                let target_str = target.to_string_lossy().to_string();

                if dest.exists() {
                    if !dry_run {
                        if dest.is_dir() {
                            std::fs::remove_dir_all(&dest).with_context(|| {
                                format!("whiteout: removing dir {}", dest.display())
                            })?;
                        } else {
                            std::fs::remove_file(&dest).with_context(|| {
                                format!("whiteout: removing file {}", dest.display())
                            })?;
                        }
                        changes.push(FileChange {
                            path: target_str,
                            kind: ChangeKind::Deleted,
                        });
                    } else {
                        changes.push(FileChange {
                            path: target_str,
                            kind: ChangeKind::WouldDelete,
                        });
                    }
                }
                continue;
            }
        }

        // --- Regular file ---
        let mut data: Vec<u8> = Vec::new();
        entry
            .read_to_end(&mut data)
            .with_context(|| format!("reading tar entry '{}'", server_rel_str))?;

        let mode = entry.header().mode().unwrap_or(0o644);
        let dest_path = server_dir.join(&server_rel);
        let existed_before = dest_path.exists();

        let kind = if dry_run {
            determine_dry_run_kind(
                &dest_path,
                &data,
                &server_rel_str,
                managed_keys,
                existed_before,
            )
        } else {
            apply_file(
                &dest_path,
                &data,
                &server_rel_str,
                managed_keys,
                mode,
                existed_before,
            )
            .with_context(|| format!("applying file '{}'", server_rel_str))?
        };

        changes.push(FileChange {
            path: server_rel_str,
            kind,
        });
    }

    Ok(changes)
}


/// Apply a single file to `dest_path`, using config merge if appropriate.
///
/// Returns the [`ChangeKind`] that describes what happened.
fn apply_file(
    dest_path: &Path,
    bundle_data: &[u8],
    server_rel: &str,
    managed_keys: &ManagedKeys,
    mode: u32,
    existed_before: bool,
) -> Result<ChangeKind> {
    // Ensure parent directories exist.
    if let Some(parent) = dest_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent directory: {}", parent.display()))?;
    }

    // Look up managed keys for this config path.
    let config_managed: Option<&Vec<String>> = managed_keys.get(server_rel);

    let final_data: Vec<u8>;
    let kind: ChangeKind;

    if existed_before {
        if let Some(keys) = config_managed {
            // Config file with managed keys: attempt a merge.
            let on_disk = std::fs::read(dest_path).with_context(|| {
                format!("reading on-disk config for merge: {}", dest_path.display())
            })?;

            match merge_config(&on_disk, bundle_data, keys, Path::new(server_rel))
                .with_context(|| format!("merging config file '{}'", server_rel))?
            {
                Some(merged) => {
                    final_data = merged;
                    kind = ChangeKind::Merged;
                }
                None => {
                    // Unrecognised format — overwrite.
                    final_data = bundle_data.to_vec();
                    kind = ChangeKind::Overwritten;
                }
            }
        } else {
            // No managed keys — always overwrite.
            final_data = bundle_data.to_vec();
            kind = ChangeKind::Overwritten;
        }
    } else {
        // File doesn't exist yet — create it.
        final_data = bundle_data.to_vec();
        kind = ChangeKind::Created;
    }

    write_file(dest_path, &final_data, mode)?;
    Ok(kind)
}

/// Determine what *would* happen for a file in dry-run mode, without writing.
fn determine_dry_run_kind(
    _dest_path: &Path,
    _bundle_data: &[u8],
    server_rel: &str,
    managed_keys: &ManagedKeys,
    existed_before: bool,
) -> ChangeKind {
    if !existed_before {
        return ChangeKind::WouldCreate;
    }

    // File exists — would we merge or overwrite?
    if let Some(keys) = managed_keys.get(server_rel) {
        if !keys.is_empty()
            && detect_format(Path::new(server_rel)).is_some() {
                return ChangeKind::WouldMerge;
            }
    }

    ChangeKind::WouldOverwrite
}


/// Delete the *contents* of `dir` (not the directory itself) and return the
/// list of server-root-relative paths that were (or would be) deleted.
fn delete_directory_contents(dir: &Path, dry_run: bool) -> Result<Vec<String>> {
    let mut deleted = Vec::new();

    for entry in walkdir::WalkDir::new(dir).min_depth(1).into_iter() {
        let entry = entry
            .with_context(|| format!("walking directory for opaque whiteout: {}", dir.display()))?;
        let path = entry.path();
        let rel = path
            .strip_prefix(dir)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

        if !dry_run
            && (path.is_file() || path.is_symlink()) {
                std::fs::remove_file(path)
                    .with_context(|| format!("removing file: {}", path.display()))?;
            }
            // Directories are removed after their contents (WalkDir visits
            // depth-first by default, leaves before parents).

        if entry.file_type().is_file() || entry.file_type().is_symlink() {
            deleted.push(rel);
        }
    }

    // Remove empty subdirectories (only when not dry-run).
    if !dry_run {
        for entry in walkdir::WalkDir::new(dir)
            .min_depth(1)
            .contents_first(true)
            .into_iter()
            .flatten()
        {
            if entry.file_type().is_dir() {
                let _ = std::fs::remove_dir(entry.path());
            }
        }
    }

    Ok(deleted)
}


/// Pretty-print a list of changes to stdout in a human-readable format.
///
/// Used by both `bundle apply` and `bundle diff`.
pub fn print_changes(changes: &[FileChange]) {
    if changes.is_empty() {
        println!("No changes.");
        return;
    }

    let mut created = 0usize;
    let mut overwritten = 0usize;
    let mut merged = 0usize;
    let mut deleted = 0usize;

    for change in changes {
        let prefix = match change.kind {
            ChangeKind::Created | ChangeKind::WouldCreate => {
                created += 1;
                "+"
            }
            ChangeKind::Overwritten | ChangeKind::WouldOverwrite => {
                overwritten += 1;
                "~"
            }
            ChangeKind::Merged | ChangeKind::WouldMerge => {
                merged += 1;
                "M"
            }
            ChangeKind::Deleted | ChangeKind::WouldDelete => {
                deleted += 1;
                "-"
            }
        };
        println!("  {} {}", prefix, change.path);
    }

    println!();
    println!(
        "  {} created, {} overwritten, {} merged, {} deleted",
        created, overwritten, merged, deleted
    );
}


/// Strip a leading `/` from a path.
fn strip_leading_slash(path: &Path) -> PathBuf {
    path.strip_prefix("/").unwrap_or(path).to_path_buf()
}

/// Write `data` to `path`, setting Unix permissions if on a Unix system.
fn write_file(path: &Path, data: &[u8], _mode: u32) -> Result<()> {
    std::fs::write(path, data).with_context(|| format!("writing file: {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(_mode & 0o777);
        std::fs::set_permissions(path, perms)
            .with_context(|| format!("setting permissions on: {}", path.display()))?;
    }

    Ok(())
}

/// Return a short human-readable form of a digest.
fn short_digest(digest: &str) -> String {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    format!("sha256:{}", &hex[..hex.len().min(12)])
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::layer::{pack_layer, LayerEntry};
    use std::collections::HashMap;
    use tempfile::TempDir;

    // Build a minimal OciImageManifest with one layer whose blob is in `blobs`.
    fn make_manifest_with_blobs(
        layers: Vec<(String, Vec<u8>)>, // (digest, compressed_data)
        managed: ManagedKeys,
    ) -> (ImageManifest, HashMap<String, Vec<u8>>) {
        use crate::bundle::annotations::{encode, MANAGED_KEYS_ANNOTATION};
        use crate::registry::types::{Descriptor, ImageManifestBuilder, MediaType, SCHEMA_VERSION};

        let annotations: Option<HashMap<String, String>> = if managed.is_empty() {
            None
        } else {
            let mut ann = HashMap::new();
            ann.insert(
                MANAGED_KEYS_ANNOTATION.to_string(),
                encode(&managed).unwrap(),
            );
            Some(ann)
        };

        let layer_descriptors: Vec<Descriptor> = layers
            .iter()
            .map(|(digest, data)| {
                use std::str::FromStr;
                let hex = digest.strip_prefix("sha256:").unwrap_or(digest.as_str());
                let sha256 =
                    oci_spec::image::Sha256Digest::from_str(hex).expect("valid sha256 in test");
                Descriptor::new(MediaType::ImageLayerGzip, data.len() as u64, sha256)
            })
            .collect();

        let blobs: HashMap<String, Vec<u8>> = layers.into_iter().collect();

        // Use the sha256 of empty bytes as a placeholder config digest.
        let config_desc = {
            use std::str::FromStr;
            let sha256 = oci_spec::image::Sha256Digest::from_str(
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            )
            .expect("valid sha256");
            Descriptor::new(MediaType::ImageConfig, 0u64, sha256)
        };

        let mut builder = ImageManifestBuilder::default()
            .schema_version(SCHEMA_VERSION)
            .media_type(MediaType::ImageManifest)
            .config(config_desc)
            .layers(layer_descriptors);
        if let Some(ann) = annotations {
            builder = builder.annotations(ann);
        }
        let manifest = builder.build().expect("build test manifest");

        (manifest, blobs)
    }

    /// Pack a jars layer where each tuple is `(dest_path, bytes)`.
    /// The dest path is exactly what will appear on disk after apply
    /// (e.g. `"plugins/Foo.jar"` or `"mods/Sodium.jar"`).
    fn pack_jars_layer(jars: Vec<(&str, &[u8])>) -> (String, Vec<u8>) {
        #[allow(unused)]
        let entries: Vec<LayerEntry> = jars
            .into_iter()
            .map(|(dest, data)| LayerEntry::file(dest, data.to_vec()))
            .collect();
        let packed = pack_layer(&entries).unwrap();
        (packed.digest, packed.compressed)
    }

    fn pack_files_layer(files: Vec<(&str, &[u8])>) -> (String, Vec<u8>) {
        let entries: Vec<LayerEntry> = files
            .into_iter()
            .map(|(path, data)| LayerEntry::file(path, data.to_vec()))
            .collect();
        let packed = pack_layer(&entries).unwrap();
        (packed.digest, packed.compressed)
    }


    /// The dest path in the layer is used verbatim — plugins go to plugins/,
    /// mods go to mods/, whatever the Bundlefile author declared.
    #[tokio::test]
    async fn jars_layer_dest_path_used_verbatim() {
        let server_dir = TempDir::new().unwrap();
        // Bundlefile author chose plugins/ for one jar and mods/ for another.
        let (digest, compressed) = pack_jars_layer(vec![
            ("plugins/EssentialsX-2.20.1.jar", b"jar-bytes-essentials"),
            ("mods/Sodium-0.5.jar", b"jar-bytes-sodium"),
        ]);

        let (manifest, blobs) =
            make_manifest_with_blobs(vec![(digest.clone(), compressed)], ManagedKeys::new());

        let cache_dir = TempDir::new().unwrap();
        let cache = LocalCache::open_at(cache_dir.path()).unwrap();
        for data in blobs.values() {
            cache.store_blob(data).unwrap();
        }

        let changes = apply_bundles(
            &[("ghcr.io/test/bundle:v1".to_string(), manifest)],
            &cache,
            server_dir.path(),
            false,
        )
        .await
        .unwrap();

        // Each jar lands at exactly the path the Bundlefile declared.
        assert!(
            server_dir
                .path()
                .join("plugins/EssentialsX-2.20.1.jar")
                .exists(),
            "essentials jar should be at plugins/"
        );
        assert!(
            server_dir.path().join("mods/Sodium-0.5.jar").exists(),
            "sodium jar should be at mods/"
        );
        // No stray bundles/ directory.
        assert!(!server_dir.path().join("bundles").exists());

        assert_eq!(changes.len(), 2);
        assert!(changes.iter().all(|c| c.kind == ChangeKind::Created));
    }

    /// Verify that changing the dest to mods/ works end-to-end with no
    /// plugins/ directory being created as a side-effect.
    #[tokio::test]
    async fn mods_only_bundle_no_plugins_dir() {
        let server_dir = TempDir::new().unwrap();
        let (digest, compressed) = pack_jars_layer(vec![("mods/Sodium-0.5.jar", b"sodium-bytes")]);

        let (manifest, blobs) =
            make_manifest_with_blobs(vec![(digest, compressed)], ManagedKeys::new());

        let cache_dir = TempDir::new().unwrap();
        let cache = LocalCache::open_at(cache_dir.path()).unwrap();
        for data in blobs.values() {
            cache.store_blob(data).unwrap();
        }

        apply_bundles(
            &[("ghcr.io/test/bundle:v1".to_string(), manifest)],
            &cache,
            server_dir.path(),
            false,
        )
        .await
        .unwrap();

        assert!(server_dir.path().join("mods/Sodium-0.5.jar").exists());
        assert!(
            !server_dir.path().join("plugins").exists(),
            "plugins/ should not be created for a mods-only bundle"
        );
    }


    #[tokio::test]
    async fn config_file_merged_with_managed_keys() {
        let server_dir = TempDir::new().unwrap();

        // Pre-place an on-disk config file.
        let config_path = server_dir.path().join("plugins/Essentials/config.yml");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            b"homes:\n  max-homes: 3\n  bed-respawn: false\nother: user-value\n",
        )
        .unwrap();

        // Bundle version of the config.
        let (digest, compressed) = pack_files_layer(vec![(
            "plugins/Essentials/config.yml",
            b"homes:\n  max-homes: 10\n  bed-respawn: true\nother: bundle-value\n",
        )]);

        let mut managed = ManagedKeys::new();
        managed.insert(
            "plugins/Essentials/config.yml".to_string(),
            vec!["homes.max-homes".to_string()],
        );

        let (manifest, blobs) = make_manifest_with_blobs(vec![(digest, compressed)], managed);

        let cache_dir = TempDir::new().unwrap();
        let cache = LocalCache::open_at(cache_dir.path()).unwrap();
        for data in blobs.values() {
            cache.store_blob(data).unwrap();
        }

        let changes = apply_bundles(
            &[("ghcr.io/test/bundle:v1".to_string(), manifest)],
            &cache,
            server_dir.path(),
            false,
        )
        .await
        .unwrap();

        // The config should have been merged.
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Merged);

        let merged: serde_yaml::Value =
            serde_yaml::from_reader(std::fs::File::open(&config_path).unwrap()).unwrap();

        // Managed key: bundle wins (10).
        assert_eq!(merged["homes"]["max-homes"], serde_yaml::Value::from(10));
        // Non-managed keys: disk values preserved.
        assert_eq!(
            merged["homes"]["bed-respawn"],
            serde_yaml::Value::from(false)
        );
        assert_eq!(
            merged["other"],
            serde_yaml::Value::String("user-value".to_string())
        );
    }


    #[tokio::test]
    async fn jar_file_always_overwritten() {
        let server_dir = TempDir::new().unwrap();

        // Pre-place an old jar.
        let jar_path = server_dir.path().join("plugins/OldPlugin.jar");
        std::fs::create_dir_all(jar_path.parent().unwrap()).unwrap();
        std::fs::write(&jar_path, b"old-jar-bytes").unwrap();

        // Bundle has a new version of the same jar.
        let (digest, compressed) =
            pack_files_layer(vec![("plugins/OldPlugin.jar", b"new-jar-bytes")]);

        // No managed keys for jars — they are always overwritten.
        let (manifest, blobs) =
            make_manifest_with_blobs(vec![(digest, compressed)], ManagedKeys::new());

        let cache_dir = TempDir::new().unwrap();
        let cache = LocalCache::open_at(cache_dir.path()).unwrap();
        for data in blobs.values() {
            cache.store_blob(data).unwrap();
        }

        apply_bundles(
            &[("ghcr.io/test/bundle:v1".to_string(), manifest)],
            &cache,
            server_dir.path(),
            false,
        )
        .await
        .unwrap();

        let content = std::fs::read(&jar_path).unwrap();
        assert_eq!(content, b"new-jar-bytes");
    }


    #[tokio::test]
    async fn dry_run_does_not_write() {
        let server_dir = TempDir::new().unwrap();

        let (digest, compressed) =
            pack_files_layer(vec![("plugins/Test/config.yml", b"key: value\n")]);

        let (manifest, blobs) =
            make_manifest_with_blobs(vec![(digest, compressed)], ManagedKeys::new());

        let cache_dir = TempDir::new().unwrap();
        let cache = LocalCache::open_at(cache_dir.path()).unwrap();
        for data in blobs.values() {
            cache.store_blob(data).unwrap();
        }

        let changes = apply_bundles(
            &[("ghcr.io/test/bundle:v1".to_string(), manifest)],
            &cache,
            server_dir.path(),
            true, // dry_run = true
        )
        .await
        .unwrap();

        // Nothing should be on disk.
        assert!(!server_dir.path().join("plugins").exists());

        // But the change should be reported.
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::WouldCreate);
    }

    #[tokio::test]
    async fn dry_run_reports_would_merge_for_existing_config() {
        let server_dir = TempDir::new().unwrap();

        let config_path = server_dir.path().join("plugins/A/config.yml");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, b"key: disk\n").unwrap();

        let (digest, compressed) =
            pack_files_layer(vec![("plugins/A/config.yml", b"key: bundle\n")]);

        let mut managed = ManagedKeys::new();
        managed.insert("plugins/A/config.yml".to_string(), vec!["key".to_string()]);

        let (manifest, blobs) = make_manifest_with_blobs(vec![(digest, compressed)], managed);

        let cache_dir = TempDir::new().unwrap();
        let cache = LocalCache::open_at(cache_dir.path()).unwrap();
        for data in blobs.values() {
            cache.store_blob(data).unwrap();
        }

        let changes = apply_bundles(
            &[("ghcr.io/test/bundle:v1".to_string(), manifest)],
            &cache,
            server_dir.path(),
            true,
        )
        .await
        .unwrap();

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::WouldMerge);

        // On-disk content should be unchanged.
        assert_eq!(std::fs::read(&config_path).unwrap(), b"key: disk\n");
    }


    #[tokio::test]
    async fn multiple_bundles_applied_in_order() {
        let server_dir = TempDir::new().unwrap();

        // Bundle A writes file.txt with "content-a".
        let (digest_a, compressed_a) = pack_files_layer(vec![("file.txt", b"content-a")]);
        // Bundle B writes file.txt with "content-b" (should overwrite A).
        let (digest_b, compressed_b) = pack_files_layer(vec![("file.txt", b"content-b")]);

        let (manifest_a, blobs_a) =
            make_manifest_with_blobs(vec![(digest_a, compressed_a)], ManagedKeys::new());
        let (manifest_b, blobs_b) =
            make_manifest_with_blobs(vec![(digest_b, compressed_b)], ManagedKeys::new());

        let cache_dir = TempDir::new().unwrap();
        let cache = LocalCache::open_at(cache_dir.path()).unwrap();
        for (_d, data) in blobs_a.iter().chain(blobs_b.iter()) {
            cache.store_blob(data).unwrap();
        }

        apply_bundles(
            &[
                ("bundle-a:v1".to_string(), manifest_a),
                ("bundle-b:v1".to_string(), manifest_b),
            ],
            &cache,
            server_dir.path(),
            false,
        )
        .await
        .unwrap();

        // Bundle B should win.
        assert_eq!(
            std::fs::read(server_dir.path().join("file.txt")).unwrap(),
            b"content-b"
        );
    }


    #[test]
    fn print_changes_no_panic_on_empty() {
        print_changes(&[]);
    }

    #[test]
    fn print_changes_all_kinds() {
        let changes = vec![
            FileChange {
                path: "a.jar".to_string(),
                kind: ChangeKind::Created,
            },
            FileChange {
                path: "b.yml".to_string(),
                kind: ChangeKind::Merged,
            },
            FileChange {
                path: "c.jar".to_string(),
                kind: ChangeKind::Overwritten,
            },
            FileChange {
                path: "d.yml".to_string(),
                kind: ChangeKind::Deleted,
            },
        ];
        print_changes(&changes);
    }


    #[test]
    fn change_kind_is_dry_run() {
        assert!(ChangeKind::WouldCreate.is_dry_run());
        assert!(ChangeKind::WouldOverwrite.is_dry_run());
        assert!(ChangeKind::WouldMerge.is_dry_run());
        assert!(ChangeKind::WouldDelete.is_dry_run());
        assert!(!ChangeKind::Created.is_dry_run());
        assert!(!ChangeKind::Overwritten.is_dry_run());
        assert!(!ChangeKind::Merged.is_dry_run());
        assert!(!ChangeKind::Deleted.is_dry_run());
    }

    #[test]
    fn change_kind_display() {
        assert_eq!(ChangeKind::Created.to_string(), "created");
        assert_eq!(ChangeKind::WouldMerge.to_string(), "would merge");
    }
}
