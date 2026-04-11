//! Bundle build — parse the Bundlefile, fetch / copy sources, pack OCI layers,
//! and assemble a spec-compliant `LocalImage` ready for `bundle push`.
//!
//! ## Layer structure produced
//!
//! For each Bundlefile stage (in order), all `ADD` and `COPY` files are batched
//! into a single gzip-compressed OCI layer.  If a stage `FROM`s an existing OCI
//! image its layer descriptors and `bundle.managed-keys` annotation are
//! inherited first.
//!
//! ## The OCI manifest IS the lock file
//!
//! `bundle build` does **not** read or write `bundle.lock`.  The output OCI
//! image manifest already content-addresses every layer by its sha256 digest,
//! making the manifest itself a complete build lock.
//!
//! - Use `ADD --checksum=sha256:<hex> <url> <dest>` for explicit URL
//!   verification at build time.
//!
//! ## Multi-stage annotation merging
//!
//! `MANAGE` directives accumulate across stages using last-writer-wins per
//! config path (later stage wins).

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use oci_client::client::ClientConfig;
use oci_client::{Client, Reference};

/// The reserved keyword for a stage with no base image.
///
/// `FROM scratch` means "start with zero inherited layers" — it is a
/// build-time concept only and never triggers a registry lookup.
/// It maps directly to an OCI image whose `rootfs.diff_ids` contains
use crate::bundle::annotations::{self, ManagedKeys};
use crate::bundle::layer::{self, collect_directory_entries, LayerEntry, PackedLayer};
use crate::bundlefile::parser;
use crate::bundlefile::types::{
    AddDirective, AddSource, Bundlefile, CopyDirective, CopyFrom, Stage, SCRATCH_STAGE,
};
use crate::registry::client::McpmRegistryClient;
use crate::registry::types::{
    build_image_config, image_config_to_bytes, Descriptor, ImageManifest, ImageManifestBuilder,
    LocalImage, MediaType, SCHEMA_VERSION,
};
use crate::util::digest::sha256_digest;
use crate::util::fetch::fetch_url;

/// Build a `LocalImage` from the `Bundlefile` at `bundlefile_path`.
///
/// `cli_overrides` contains `--build-arg KEY=VAL` pairs from the command line;
/// they override `ARG KEY=DEFAULT` defaults declared in the Bundlefile.
///
/// Does **not** read or write `bundle.lock` — the output OCI manifest is the
/// lock file.  Does **not** push to a registry; call
/// [`McpmRegistryClient::push_image`] afterwards.
pub async fn build(
    bundlefile_path: &Path,
    cli_overrides: &HashMap<String, String>,
) -> Result<LocalImage> {
    let bundlefile_dir = bundlefile_path
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf();

    let content = std::fs::read_to_string(bundlefile_path)
        .with_context(|| format!("reading Bundlefile: {}", bundlefile_path.display()))?;

    let bundlefile = parser::parse(&content, cli_overrides).context("parsing Bundlefile")?;

    build_from_parsed(&bundlefile, &bundlefile_dir).await
}

