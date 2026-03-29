//! OCI layer creation (tar + gzip) and extraction.
//!
//! Each OCI layer is a gzip-compressed tar archive.  This module handles:
//!
//! - **Packing**: assembling a list of in-memory [`LayerEntry`] values into a
//!   compressed tar blob, returning both the `diff_id` (sha256 of the
//!   *uncompressed* tar) and the layer `digest` (sha256 of the *compressed*
//!   blob), as required by the OCI image spec.
//!
//! - **Unpacking**: extracting a compressed tar blob onto a filesystem path,
//!   respecting OCI whiteout semantics (`.wh.<name>` and `.wh..wh..opq`).
//!
//! ## OCI Whiteout Convention
//!
//! | Special filename          | Meaning                                      |
//! |---------------------------|----------------------------------------------|
//! | `.wh.<name>`              | Delete the file/dir `<name>` in the same dir |
//! | `.wh..wh..opq`            | Opaque whiteout: delete the entire directory |
//!
//! mcpm does not currently *emit* whiteout entries (it only appends / replaces)
//! but it correctly *handles* them when extracting layers pulled from a registry.

use std::fmt::Write as FmtWrite;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use sha2::{Digest as ShaDigest, Sha256};
use tar::{Archive, Builder, EntryType, Header};

// ── LayerEntry ────────────────────────────────────────────────────────────────

/// A single file or symlink to be packed into an OCI layer tar archive.
#[derive(Debug, Clone)]
pub struct LayerEntry {
    /// The path *inside* the tar archive (relative, no leading `/`).
    ///
    /// Examples: `"bundles/jars/EssentialsX.jar"`, `"plugins/Essentials/config.yml"`
    pub path: String,

    /// File content.  For directories use [`LayerEntry::directory`].
    pub data: Vec<u8>,

    /// Whether the entry should be marked executable (`0o755`) in the archive.
    /// Plain files default to `0o644`.
    pub executable: bool,
}

#[allow(dead_code)]
impl LayerEntry {
    /// Construct a regular (non-executable) file entry.
    pub fn file(path: impl Into<String>, data: Vec<u8>) -> Self {
        LayerEntry {
            path: path.into(),
            data,
            executable: false,
        }
    }

    /// Construct an executable file entry.
    pub fn executable(path: impl Into<String>, data: Vec<u8>) -> Self {
        LayerEntry {
            path: path.into(),
            data,
            executable: true,
        }
    }

    /// Construct a directory entry (zero-byte, with `0o755` mode).
    pub fn directory(path: impl Into<String>) -> Self {
        LayerEntry {
            path: path.into(),
            data: Vec::new(),
            executable: true, // directories use 0o755
        }
    }
}

// ── PackedLayer ───────────────────────────────────────────────────────────────

/// The result of [`pack_layer`]: a gzip-compressed tar blob with its OCI
/// content-addressable metadata.
#[derive(Debug, Clone)]
pub struct PackedLayer {
    /// The gzip-compressed tar bytes — this is what gets stored in the registry
    /// and in the local blob cache.
    pub compressed: Vec<u8>,

    /// `"sha256:<hex>"` of the **uncompressed** tar.
    ///
    /// This is the `diff_id` stored in the OCI image config's
    /// `rootfs.diff_ids` array.
    pub diff_id: String,

    /// `"sha256:<hex>"` of the **compressed** blob.
    ///
    /// This is the `digest` stored in the manifest's `layers` array.
    pub digest: String,

    /// Compressed byte size — the `size` stored in the manifest descriptor.
    pub size: u64,
}

// ── pack_layer ────────────────────────────────────────────────────────────────

/// Pack a slice of [`LayerEntry`] values into a single gzip-compressed OCI
/// layer, computing both the `diff_id` and the `digest` in one pass.
///
/// Intermediate directories are automatically inserted so the archive is
/// well-formed.  All timestamps are set to zero (epoch) for reproducibility.
pub fn pack_layer(entries: &[LayerEntry]) -> Result<PackedLayer> {
    // We build the uncompressed tar in memory first so we can hash it for the
    // diff_id, then compress the result for the layer blob.
    //
    // For large layers a two-pass streaming approach would be more
    // memory-efficient, but in-memory is simpler and sufficient for plugin
    // bundles (jars are typically < 50 MB each).

    let uncompressed_tar = build_tar(entries)?;

    // diff_id = sha256 of the *uncompressed* tar.
    let diff_id = sha256_hex(&uncompressed_tar);

    // Compress.
    let compressed = gzip_compress(&uncompressed_tar)?;

    // digest = sha256 of the *compressed* blob.
    let digest = sha256_hex(&compressed);
    let size = compressed.len() as u64;

    Ok(PackedLayer {
        compressed,
        diff_id,
        digest,
        size,
    })
}