/// Build from an already-parsed [`Bundlefile`].
///
/// `root_dir` is the directory used to resolve relative `ADD ./…` and
/// `COPY ./…` paths; normally the directory containing the Bundlefile.
pub async fn build_from_parsed(bundlefile: &Bundlefile, root_dir: &Path) -> Result<LocalImage> {
    let mut all_layer_descriptors: Vec<Descriptor> = Vec::new();
    let mut all_diff_ids: Vec<String> = Vec::new();
    let mut new_blobs: HashMap<String, Vec<u8>> = HashMap::new();
    let mut accumulated_managed_keys: ManagedKeys = ManagedKeys::new();
    let mut accumulated_labels: HashMap<String, String> = HashMap::new();
    let mut stage_outputs: StageOutputs = Vec::new();

    for (stage_idx, stage) in bundlefile.stages.iter().enumerate() {
        let stage_label = format!("stage {} (FROM {})", stage_idx + 1, stage.from);
        eprintln!("[build] processing {}", stage_label);

        if stage.from.trim().to_lowercase() == SCRATCH_STAGE {
            // FROM scratch — start with zero inherited layers.
            // No registry access, no network call.
            eprintln!("[build]   FROM scratch — starting with empty layers");
        } else {
            eprintln!("[build]   pulling base image manifest: {}", stage.from);
            let (base_layers, base_diff_ids, base_keys) =
                fetch_base_image_info(&stage.from).await.map_err(|e| {
                    // Give a targeted hint when the reference looks like a
                    // misspelling of "scratch" (e.g. "scrath", "Scratch", …).
                    let looks_like_scratch_typo = {
                        let lower = stage.from.trim().to_lowercase();
                        lower.len() <= 8
                            && lower.chars().filter(|c| "scratch".contains(*c)).count()
                                >= lower.len().saturating_sub(1)
                    };
                    if looks_like_scratch_typo {
                        anyhow::anyhow!(
                            "failed to fetch base image '{}'.\n\
                             \n  Did you mean `FROM scratch`?\n\
                             \n  `FROM scratch` is the reserved keyword for a stage\n\
                             \n  with no base image (no network access needed).\n\
                             \n  Original error: {:#}",
                            stage.from,
                            e
                        )
                    } else {
                        anyhow::anyhow!(
                            "failed to fetch base image '{}'.\n\
                             \n  Make sure the image exists and you are authenticated.\n\
                             \n  Use `FROM scratch` to build with no base image.\n\
                             \n  Original error: {:#}",
                            stage.from,
                            e
                        )
                    }
                })?;

            if base_layers.len() != base_diff_ids.len() {
                eprintln!(
                    "[build]   warning: base image layer count ({}) does not match \
                     diff_id count ({}); diff_ids will be incomplete",
                    base_layers.len(),
                    base_diff_ids.len()
                );
            }
            all_layer_descriptors.extend(base_layers);
            all_diff_ids.extend(base_diff_ids);
            accumulated_managed_keys = annotations::merge(accumulated_managed_keys, base_keys);
        }

        // All files from this stage are batched into a single layer.
        // Stage outputs are tracked so that later stages can COPY --from them.
        let mut stage_files: HashMap<String, Vec<u8>> = HashMap::new();

        // Process ADD directives (local paths and remote URLs).
        for (add_idx, add) in stage.adds.iter().enumerate() {
            eprintln!("[build]   ADD {}/{}", add_idx + 1, stage.adds.len());
            let resolved = resolve_add(add, root_dir)
                .await
                .with_context(|| format!("ADD directive {} in {}", add_idx + 1, stage_label))?;
            for (path, data) in resolved {
                stage_files.insert(path, data);
            }
        }

        // Process COPY directives (local context or --from=<stage>).
        for (copy_idx, copy) in stage.copies.iter().enumerate() {
            eprintln!("[build]   COPY {}/{}", copy_idx + 1, stage.copies.len());
            let resolved = resolve_copy(copy, root_dir, &stage_outputs, &bundlefile.stages)
                .with_context(|| format!("COPY directive {} in {}", copy_idx + 1, stage_label))?;
            for (path, data) in resolved {
                stage_files.insert(path, data);
            }
        }

        // Record this stage's file tree for COPY --from in later stages.
        stage_outputs.push(stage_files.clone());

        // Pack all stage files into a single layer (sorted for determinism).
        if !stage_files.is_empty() {
            let mut entries: Vec<LayerEntry> = stage_files
                .into_iter()
                .map(|(path, data)| LayerEntry::file(path, data))
                .collect();
            entries.sort_by(|a, b| a.path.cmp(&b.path));

            let packed = layer::pack_layer(&entries)
                .with_context(|| format!("packing layer for {}", stage_label))?;

            eprintln!(
                "[build]   stage layer: digest={} size={} ({} file(s))",
                &packed.digest[..std::cmp::min(19, packed.digest.len())],
                packed.size,
                entries.len(),
            );

            all_diff_ids.push(packed.diff_id.clone());
            let descriptor = make_layer_descriptor(&packed);
            new_blobs.insert(packed.digest, packed.compressed);
            all_layer_descriptors.push(descriptor);
        }

        let stage_keys = annotations::from_manage_directives(&stage.manages);
        accumulated_managed_keys = annotations::merge(accumulated_managed_keys, stage_keys);

        // Labels accumulate across stages; later stages override earlier ones.
        accumulated_labels.extend(stage.labels.iter().map(|(k, v)| (k.clone(), v.clone())));
    }

    let image_config = build_image_config(all_diff_ids, accumulated_labels)
        .context("building OCI image config")?;
    let config_data =
        image_config_to_bytes(&image_config).context("serialising OCI image config")?;
    let config_digest = sha256_digest(&config_data);

    eprintln!(
        "[build] image config digest: {}",
        &config_digest[..std::cmp::min(19, config_digest.len())]
    );

    let manifest_annotations: Option<HashMap<String, String>> =
        if accumulated_managed_keys.is_empty() {
            None
        } else {
            let mut ann: HashMap<String, String> = HashMap::new();
            annotations::set_in_annotations(&mut ann, &accumulated_managed_keys)
                .context("encoding bundle.managed-keys annotation")?;
            Some(ann)
        };

    // Descriptor::new takes impl Into<Digest>; Sha256Digest (via from_str)
    // satisfies that bound.  Strip the "sha256:" prefix since Sha256Digest
    // represents only the hex portion.
    let config_hex = config_digest
        .strip_prefix("sha256:")
        .unwrap_or(&config_digest);
    let config_sha256 = {
        use std::str::FromStr as _;
        oci_spec::image::Sha256Digest::from_str(config_hex)
            .context("computing config Sha256Digest")?
    };
    let config_descriptor = Descriptor::new(
        MediaType::ImageConfig,
        config_data.len() as u64,
        config_sha256,
    );

    let mut manifest_builder = ImageManifestBuilder::default()
        .schema_version(SCHEMA_VERSION)
        .media_type(MediaType::ImageManifest)
        .config(config_descriptor)
        .layers(all_layer_descriptors);
    if let Some(ann) = manifest_annotations {
        manifest_builder = manifest_builder.annotations(ann);
    }
    let manifest = manifest_builder
        .build()
        .context("assembling OCI image manifest")?;

    eprintln!(
        "[build] manifest has {} layer(s), {} new blob(s)",
        manifest.layers().len(),
        new_blobs.len()
    );

    // Include the config data as a "new blob" so push can upload it.
    new_blobs.insert(config_digest, config_data.clone());

    Ok(LocalImage {
        manifest,
        config_data,
        new_blobs,
    })
}

/// Tracks each stage's output file tree for `COPY --from=<stage>` resolution.
///
/// `stage_outputs[i]` maps server-root-relative path → raw file bytes for
/// every file written by stage `i`.
type StageOutputs = Vec<HashMap<String, Vec<u8>>>;

/// Resolve an [`AddDirective`] into a list of `(dest_path, bytes)` pairs.
///
/// - Local sources are read from `root_dir` (recursively for directories).
/// - Remote sources are downloaded; if `--checksum=sha256:<hex>` was declared
///   the download is verified immediately.  No lock file interaction — the
///   output OCI manifest content-addresses all layers.
async fn resolve_add(add: &AddDirective, root_dir: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    match &add.source {
        AddSource::Local { path } => {
            let full = if path.is_relative() {
                root_dir.join(path)
            } else {
                path.clone()
            };
            eprintln!("[build]     ADD local {} → {}", full.display(), add.dest);
            collect_add_entries_for_path(&full, &add.dest)
                .map(|es| es.into_iter().map(|e| (e.path, e.data)).collect())
        }

        AddSource::Remote { url, checksum } => {
            eprintln!("[build]     ADD remote {} → {}", url, add.dest);
            let data = fetch_remote(url, checksum.as_deref())
                .await
                .with_context(|| format!("fetching remote ADD source: {}", url))?;
            eprintln!("[build]       {} bytes", data.len());
            Ok(vec![(add.dest.clone(), data)])
        }
    }
}

/// Download a remote URL, optionally verifying an explicit checksum.
///
/// If `--checksum=sha256:<hex>` was declared on the `ADD` directive the
/// download is verified immediately and the build fails on mismatch.
///
/// No lock file interaction.  The output OCI manifest content-addresses every
/// layer by sha256, making it the authoritative record of what was built.
async fn fetch_remote(url: &str, explicit_checksum: Option<&str>) -> Result<Vec<u8>> {
    let data = fetch_url(url)
        .await
        .with_context(|| format!("downloading {}", url))?;

    if let Some(expected) = explicit_checksum {
        let actual = sha256_digest(&data);
        if actual != expected {
            bail!(
                "ADD --checksum mismatch for {}:\n  expected: {}\n  got:      {}",
                url,
                expected,
                actual
            );
        }
        eprintln!("[build]       checksum verified ✓");
    } else {
        eprintln!(
            "[build]       fetched {} bytes (no checksum — layer digest in manifest)",
            data.len()
        );
    }

    Ok(data)
}

/// Resolve a [`CopyDirective`] into `(dest_path, bytes)` pairs.
///
/// - `CopyFrom::BuildContext` — reads from `root_dir` like a local ADD.
/// - `CopyFrom::Stage(ref)` — copies matching files from a previous stage's
///   output tree (`stage_outputs[resolved_index]`).
fn resolve_copy(
    copy: &CopyDirective,
    root_dir: &Path,
    stage_outputs: &StageOutputs,
    all_stages: &[Stage],
) -> Result<Vec<(String, Vec<u8>)>> {
    match &copy.from {
        CopyFrom::BuildContext => {
            let full = if copy.src.is_relative() {
                root_dir.join(&copy.src)
            } else {
                copy.src.clone()
            };
            let full_str = full
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("source path contains non-UTF-8 characters"))?;

            if has_glob_chars(full_str) {
                eprintln!("[build]     COPY {} → {} (glob)", full_str, copy.dest);
                resolve_glob_from_context(full_str, &copy.dest)
            } else {
                eprintln!("[build]     COPY {} → {}", full.display(), copy.dest);
                collect_add_entries_for_path(&full, &copy.dest)
                    .map(|es| es.into_iter().map(|e| (e.path, e.data)).collect())
            }
        }

        CopyFrom::Stage(stage_ref) => {
            let idx = resolve_stage_ref(stage_ref, all_stages, stage_outputs.len())?;
            let source_files = &stage_outputs[idx];
            let src_str = copy.src.to_string_lossy().to_string();

            if has_glob_chars(&src_str) {
                eprintln!(
                    "[build]     COPY --from={} (stage {}) {} → {} (glob)",
                    stage_ref, idx, src_str, copy.dest
                );
                resolve_glob_from_stage(&src_str, &copy.dest, stage_ref, idx, source_files)
            } else {
                eprintln!(
                    "[build]     COPY --from={} (stage {}) {} → {}",
                    stage_ref, idx, src_str, copy.dest
                );

                let mut result: Vec<(String, Vec<u8>)> = Vec::new();

                for (file_path, data) in source_files {
                    if *file_path == src_str {
                        // Exact file match.
                        result.push((copy.dest.clone(), data.clone()));
                    } else if file_path.starts_with(&format!("{}/", src_str)) {
                        // File inside a matched directory subtree.
                        let rel = &file_path[src_str.len() + 1..];
                        let dest_path = if copy.dest.ends_with('/') {
                            format!("{}{}", copy.dest, rel)
                        } else {
                            format!("{}/{}", copy.dest, rel)
                        };
                        result.push((dest_path, data.clone()));
                    }
                }

                if result.is_empty() {
                    bail!(
                        "COPY --from={} (stage {}): no files matched path '{}'\n\
                         Available paths in that stage: {}",
                        stage_ref,
                        idx,
                        src_str,
                        {
                            let mut paths: Vec<&str> =
                                source_files.keys().map(String::as_str).collect();
                            paths.sort_unstable();
                            paths.join(", ")
                        }
                    );
                }

                Ok(result)
            }
        }
    }
}