/// Build an uncompressed tar archive from the given entries.
///
/// Parent directories are synthesised automatically so that every entry has a
/// well-formed ancestor chain.  Entries are processed in order; duplicate
/// paths are allowed (later entry replaces earlier one in a real overlay, but
/// within a single layer the tar is simply appended — extraction tools handle
/// the collision by writing the last occurrence).
fn build_tar(entries: &[LayerEntry]) -> Result<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    let mut builder = Builder::new(&mut buf);
    builder.follow_symlinks(false);

    // Track which parent directories we have already added.
    let mut dirs_added: std::collections::HashSet<String> = std::collections::HashSet::new();

    for entry in entries {
        // Ensure all ancestor directories appear in the archive first.
        ensure_parent_dirs(&mut builder, &entry.path, &mut dirs_added)
            .with_context(|| format!("adding parent dirs for '{}'", entry.path))?;

        if entry.data.is_empty() && entry.path.ends_with('/') {
            // Directory entry.
            add_directory_entry(&mut builder, &entry.path, &mut dirs_added)?;
        } else {
            // Regular file entry.
            add_file_entry(&mut builder, &entry.path, &entry.data, entry.executable)
                .with_context(|| format!("adding tar entry for '{}'", entry.path))?;
        }
    }

    builder.finish().context("finalising tar archive")?;
    drop(builder);

    Ok(buf)
}

/// Add a directory entry to the tar archive (if not already present).
fn add_directory_entry(
    builder: &mut Builder<&mut Vec<u8>>,
    path: &str,
    dirs_added: &mut std::collections::HashSet<String>,
) -> Result<()> {
    let normalised = normalise_dir_path(path);
    if dirs_added.contains(&normalised) {
        return Ok(());
    }
    dirs_added.insert(normalised.clone());

    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::Directory);
    header.set_mode(0o755);
    header.set_size(0);
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_cksum();

    builder
        .append_data(&mut header, &normalised, io::empty())
        .with_context(|| format!("appending directory entry '{}'", normalised))?;

    Ok(())
}

/// Add all ancestor directories of `entry_path` that have not yet been added.
fn ensure_parent_dirs(
    builder: &mut Builder<&mut Vec<u8>>,
    entry_path: &str,
    dirs_added: &mut std::collections::HashSet<String>,
) -> Result<()> {
    let path = Path::new(entry_path);
    let mut prefix = PathBuf::new();

    for component in path.components() {
        // Skip `.`, `..`, and root components — tar archives should use
        // relative paths only.
        if let Component::Normal(c) = component {
            prefix.push(c);
            // Don't add a directory entry for the file itself.
            if prefix.as_os_str() == path.as_os_str() {
                break;
            }
            let dir_path = format!("{}/", prefix.to_string_lossy());
            if !dirs_added.contains(&dir_path) {
                dirs_added.insert(dir_path.clone());
                let mut header = Header::new_gnu();
                header.set_entry_type(EntryType::Directory);
                header.set_mode(0o755);
                header.set_size(0);
                header.set_mtime(0);
                header.set_uid(0);
                header.set_gid(0);
                header.set_cksum();
                builder
                    .append_data(&mut header, &dir_path, io::empty())
                    .with_context(|| format!("appending parent dir '{}'", dir_path))?;
            }
        }
    }
    Ok(())
}

/// Add a regular file entry to the tar archive.
fn add_file_entry(
    builder: &mut Builder<&mut Vec<u8>>,
    path: &str,
    data: &[u8],
    executable: bool,
) -> Result<()> {
    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::Regular);
    header.set_mode(if executable { 0o755 } else { 0o644 });
    header.set_size(data.len() as u64);
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_cksum();

    builder
        .append_data(&mut header, path, data)
        .with_context(|| format!("appending file entry '{}'", path))?;

    Ok(())
}