/// Returns true if `s` contains any glob metacharacter (`*`, `?`, `[`).
fn has_glob_chars(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

/// Returns the non-wildcard directory prefix of a glob pattern.
///
/// Used to strip the fixed prefix from matched paths so only the
/// variable portion is appended to the destination.
///
/// Examples:
/// - `"plugins/**/*.jar"` → `"plugins/"`
/// - `"src/*.rs"` → `"src/"`
/// - `"*.jar"` → `""`
fn glob_base_prefix(pattern: &str) -> &str {
    let meta_pos = pattern.find(['*', '?', '[']).unwrap_or(pattern.len());
    let base_end = pattern[..meta_pos].rfind('/').map(|i| i + 1).unwrap_or(0);
    &pattern[..base_end]
}

/// Joins a destination prefix with the variable tail of a glob match.
fn join_glob_dest(dest: &str, rel: &str) -> String {
    if dest.ends_with('/') {
        format!("{}{}", dest, rel)
    } else {
        format!("{}/{}", dest, rel)
    }
}

/// Expands a glob pattern against the real filesystem and returns
/// `(dest_path, bytes)` pairs for every matched regular file.
///
/// `full_pattern` must be an absolute path string that may contain
/// glob metacharacters.  The non-wildcard directory prefix is stripped
/// from each match before combining with `dest`.
fn resolve_glob_from_context(full_pattern: &str, dest: &str) -> Result<Vec<(String, Vec<u8>)>> {
    let base = glob_base_prefix(full_pattern).to_string();

    let matches = glob::glob(full_pattern)
        .with_context(|| format!("invalid glob pattern '{}'", full_pattern))?;

    let mut result = Vec::new();
    for entry in matches {
        let path = entry.with_context(|| format!("error expanding glob '{}'", full_pattern))?;
        if path.is_dir() {
            continue; // directories are traversed implicitly via their contents
        }
        let path_str = path.to_str().ok_or_else(|| {
            anyhow::anyhow!(
                "matched path '{}' contains non-UTF-8 characters",
                path.display()
            )
        })?;
        let rel = path_str.strip_prefix(&base).unwrap_or(path_str);
        let dest_path = join_glob_dest(dest, rel);
        let data = std::fs::read(&path)
            .with_context(|| format!("reading COPY source '{}'", path.display()))?;
        result.push((dest_path, data));
    }

    if result.is_empty() {
        bail!("COPY: glob '{}' matched no files", full_pattern);
    }

    Ok(result)
}

/// Matches a glob pattern against the in-memory file tree of a previous stage
/// and returns `(dest_path, bytes)` pairs for every matching path.
///
/// Uses `require_literal_separator = true` so `*` does not cross directory
/// boundaries and `**` is required for recursive matching.
fn resolve_glob_from_stage(
    src_pattern: &str,
    dest: &str,
    stage_ref: &str,
    stage_idx: usize,
    source_files: &HashMap<String, Vec<u8>>,
) -> Result<Vec<(String, Vec<u8>)>> {
    let pattern = glob::Pattern::new(src_pattern)
        .with_context(|| format!("invalid glob pattern '{}'", src_pattern))?;
    let opts = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };
    let base = glob_base_prefix(src_pattern).to_string();

    // Sort paths for deterministic output ordering.
    let mut all_paths: Vec<&str> = source_files.keys().map(String::as_str).collect();
    all_paths.sort_unstable();

    let mut result = Vec::new();
    for file_path in &all_paths {
        if pattern.matches_with(file_path, opts) {
            let rel = file_path.strip_prefix(&base).unwrap_or(file_path);
            let dest_path = join_glob_dest(dest, rel);
            let data = source_files[*file_path].clone();
            result.push((dest_path, data));
        }
    }

    if result.is_empty() {
        bail!(
            "COPY --from={} (stage {}): glob '{}' matched no files\n\
             Available paths in that stage: {}",
            stage_ref,
            stage_idx,
            src_pattern,
            {
                let mut paths: Vec<&str> = source_files.keys().map(String::as_str).collect();
                paths.sort_unstable();
                paths.join(", ")
            }
        );
    }

    Ok(result)
}

/// Resolve a stage reference (numeric index string or stage name) to a
/// zero-based index into `stage_outputs`.
///
/// Returns an error if:
/// - The index is out of bounds (stage not yet built).
/// - The name does not match any stage built so far.
fn resolve_stage_ref(stage_ref: &str, all_stages: &[Stage], num_built: usize) -> Result<usize> {
    if let Ok(n) = stage_ref.parse::<usize>() {
        if n >= num_built {
            bail!(
                "COPY --from={}: stage index {} has not been built yet \
                 (only {} stage(s) complete so far)",
                stage_ref,
                n,
                num_built
            );
        }
        return Ok(n);
    }

    for (i, stage) in all_stages[..num_built].iter().enumerate() {
        if stage.name.as_deref() == Some(stage_ref) {
            return Ok(i);
        }
    }

    bail!(
        "COPY --from={}: no stage with that index or name among the {} built stage(s)\n\
         Named stages so far: {}",
        stage_ref,
        num_built,
        all_stages[..num_built]
            .iter()
            .filter_map(|s| s.name.as_deref())
            .collect::<Vec<_>>()
            .join(", ")
    );
}

/// Collect [`LayerEntry`] values for an `ADD <src> <dest>` directive.
///
/// - If `src_path` is a directory, it is walked recursively and all files are
///   included under `dest`.
/// - If `src_path` is a regular file, it is included directly at `dest`.
fn collect_add_entries_for_path(src_path: &Path, dest: &str) -> Result<Vec<LayerEntry>> {
    if !src_path.exists() {
        anyhow::bail!("ADD source path does not exist: {}", src_path.display());
    }

    if src_path.is_dir() {
        // Recursively collect directory contents.
        // Ensure dest ends with '/' to signal directory-to-directory copy.
        let dest_prefix = if dest.ends_with('/') {
            dest.to_string()
        } else {
            format!("{}/", dest)
        };
        collect_directory_entries(src_path, &dest_prefix).with_context(|| {
            format!(
                "collecting directory entries: {} → {}",
                src_path.display(),
                dest_prefix
            )
        })
    } else {
        // Single file: dest is the exact target path.
        let data = std::fs::read(src_path)
            .with_context(|| format!("reading ADD source file: {}", src_path.display()))?;

        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(src_path)?;
            meta.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;

        Ok(vec![LayerEntry {
            path: dest.to_string(),
            data,
            executable,
        }])
    }
}