/// Gzip-compress `data` with the default compression level.
fn gzip_compress(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(data)
        .context("writing data to gzip encoder")?;
    encoder.finish().context("finalising gzip compression")
}

// ── unpack_layer ──────────────────────────────────────────────────────────────

/// The result of unpacking a layer — records which paths were written.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct UnpackResult {
    /// Paths that were newly created (did not previously exist on disk).
    pub created: Vec<String>,
    /// Paths that were overwritten (already existed on disk).
    pub overwritten: Vec<String>,
    /// Paths that were deleted via OCI whiteout entries.
    pub deleted: Vec<String>,
}

#[allow(dead_code)]
impl UnpackResult {
    fn new() -> Self {
        UnpackResult {
            created: Vec::new(),
            overwritten: Vec::new(),
            deleted: Vec::new(),
        }
    }

    /// Total number of files touched (created + overwritten + deleted).
    pub fn total_changes(&self) -> usize {
        self.created.len() + self.overwritten.len() + self.deleted.len()
    }
}

/// Extract a gzip-compressed OCI layer onto `dest_dir`.
///
/// - Standard files/directories are written to `dest_dir/<path>`.
/// - OCI whiteout entries are processed: `.wh.<name>` deletes `<name>`, and
///   `.wh..wh..opq` deletes the entire parent directory contents.
/// - Existing files are overwritten (last-layer-wins semantics, as per OCI
///   overlay spec).
/// - Returns an [`UnpackResult`] describing what changed.
#[allow(dead_code)]
pub fn unpack_layer(compressed: &[u8], dest_dir: &Path) -> Result<UnpackResult> {
    let mut result = UnpackResult::new();

    // Decompress.
    let mut decoder = GzDecoder::new(compressed);
    let mut tar_data: Vec<u8> = Vec::new();
    decoder
        .read_to_end(&mut tar_data)
        .context("decompressing layer blob")?;

    let mut archive = Archive::new(tar_data.as_slice());

    // First pass: identify opaque whiteout directories.
    // We process entries twice: once to find all .wh..wh..opq entries, then
    // again to extract.  Since tar data is already in memory this is cheap.
    let mut opaque_dirs: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    {
        let mut scan_archive = Archive::new(tar_data.as_slice());
        for entry_result in scan_archive.entries().context("scanning tar entries")? {
            let entry = entry_result.context("reading tar entry during scan")?;
            let tar_path = entry
                .path()
                .context("reading tar entry path during scan")?
                .to_path_buf();

            if let Some(file_name) = tar_path.file_name() {
                if file_name == ".wh..wh..opq" {
                    if let Some(parent) = tar_path.parent() {
                        opaque_dirs.insert(parent.to_path_buf());
                    }
                }
            }
        }
    }

    // Apply opaque whiteouts: remove directory contents before extraction.
    for opaque_dir in &opaque_dirs {
        let dest_path = dest_dir.join(opaque_dir);
        if dest_path.exists() && dest_path.is_dir() {
            for sub_entry in std::fs::read_dir(&dest_path).with_context(|| {
                format!("reading dir for opaque whiteout: {}", dest_path.display())
            })? {
                let sub = sub_entry?.path();
                let rel = sub.strip_prefix(dest_dir).unwrap_or(&sub);
                if sub.is_dir() {
                    std::fs::remove_dir_all(&sub).with_context(|| {
                        format!("opaque whiteout: removing dir {}", sub.display())
                    })?;
                } else {
                    std::fs::remove_file(&sub).with_context(|| {
                        format!("opaque whiteout: removing file {}", sub.display())
                    })?;
                }
                result.deleted.push(rel.to_string_lossy().into_owned());
            }
        }
    }

    // Second pass: extract entries.
    for entry_result in archive.entries().context("iterating tar entries")? {
        let mut entry = entry_result.context("reading tar entry")?;
        let tar_path = entry
            .path()
            .context("reading tar entry path")?
            .to_path_buf();

        // Strip any leading `/` for safety.
        let rel_path = strip_leading_slash(&tar_path);

        let file_name = rel_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        // Handle regular whiteout entries (.wh.<name>).
        if file_name.starts_with(".wh.") && file_name != ".wh..wh..opq" {
            let real_name = &file_name[4..]; // strip ".wh."
            let target = rel_path.parent().unwrap_or(Path::new("")).join(real_name);
            let dest_path = dest_dir.join(&target);

            if dest_path.exists() {
                if dest_path.is_dir() {
                    std::fs::remove_dir_all(&dest_path).with_context(|| {
                        format!("whiteout: removing dir {}", dest_path.display())
                    })?;
                } else {
                    std::fs::remove_file(&dest_path).with_context(|| {
                        format!("whiteout: removing file {}", dest_path.display())
                    })?;
                }
                result.deleted.push(target.to_string_lossy().into_owned());
            }
            continue;
        }

        // Skip the opaque whiteout sentinel itself.
        if file_name == ".wh..wh..opq" {
            continue;
        }

        let dest_path = dest_dir.join(&rel_path);

        match entry.header().entry_type() {
            EntryType::Directory => {
                std::fs::create_dir_all(&dest_path)
                    .with_context(|| format!("creating directory: {}", dest_path.display()))?;
            }

            EntryType::Regular | EntryType::Continuous => {
                // Ensure parent directory exists.
                if let Some(parent) = dest_path.parent() {
                    std::fs::create_dir_all(parent).with_context(|| {
                        format!("creating parent directory: {}", parent.display())
                    })?;
                }

                let existed = dest_path.exists();

                // Read content.
                let mut content = Vec::new();
                entry
                    .read_to_end(&mut content)
                    .with_context(|| format!("reading tar entry '{}'", rel_path.display()))?;

                // Apply permissions from tar header.
                let mode = entry.header().mode().unwrap_or(0o644);

                write_file(&dest_path, &content, mode)?;

                let rel_str = rel_path.to_string_lossy().into_owned();
                if existed {
                    result.overwritten.push(rel_str);
                } else {
                    result.created.push(rel_str);
                }
            }

            EntryType::Symlink => {
                let link_target = entry
                    .link_name()
                    .context("reading symlink target")?
                    .map(|p| p.to_path_buf())
                    .unwrap_or_default();

                if let Some(parent) = dest_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }

                // Remove existing entry if present (symlink targets may change).
                if dest_path.exists() || dest_path.symlink_metadata().is_ok() {
                    let _ = std::fs::remove_file(&dest_path);
                }

                #[cfg(unix)]
                {
                    std::os::unix::fs::symlink(&link_target, &dest_path).with_context(|| {
                        format!(
                            "creating symlink {} → {}",
                            dest_path.display(),
                            link_target.display()
                        )
                    })?;
                }
                #[cfg(not(unix))]
                {
                    // On non-Unix systems, extract the symlink target as a
                    // plain file if we can read it relative to dest_dir.
                    let _ = link_target; // suppress unused warning
                }

                let existed_before = result
                    .overwritten
                    .contains(&rel_path.to_string_lossy().into_owned());
                let rel_str = rel_path.to_string_lossy().into_owned();
                if !existed_before {
                    result.created.push(rel_str);
                }
            }

            EntryType::Link => {
                // Hard links: resolve relative to dest_dir.
                let link_src = entry
                    .link_name()
                    .context("reading hardlink target")?
                    .map(|p| dest_dir.join(strip_leading_slash(&p)))
                    .unwrap_or_default();

                if let Some(parent) = dest_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }

                std::fs::copy(&link_src, &dest_path).with_context(|| {
                    format!(
                        "copying hardlink {} → {}",
                        link_src.display(),
                        dest_path.display()
                    )
                })?;

                let rel_str = rel_path.to_string_lossy().into_owned();
                result.created.push(rel_str);
            }

            // Skip character/block devices, FIFOs, etc.  They cannot exist
            // meaningfully in a Minecraft server directory.
            _ => {}
        }
    }

    Ok(result)
}