/// Fetch the layer descriptors, diff_ids, and managed-keys annotation of an
/// existing OCI image so that they can be inherited by the current build.
///
/// Returns `(layer_descriptors, diff_ids, managed_keys)`.
///
/// Errors here are treated as warnings by the caller — if the base image
/// cannot be reached the build continues without inheriting base layers.
async fn fetch_base_image_info(
    image_ref: &str,
) -> Result<(Vec<Descriptor>, Vec<String>, ManagedKeys)> {
    let cfg = ClientConfig {
        protocol: oci_client::client::ClientProtocol::Https,
        ..Default::default()
    };
    let client = Client::new(cfg);

    let reference: Reference = image_ref
        .parse()
        .with_context(|| format!("invalid base image reference: {}", image_ref))?;

    let auth = McpmRegistryClient::auth_for(image_ref);

    // Pull manifest as raw bytes and parse with oci-spec for type consistency.
    let (raw_bytes, _digest) = client
        .pull_manifest_raw(
            &reference,
            &auth,
            &[oci_client::manifest::OCI_IMAGE_MEDIA_TYPE],
        )
        .await
        .with_context(|| format!("fetching base image manifest: {}", image_ref))?;

    let manifest = ImageManifest::from_reader(raw_bytes.as_ref())
        .with_context(|| format!("parsing base image manifest: {}", image_ref))?;

    // Fetch the config blob to extract diff_ids.
    let config_desc = manifest.config().clone();
    let config_bytes: Vec<u8> = {
        let config_oci_desc = oci_client::manifest::OciDescriptor {
            media_type: config_desc.media_type().to_string(),
            digest: config_desc.digest().to_string(),
            size: config_desc.size() as i64,
            urls: None,
            annotations: None,
        };
        let mut out = tokio::io::BufWriter::new(AsyncVecWriter::new());
        client
            .pull_blob(&reference, &config_oci_desc, &mut out)
            .await
            .with_context(|| format!("fetching base image config: {}", image_ref))?;
        use tokio::io::AsyncWriteExt;
        out.flush().await.context("flushing config download")?;
        out.into_inner().into_inner()
    };

    // Parse the image config to get diff_ids.
    let diff_ids: Vec<String> = {
        #[derive(serde::Deserialize)]
        struct MinimalConfig {
            rootfs: MinimalRootFs,
        }
        #[derive(serde::Deserialize)]
        struct MinimalRootFs {
            diff_ids: Vec<String>,
        }

        match serde_json::from_slice::<MinimalConfig>(&config_bytes) {
            Ok(cfg) => cfg.rootfs.diff_ids,
            Err(e) => {
                eprintln!(
                    "[build]   warning: could not parse base image config for diff_ids: {}",
                    e
                );
                Vec::new()
            }
        }
    };

    let managed_keys =
        annotations::from_manifest_annotations(manifest.annotations()).unwrap_or_default();

    Ok((manifest.layers().to_vec(), diff_ids, managed_keys))
}

struct AsyncVecWriter {
    buf: Vec<u8>,
}

impl AsyncVecWriter {
    fn new() -> Self {
        AsyncVecWriter { buf: Vec::new() }
    }
    fn into_inner(self) -> Vec<u8> {
        self.buf
    }
}