// ── unpack_layer_dry_run ──────────────────────────────────────────────────────

/// Like [`unpack_layer`], but does **not** write anything to disk.
///
/// Returns what *would* change if the layer were applied to `dest_dir`.
/// Used by `bundle diff`.
#[allow(dead_code)]
pub fn unpack_layer_dry_run(compressed: &[u8], dest_dir: &Path) -> Result<UnpackResult> {
    let mut result = UnpackResult::new();

    let mut decoder = GzDecoder::new(compressed);
    let mut tar_data: Vec<u8> = Vec::new();
    decoder
        .read_to_end(&mut tar_data)
        .context("decompressing layer blob (dry-run)")?;

    let mut archive = Archive::new(tar_data.as_slice());

    for entry_result in archive
        .entries()
        .context("iterating tar entries (dry-run)")?
    {
        let entry = entry_result.context("reading tar entry (dry-run)")?;
        let tar_path = entry
            .path()
            .context("reading tar entry path (dry-run)")?
            .to_path_buf();

        let rel_path = strip_leading_slash(&tar_path);

        let file_name = rel_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        if file_name == ".wh..wh..opq" {
            // Would delete the directory contents.
            let dir = rel_path.parent().unwrap_or(Path::new("")).to_path_buf();
            let dest = dest_dir.join(&dir);
            if dest.is_dir() {
                for e in walkdir::WalkDir::new(&dest)
                    .min_depth(1)
                    .into_iter()
                    .flatten()
                {
                    let p = e.path().strip_prefix(dest_dir).unwrap_or(e.path());
                    result.deleted.push(p.to_string_lossy().into_owned());
                }
            }
            continue;
        }

        if let Some(real_name) = file_name.strip_prefix(".wh.") {
            let target = rel_path.parent().unwrap_or(Path::new("")).join(real_name);
            let dest_path = dest_dir.join(&target);
            if dest_path.exists() {
                result.deleted.push(target.to_string_lossy().into_owned());
            }
            continue;
        }

        match entry.header().entry_type() {
            EntryType::Directory => {
                // Directory creation is not reported as a change.
            }
            EntryType::Regular | EntryType::Continuous | EntryType::Symlink | EntryType::Link => {
                let dest_path = dest_dir.join(&rel_path);
                let rel_str = rel_path.to_string_lossy().into_owned();
                if dest_path.exists() {
                    result.overwritten.push(rel_str);
                } else {
                    result.created.push(rel_str);
                }
            }
            _ => {}
        }
    }

    Ok(result)
}

// ── collect_directory_entries ─────────────────────────────────────────────────

/// Recursively collect all files under `src_dir` as [`LayerEntry`] values,
/// prefixing each path with `dest_prefix`.
///
/// Used by the `ADD <dir>/ <dest>/` directive.  Directory entries are included
/// so that the tar archive has a complete ancestor chain.
///
/// # Example
///
/// ```text
/// src_dir     = "./config/Essentials/"
/// dest_prefix = "plugins/Essentials/"
/// ```
///
/// Produces entries like `"plugins/Essentials/config.yml"`,
/// `"plugins/Essentials/userdata/"`, etc.
pub fn collect_directory_entries(src_dir: &Path, dest_prefix: &str) -> Result<Vec<LayerEntry>> {
    let mut entries = Vec::new();

    for dir_entry in walkdir::WalkDir::new(src_dir)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
    {
        let dir_entry =
            dir_entry.with_context(|| format!("walking directory: {}", src_dir.display()))?;

        let relative = dir_entry.path().strip_prefix(src_dir).with_context(|| {
            format!(
                "stripping prefix '{}' from '{}'",
                src_dir.display(),
                dir_entry.path().display()
            )
        })?;

        // Skip the root itself.
        if relative == Path::new("") || relative.as_os_str().is_empty() {
            continue;
        }

        let entry_path = if dest_prefix.ends_with('/') {
            format!("{}{}", dest_prefix, relative.to_string_lossy())
        } else {
            format!("{}/{}", dest_prefix, relative.to_string_lossy())
        };

        if dir_entry.file_type().is_dir() {
            entries.push(LayerEntry::directory(format!("{}/", entry_path)));
        } else if dir_entry.file_type().is_file() {
            let data = std::fs::read(dir_entry.path())
                .with_context(|| format!("reading file: {}", dir_entry.path().display()))?;
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                let meta = dir_entry.metadata()?;
                meta.permissions().mode() & 0o111 != 0
            };
            #[cfg(not(unix))]
            let executable = false;

            entries.push(LayerEntry {
                path: entry_path,
                data,
                executable,
            });
        }
        // Symlinks are skipped for simplicity — they could be added if needed.
    }

    Ok(entries)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Strip a leading `/` from `path` to make it relative.
#[allow(dead_code)]
fn strip_leading_slash(path: &Path) -> PathBuf {
    path.strip_prefix("/").unwrap_or(path).to_path_buf()
}