impl tokio::io::AsyncWrite for AsyncVecWriter {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.buf.extend_from_slice(buf);
        std::task::Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

/// Construct an OCI layer descriptor from a [`PackedLayer`].
fn make_layer_descriptor(packed: &PackedLayer) -> Descriptor {
    use std::str::FromStr as _;
    let hex = packed
        .digest
        .strip_prefix("sha256:")
        .unwrap_or(&packed.digest);
    let sha256 = oci_spec::image::Sha256Digest::from_str(hex)
        .expect("packed layer digest is always valid sha256");
    Descriptor::new(MediaType::ImageLayerGzip, packed.size, sha256)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[allow(dead_code)]
    fn write_temp_file(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn collect_add_single_file() {
        let dir = TempDir::new().unwrap();
        write_temp_file(dir.path(), "MyPlugin.jar", b"jar-bytes");

        let entries =
            collect_add_entries_for_path(&dir.path().join("MyPlugin.jar"), "plugins/MyPlugin.jar")
                .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "plugins/MyPlugin.jar");
        assert_eq!(entries[0].data, b"jar-bytes");
    }

    #[test]
    fn collect_add_directory() {
        let dir = TempDir::new().unwrap();
        write_temp_file(dir.path(), "config/file_a.yml", b"a");
        write_temp_file(dir.path(), "config/file_b.yml", b"b");

        let entries =
            collect_add_entries_for_path(&dir.path().join("config"), "plugins/Config/").unwrap();

        // Should contain both files.
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert!(
            paths.iter().any(|p| p.contains("file_a.yml")),
            "file_a.yml not found in {:?}",
            paths
        );
        assert!(
            paths.iter().any(|p| p.contains("file_b.yml")),
            "file_b.yml not found in {:?}",
            paths
        );
    }

    #[test]
    fn collect_add_nonexistent_path_is_error() {
        let result = collect_add_entries_for_path(Path::new("/nonexistent/path"), "plugins/x/");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn build_single_stage_local_add() {
        let dir = TempDir::new().unwrap();
        write_temp_file(dir.path(), "build/MyPlugin.jar", b"fake-jar-content");

        let bundlefile_content = "FROM scratch\nADD ./build/MyPlugin.jar plugins/MyPlugin.jar\n";
        let bundlefile_path = dir.path().join("Bundlefile");
        std::fs::write(&bundlefile_path, bundlefile_content).unwrap();

        let image = build(&bundlefile_path, &HashMap::new()).await.unwrap();

        // One layer, two blobs (layer + config).
        assert_eq!(image.manifest.layers().len(), 1);
        assert_eq!(image.new_blobs.len(), 2);

        let cfg: serde_json::Value = serde_json::from_slice(&image.config_data).unwrap();
        assert_eq!(cfg["os"], "linux");
        assert_eq!(cfg["rootfs"]["diff_ids"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn build_add_jar_and_config_in_same_layer() {
        let dir = TempDir::new().unwrap();
        write_temp_file(dir.path(), "build/MyPlugin.jar", b"jar");
        write_temp_file(dir.path(), "config/config.yml", b"key: value");

        // Both ADD directives are in one stage → one layer.
        let bundlefile_content = "FROM scratch\n\
                ADD ./build/MyPlugin.jar   plugins/MyPlugin.jar\n\
                ADD ./config/config.yml    plugins/MyPlugin/config.yml\n";
        let bundlefile_path = dir.path().join("Bundlefile");
        std::fs::write(&bundlefile_path, bundlefile_content).unwrap();

        let image = build(&bundlefile_path, &HashMap::new()).await.unwrap();

        // One stage → one layer (all files batched).
        assert_eq!(image.manifest.layers().len(), 1);
    }

    #[tokio::test]
    async fn build_add_dest_path_is_respected() {
        let dir = TempDir::new().unwrap();
        write_temp_file(dir.path(), "build/Sodium.jar", b"sodium-bytes");

        let bundlefile_content = "FROM scratch\nADD ./build/Sodium.jar mods/Sodium.jar\n";
        let bundlefile_path = dir.path().join("Bundlefile");
        std::fs::write(&bundlefile_path, bundlefile_content).unwrap();

        let image = build(&bundlefile_path, &HashMap::new()).await.unwrap();

        let layer_digest = image.manifest.layers()[0].digest().to_string();
        let layer_data = image.get_blob(&layer_digest).unwrap();
        let unpack_dir = TempDir::new().unwrap();
        crate::bundle::layer::unpack_layer(layer_data, unpack_dir.path()).unwrap();

        assert!(
            unpack_dir.path().join("mods/Sodium.jar").exists(),
            "jar should be at mods/Sodium.jar"
        );
        assert!(
            !unpack_dir.path().join("plugins").exists(),
            "plugins/ should not be created"
        );
    }

    #[tokio::test]
    async fn build_local_add_does_not_update_lock() {
        let dir = TempDir::new().unwrap();
        write_temp_file(dir.path(), "build/Local.jar", b"local-jar");

        let bundlefile_content = "FROM scratch\nADD ./build/Local.jar plugins/Local.jar\n";
        let bundlefile_path = dir.path().join("Bundlefile");
        std::fs::write(&bundlefile_path, bundlefile_content).unwrap();

        build(&bundlefile_path, &HashMap::new()).await.unwrap();
        // No lock file interaction expected — just verify build succeeds.
    }

    #[tokio::test]
    async fn build_copy_local_behaves_like_add() {
        let dir = TempDir::new().unwrap();
        write_temp_file(dir.path(), "build/Plugin.jar", b"copy-jar");

        let bundlefile_content = "FROM scratch\nCOPY ./build/Plugin.jar plugins/Plugin.jar\n";
        let bundlefile_path = dir.path().join("Bundlefile");
        std::fs::write(&bundlefile_path, bundlefile_content).unwrap();

        let image = build(&bundlefile_path, &HashMap::new()).await.unwrap();

        let layer_data = image
            .get_blob(image.manifest.layers()[0].digest().as_ref())
            .unwrap();
        let unpack_dir = TempDir::new().unwrap();
        crate::bundle::layer::unpack_layer(layer_data, unpack_dir.path()).unwrap();
        assert!(unpack_dir.path().join("plugins/Plugin.jar").exists());
    }

    #[tokio::test]
    async fn build_copy_from_stage_by_index() {
        let dir = TempDir::new().unwrap();
        write_temp_file(dir.path(), "build/Mod.jar", b"mod-bytes");

        // Stage 0 ADDs Mod.jar; stage 1 COPYs it from stage 0.
        let bundlefile_content = concat!(
            "FROM scratch AS builder\n",
            "ADD ./build/Mod.jar mods/Mod.jar\n",
            "\n",
            "FROM scratch\n",
            "COPY --from=0 mods/Mod.jar mods/Mod.jar\n",
        );
        let bundlefile_path = dir.path().join("Bundlefile");
        std::fs::write(&bundlefile_path, bundlefile_content).unwrap();

        let image = build(&bundlefile_path, &HashMap::new()).await.unwrap();

        // Two stages → two layers.
        assert_eq!(image.manifest.layers().len(), 2);

        // The final layer (stage 1) should contain mods/Mod.jar.
        let last_layer = image.manifest.layers().last().unwrap();
        let layer_data = image.get_blob(last_layer.digest().as_ref()).unwrap();
        let unpack_dir = TempDir::new().unwrap();
        crate::bundle::layer::unpack_layer(layer_data, unpack_dir.path()).unwrap();
        assert!(
            unpack_dir.path().join("mods/Mod.jar").exists(),
            "COPY --from=0 should place file at mods/Mod.jar in stage 1"
        );
    }

    #[tokio::test]
    async fn build_copy_from_named_stage() {
        let dir = TempDir::new().unwrap();
        write_temp_file(dir.path(), "build/Plugin.jar", b"plugin-bytes");

        let bundlefile_content = concat!(
            "FROM scratch AS deps\n",
            "ADD ./build/Plugin.jar plugins/Plugin.jar\n",
            "\n",
            "FROM scratch\n",
            "COPY --from=deps plugins/Plugin.jar plugins/Plugin.jar\n",
        );
        let bundlefile_path = dir.path().join("Bundlefile");
        std::fs::write(&bundlefile_path, bundlefile_content).unwrap();

        let image = build(&bundlefile_path, &HashMap::new()).await.unwrap();

        assert_eq!(image.manifest.layers().len(), 2);

        let last_layer = image.manifest.layers().last().unwrap();
        let layer_data = image.get_blob(last_layer.digest().as_ref()).unwrap();
        let unpack_dir = TempDir::new().unwrap();
        crate::bundle::layer::unpack_layer(layer_data, unpack_dir.path()).unwrap();
        assert!(unpack_dir.path().join("plugins/Plugin.jar").exists());
    }

    #[tokio::test]
    async fn build_manage_sets_annotation() {
        let dir = TempDir::new().unwrap();
        write_temp_file(dir.path(), "build/MyPlugin.jar", b"jar");

        let bundlefile_content = "FROM scratch\n\
                ADD ./build/MyPlugin.jar plugins/MyPlugin.jar\n\
                MANAGE plugins/MyPlugin/config.yml: key.a, key.b\n";
        let bundlefile_path = dir.path().join("Bundlefile");
        std::fs::write(&bundlefile_path, bundlefile_content).unwrap();

        let image = build(&bundlefile_path, &HashMap::new()).await.unwrap();

        let annotations = image.manifest.annotations().as_ref().unwrap();
        let managed = crate::bundle::annotations::decode(
            annotations
                .get(crate::bundle::annotations::MANAGED_KEYS_ANNOTATION)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            managed["plugins/MyPlugin/config.yml"],
            vec!["key.a", "key.b"]
        );
    }

    #[tokio::test]
    async fn build_multistage_annotation_last_writer_wins() {
        let dir = TempDir::new().unwrap();
        write_temp_file(dir.path(), "build/A.jar", b"jar-a");
        write_temp_file(dir.path(), "build/B.jar", b"jar-b");

        let bundlefile_content = "FROM scratch\n\
                ADD ./build/A.jar plugins/A.jar\n\
                MANAGE plugins/A/config.yml: old.key\n\
                \n\
                FROM scratch\n\
                ADD ./build/B.jar plugins/B.jar\n\
                MANAGE plugins/A/config.yml: new.key\n\
                MANAGE plugins/B/config.yml: b.key\n";
        let bundlefile_path = dir.path().join("Bundlefile");
        std::fs::write(&bundlefile_path, bundlefile_content).unwrap();

        let image = build(&bundlefile_path, &HashMap::new()).await.unwrap();

        let annotations = image.manifest.annotations().as_ref().unwrap();
        let managed = crate::bundle::annotations::decode(
            annotations
                .get(crate::bundle::annotations::MANAGED_KEYS_ANNOTATION)
                .unwrap(),
        )
        .unwrap();

        assert_eq!(managed["plugins/A/config.yml"], vec!["new.key"]);
        assert_eq!(managed["plugins/B/config.yml"], vec!["b.key"]);
    }

    #[tokio::test]
    async fn build_arg_substitution_in_add_dest() {
        let dir = TempDir::new().unwrap();
        write_temp_file(dir.path(), "build/Plugin-1.0.jar", b"jar");

        let bundlefile_content = "ARG VERSION=1.0\nFROM scratch\n\
                 ADD ./build/Plugin-${VERSION}.jar plugins/Plugin-${VERSION}.jar\n";
        let bundlefile_path = dir.path().join("Bundlefile");
        std::fs::write(&bundlefile_path, bundlefile_content).unwrap();

        let image = build(&bundlefile_path, &HashMap::new()).await.unwrap();
        assert_eq!(image.manifest.layers().len(), 1);
    }

    #[tokio::test]
    async fn build_arg_cli_override() {
        let dir = TempDir::new().unwrap();
        write_temp_file(dir.path(), "build/Plugin-2.0.jar", b"jar-2");

        let bundlefile_content = "ARG VERSION=1.0\nFROM scratch\n\
                 ADD ./build/Plugin-${VERSION}.jar plugins/Plugin-${VERSION}.jar\n";
        let bundlefile_path = dir.path().join("Bundlefile");
        std::fs::write(&bundlefile_path, bundlefile_content).unwrap();

        let mut overrides = HashMap::new();
        overrides.insert("VERSION".to_string(), "2.0".to_string());

        let image = build(&bundlefile_path, &overrides).await.unwrap();
        assert_eq!(image.manifest.layers().len(), 1);

        let layer_data = image
            .get_blob(image.manifest.layers()[0].digest().as_ref())
            .unwrap();
        let unpack_dir = TempDir::new().unwrap();
        crate::bundle::layer::unpack_layer(layer_data, unpack_dir.path()).unwrap();
        assert!(unpack_dir.path().join("plugins/Plugin-2.0.jar").exists());
    }

    #[tokio::test]
    async fn build_config_blob_in_new_blobs() {
        let dir = TempDir::new().unwrap();
        write_temp_file(dir.path(), "build/P.jar", b"jar");

        let bundlefile_content = "FROM scratch\nADD ./build/P.jar mods/P.jar\n";
        let bundlefile_path = dir.path().join("Bundlefile");
        std::fs::write(&bundlefile_path, bundlefile_content).unwrap();

        let image = build(&bundlefile_path, &HashMap::new()).await.unwrap();

        let config_digest = image.config_digest();
        assert!(image.new_blobs.contains_key(&config_digest));
        assert_eq!(image.new_blobs[&config_digest], image.config_data);
    }

    #[tokio::test]
    async fn build_layer_digests_in_new_blobs() {
        let dir = TempDir::new().unwrap();
        write_temp_file(dir.path(), "build/P.jar", b"jar");

        let bundlefile_content = "FROM scratch\nADD ./build/P.jar plugins/P.jar\n";
        let bundlefile_path = dir.path().join("Bundlefile");
        std::fs::write(&bundlefile_path, bundlefile_content).unwrap();

        let image = build(&bundlefile_path, &HashMap::new()).await.unwrap();

        for layer in image.manifest.layers() {
            let digest = layer.digest().to_string();
            assert!(
                image.new_blobs.contains_key(&digest),
                "layer digest {} not in new_blobs",
                digest
            );
        }
    }

    #[tokio::test]
    async fn build_copy_glob_from_context_flat() {
        // COPY plugins/*.jar output/  — matches multiple files in one directory.
        let dir = TempDir::new().unwrap();
        write_temp_file(dir.path(), "plugins/Foo.jar", b"foo-bytes");
        write_temp_file(dir.path(), "plugins/Bar.jar", b"bar-bytes");
        write_temp_file(dir.path(), "plugins/readme.txt", b"not-a-jar");

        let bundlefile_content = "FROM scratch\nCOPY plugins/*.jar output/\n";
        let bundlefile_path = dir.path().join("Bundlefile");
        std::fs::write(&bundlefile_path, bundlefile_content).unwrap();

        let image = build(&bundlefile_path, &HashMap::new()).await.unwrap();
        let layer_data = image
            .get_blob(image.manifest.layers()[0].digest().as_ref())
            .unwrap();
        let unpack_dir = TempDir::new().unwrap();
        crate::bundle::layer::unpack_layer(layer_data, unpack_dir.path()).unwrap();

        assert!(
            unpack_dir.path().join("output/Foo.jar").exists(),
            "Foo.jar should be at output/Foo.jar"
        );
        assert!(
            unpack_dir.path().join("output/Bar.jar").exists(),
            "Bar.jar should be at output/Bar.jar"
        );
        assert!(
            !unpack_dir.path().join("output/readme.txt").exists(),
            "readme.txt should not match *.jar"
        );
    }

    #[tokio::test]
    async fn build_copy_glob_from_context_deep_prefix_stripped() {
        // Regression test: COPY build/libs/*.jar plugins/
        // The non-wildcard prefix "build/libs/" must be stripped so the jar
        // lands at plugins/<name>.jar, NOT plugins/build/libs/<name>.jar.
        let dir = TempDir::new().unwrap();
        write_temp_file(
            dir.path(),
            "build/libs/mcmetrics-exporter-velocity-0.5.0-rc2.jar",
            b"fake-jar",
        );

        let bundlefile_content =
            "FROM scratch\nCOPY build/libs/mcmetrics-exporter-velocity-*.jar plugins/\n";
        let bundlefile_path = dir.path().join("Bundlefile");
        std::fs::write(&bundlefile_path, bundlefile_content).unwrap();

        let image = build(&bundlefile_path, &HashMap::new()).await.unwrap();
        let layer_data = image
            .get_blob(image.manifest.layers()[0].digest().as_ref())
            .unwrap();
        let unpack_dir = TempDir::new().unwrap();
        crate::bundle::layer::unpack_layer(layer_data, unpack_dir.path()).unwrap();

        assert!(
            unpack_dir
                .path()
                .join("plugins/mcmetrics-exporter-velocity-0.5.0-rc2.jar")
                .exists(),
            "jar must land flat in plugins/ after prefix strip"
        );
        assert!(
            !unpack_dir
                .path()
                .join("plugins/build/libs/mcmetrics-exporter-velocity-0.5.0-rc2.jar")
                .exists(),
            "build/libs/ prefix must not be reproduced under plugins/"
        );
    }

    #[tokio::test]
    async fn build_copy_glob_from_context_recursive() {
        // COPY plugins/**/*.jar output/  — matches files in subdirectories too,
        // preserving the relative directory structure under the glob base.
        let dir = TempDir::new().unwrap();
        write_temp_file(dir.path(), "plugins/Top.jar", b"top");
        write_temp_file(dir.path(), "plugins/sub/Deep.jar", b"deep");

        let bundlefile_content = "FROM scratch\nCOPY plugins/**/*.jar output/\n";
        let bundlefile_path = dir.path().join("Bundlefile");
        std::fs::write(&bundlefile_path, bundlefile_content).unwrap();

        let image = build(&bundlefile_path, &HashMap::new()).await.unwrap();
        let layer_data = image
            .get_blob(image.manifest.layers()[0].digest().as_ref())
            .unwrap();
        let unpack_dir = TempDir::new().unwrap();
        crate::bundle::layer::unpack_layer(layer_data, unpack_dir.path()).unwrap();

        assert!(
            unpack_dir.path().join("output/Top.jar").exists(),
            "top-level jar should be at output/Top.jar"
        );
        assert!(
            unpack_dir.path().join("output/sub/Deep.jar").exists(),
            "nested jar should preserve subdir: output/sub/Deep.jar"
        );
    }

    #[tokio::test]
    async fn build_copy_glob_from_context_no_match_is_error() {
        let dir = TempDir::new().unwrap();
        write_temp_file(dir.path(), "plugins/readme.txt", b"text");

        let bundlefile_content = "FROM scratch\nCOPY plugins/*.jar output/\n";
        let bundlefile_path = dir.path().join("Bundlefile");
        std::fs::write(&bundlefile_path, bundlefile_content).unwrap();

        let result = build(&bundlefile_path, &HashMap::new()).await;
        assert!(result.is_err(), "glob matching no files must be an error");
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("matched no files"),
            "error should mention 'matched no files', got: {msg}"
        );
    }

    #[tokio::test]
    async fn build_copy_glob_from_stage() {
        // COPY --from=0 mods/*.jar output/  — glob matches files in a prior stage.
        let dir = TempDir::new().unwrap();
        write_temp_file(dir.path(), "jars/a.jar", b"a-bytes");
        write_temp_file(dir.path(), "jars/b.jar", b"b-bytes");

        let bundlefile_content = concat!(
            "FROM scratch AS builder\n",
            "ADD ./jars/a.jar mods/a.jar\n",
            "ADD ./jars/b.jar mods/b.jar\n",
            "\n",
            "FROM scratch\n",
            "COPY --from=0 mods/*.jar output/\n",
        );
        let bundlefile_path = dir.path().join("Bundlefile");
        std::fs::write(&bundlefile_path, bundlefile_content).unwrap();

        let image = build(&bundlefile_path, &HashMap::new()).await.unwrap();

        let last_layer = image.manifest.layers().last().unwrap();
        let layer_data = image.get_blob(last_layer.digest().as_ref()).unwrap();
        let unpack_dir = TempDir::new().unwrap();
        crate::bundle::layer::unpack_layer(layer_data, unpack_dir.path()).unwrap();

        assert!(
            unpack_dir.path().join("output/a.jar").exists(),
            "a.jar should be at output/a.jar"
        );
        assert!(
            unpack_dir.path().join("output/b.jar").exists(),
            "b.jar should be at output/b.jar"
        );
    }

    #[tokio::test]
    async fn build_copy_glob_from_stage_no_match_is_error() {
        let dir = TempDir::new().unwrap();
        write_temp_file(dir.path(), "jars/a.jar", b"a-bytes");

        let bundlefile_content = concat!(
            "FROM scratch AS builder\n",
            "ADD ./jars/a.jar mods/a.jar\n",
            "\n",
            "FROM scratch\n",
            "COPY --from=0 plugins/*.jar output/\n",
        );
        let bundlefile_path = dir.path().join("Bundlefile");
        std::fs::write(&bundlefile_path, bundlefile_content).unwrap();

        let result = build(&bundlefile_path, &HashMap::new()).await;
        assert!(
            result.is_err(),
            "glob matching no stage files must be an error"
        );
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("matched no files"),
            "error should mention 'matched no files', got: {msg}"
        );
    }
}