/// Normalise a directory path: ensure it ends with `/` and has no leading `/`.
fn normalise_dir_path(path: &str) -> String {
    let stripped = path.trim_start_matches('/');
    if stripped.ends_with('/') {
        stripped.to_string()
    } else {
        format!("{}/", stripped)
    }
}

/// Compute `"sha256:<hex>"` of `data`.
fn sha256_hex(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    let mut s = String::with_capacity(7 + 64);
    s.push_str("sha256:");
    for byte in hash.iter() {
        write!(s, "{:02x}", byte).unwrap();
    }
    s
}

/// Write `data` to `path`, optionally setting Unix permissions.
#[allow(dead_code)]
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── pack_layer ────────────────────────────────────────────────────────────

    #[test]
    fn pack_empty_layer() {
        let result = pack_layer(&[]).unwrap();
        assert!(
            !result.compressed.is_empty(),
            "empty tar should still have gzip header"
        );
        assert!(result.diff_id.starts_with("sha256:"));
        assert!(result.digest.starts_with("sha256:"));
    }

    #[test]
    fn pack_and_unpack_single_file() {
        let entries = vec![LayerEntry::file(
            "bundles/jars/MyPlugin.jar",
            b"fake-jar-bytes".to_vec(),
        )];

        let packed = pack_layer(&entries).unwrap();

        let dir = TempDir::new().unwrap();
        let result = unpack_layer(&packed.compressed, dir.path()).unwrap();

        let expected = dir.path().join("bundles/jars/MyPlugin.jar");
        assert!(expected.exists(), "jar file should be extracted");
        assert_eq!(std::fs::read(&expected).unwrap(), b"fake-jar-bytes");
        assert_eq!(result.created, vec!["bundles/jars/MyPlugin.jar"]);
        assert!(result.overwritten.is_empty());
        assert!(result.deleted.is_empty());
    }

    #[test]
    fn pack_creates_parent_directories() {
        let entries = vec![LayerEntry::file(
            "plugins/Essentials/config.yml",
            b"essentials-config".to_vec(),
        )];

        let packed = pack_layer(&entries).unwrap();

        let dir = TempDir::new().unwrap();
        unpack_layer(&packed.compressed, dir.path()).unwrap();

        assert!(dir.path().join("plugins").is_dir());
        assert!(dir.path().join("plugins/Essentials").is_dir());
        assert!(dir.path().join("plugins/Essentials/config.yml").exists());
    }

    #[test]
    fn pack_multiple_files() {
        let entries = vec![
            LayerEntry::file("bundles/jars/A.jar", b"jar-a".to_vec()),
            LayerEntry::file("bundles/jars/B.jar", b"jar-b".to_vec()),
            LayerEntry::file("plugins/A/config.yml", b"config-a".to_vec()),
        ];

        let packed = pack_layer(&entries).unwrap();

        let dir = TempDir::new().unwrap();
        let result = unpack_layer(&packed.compressed, dir.path()).unwrap();

        assert_eq!(result.created.len(), 3);
        assert_eq!(
            std::fs::read(dir.path().join("bundles/jars/A.jar")).unwrap(),
            b"jar-a"
        );
        assert_eq!(
            std::fs::read(dir.path().join("bundles/jars/B.jar")).unwrap(),
            b"jar-b"
        );
        assert_eq!(
            std::fs::read(dir.path().join("plugins/A/config.yml")).unwrap(),
            b"config-a"
        );
    }

    #[test]
    fn overwrite_existing_file_is_recorded() {
        let entries = vec![LayerEntry::file(
            "plugins/A/config.yml",
            b"new-content".to_vec(),
        )];

        let packed = pack_layer(&entries).unwrap();

        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("plugins/A")).unwrap();
        std::fs::write(dir.path().join("plugins/A/config.yml"), b"old-content").unwrap();

        let result = unpack_layer(&packed.compressed, dir.path()).unwrap();

        assert!(result.created.is_empty());
        assert_eq!(result.overwritten.len(), 1);
        assert_eq!(
            std::fs::read(dir.path().join("plugins/A/config.yml")).unwrap(),
            b"new-content"
        );
    }

    // ── diff_id / digest consistency ──────────────────────────────────────────

    #[test]
    fn diff_id_is_sha256_of_uncompressed() {
        let entries = vec![LayerEntry::file("test.txt", b"hello".to_vec())];
        let packed = pack_layer(&entries).unwrap();

        // Decompress and re-hash.
        let mut decoder = GzDecoder::new(packed.compressed.as_slice());
        let mut raw = Vec::new();
        decoder.read_to_end(&mut raw).unwrap();

        let expected = sha256_hex(&raw);
        assert_eq!(packed.diff_id, expected);
    }

    #[test]
    fn digest_is_sha256_of_compressed() {
        let entries = vec![LayerEntry::file("test.txt", b"world".to_vec())];
        let packed = pack_layer(&entries).unwrap();

        let expected = sha256_hex(&packed.compressed);
        assert_eq!(packed.digest, expected);
        assert_eq!(packed.size, packed.compressed.len() as u64);
    }

    #[test]
    fn same_content_deterministic_diff_id() {
        // diff_id should be the same for two packs of the same data because we
        // use zero timestamps and deterministic ordering.
        let entries = vec![
            LayerEntry::file("a.txt", b"hello".to_vec()),
            LayerEntry::file("b.txt", b"world".to_vec()),
        ];

        let packed1 = pack_layer(&entries).unwrap();
        let packed2 = pack_layer(&entries).unwrap();

        assert_eq!(packed1.diff_id, packed2.diff_id);
    }

    // ── OCI whiteout ──────────────────────────────────────────────────────────

    #[test]
    fn whiteout_entry_deletes_file() {
        // First, create a base layer with a file.
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("plugins/A")).unwrap();
        std::fs::write(dir.path().join("plugins/A/old.yml"), b"old").unwrap();

        // Now apply a whiteout layer.
        let whiteout_entries = vec![LayerEntry::file("plugins/A/.wh.old.yml", b"".to_vec())];
        let packed = pack_layer(&whiteout_entries).unwrap();
        let result = unpack_layer(&packed.compressed, dir.path()).unwrap();

        assert!(!dir.path().join("plugins/A/old.yml").exists());
        assert_eq!(result.deleted, vec!["plugins/A/old.yml"]);
    }

    // ── dry-run ───────────────────────────────────────────────────────────────

    #[test]
    fn dry_run_does_not_write_files() {
        let entries = vec![LayerEntry::file(
            "plugins/test/config.yml",
            b"data".to_vec(),
        )];
        let packed = pack_layer(&entries).unwrap();

        let dir = TempDir::new().unwrap();
        let result = unpack_layer_dry_run(&packed.compressed, dir.path()).unwrap();

        // Nothing should be written.
        assert!(!dir.path().join("plugins/test/config.yml").exists());
        // But the result should report the expected creation.
        assert_eq!(result.created, vec!["plugins/test/config.yml"]);
    }

    #[test]
    fn dry_run_reports_overwrite_for_existing() {
        let entries = vec![LayerEntry::file("plugins/test.yml", b"new".to_vec())];
        let packed = pack_layer(&entries).unwrap();

        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("plugins")).unwrap();
        std::fs::write(dir.path().join("plugins/test.yml"), b"old").unwrap();

        let result = unpack_layer_dry_run(&packed.compressed, dir.path()).unwrap();

        assert_eq!(result.overwritten, vec!["plugins/test.yml"]);
        // File content should NOT have changed.
        assert_eq!(
            std::fs::read(dir.path().join("plugins/test.yml")).unwrap(),
            b"old"
        );
    }

    // ── normalise_dir_path ────────────────────────────────────────────────────

    #[test]
    fn normalise_adds_trailing_slash() {
        assert_eq!(normalise_dir_path("plugins/A"), "plugins/A/");
    }

    #[test]
    fn normalise_strips_leading_slash() {
        assert_eq!(normalise_dir_path("/plugins/A"), "plugins/A/");
    }

    #[test]
    fn normalise_keeps_trailing_slash() {
        assert_eq!(normalise_dir_path("plugins/A/"), "plugins/A/");
    }

    // ── strip_leading_slash ───────────────────────────────────────────────────

    #[test]
    fn strip_leading_slash_with_slash() {
        let p = PathBuf::from("/some/path/file.txt");
        assert_eq!(strip_leading_slash(&p), PathBuf::from("some/path/file.txt"));
    }

    #[test]
    fn strip_leading_slash_without_slash() {
        let p = PathBuf::from("some/path/file.txt");
        assert_eq!(strip_leading_slash(&p), PathBuf::from("some/path/file.txt"));
    }
}
