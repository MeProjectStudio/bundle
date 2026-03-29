/// The reserved keyword for a stage with no base image. `FROM scratch` never
/// triggers a registry lookup.
pub const SCRATCH_STAGE: &str = "scratch";

use std::collections::HashMap;
use std::path::PathBuf;

/// Resolved build arguments (ARG directives + CLI overrides applied).
pub type BuildArgs = HashMap<String, String>;

// ── ADD ───────────────────────────────────────────────────────────────────────

/// The source for an `ADD` directive — either a local path or a remote URL.
#[derive(Debug, Clone)]
pub enum AddSource {
    /// A local filesystem path, relative to the Bundlefile directory.
    Local { path: PathBuf },

    /// A remote HTTP/HTTPS URL.
    Remote {
        url: String,

        /// Optional checksum supplied with `--checksum=sha256:<hex>`.
        ///
        /// When present the downloaded content is verified immediately against
        /// this digest; the build fails on mismatch.
        ///
        /// When absent no digest verification is performed at download time.
        /// The downloaded bytes are packed into an OCI layer whose sha256
        /// digest is recorded in the output image manifest — that manifest
        /// is the authoritative lock file for the build.  `bundle.lock` is
        /// only used by `bundle server` commands, not by `bundle build`.
        checksum: Option<String>,
    },
}

/// A single `ADD` directive.
///
/// Mirrors Docker's `ADD` semantics:
/// - local path → copies file or directory tree into the layer
/// - remote URL → downloads and places at `dest`
/// - `--checksum=sha256:<hex>` (remote only) → verifies content after download
///
/// ```text
/// ADD ./config/Essentials/          plugins/Essentials/
/// ADD https://example.com/Foo.jar   plugins/Foo.jar
/// ADD --checksum=sha256:abc \
///     https://example.com/Bar.jar   mods/Bar.jar
/// ```
#[derive(Debug, Clone)]
pub struct AddDirective {
    pub source: AddSource,
    /// Server-root-relative destination path.
    pub dest: String,
}

// ── COPY ──────────────────────────────────────────────────────────────────────

/// Where a `COPY` directive gets its files from.
#[derive(Debug, Clone)]
pub enum CopyFrom {
    /// Copy from the local build context (no `--from`).
    BuildContext,

    /// Copy from a previous stage in this Bundlefile.
    ///
    /// The string is either:
    /// - a zero-based decimal stage index (`"0"`, `"1"`, …), or
    /// - a stage name declared with `FROM <image> AS <name>`.
    ///
    /// Validated at build time; the parser stores the raw string.
    Stage(String),
}

/// A single `COPY` directive.
///
/// Mirrors Docker's `COPY` semantics:
/// - copies from the local build context by default
/// - `--from=<index|name>` copies from another stage's output
///
/// ```text
/// COPY ./build/MyPlugin.jar              plugins/MyPlugin.jar
/// COPY --from=0  plugins/Foo.jar         plugins/Foo.jar
/// COPY --from=deps  mods/Bar.jar         mods/Bar.jar
/// ```
#[derive(Debug, Clone)]
pub struct CopyDirective {
    /// Where the source files come from.
    pub from: CopyFrom,
    /// Source path (relative to build context, or to the source stage's tree).
    pub src: PathBuf,
    /// Server-root-relative destination path.
    pub dest: String,
}

// ── MANAGE ────────────────────────────────────────────────────────────────────

/// A single `MANAGE` directive — declares which config keys this bundle owns.
///
/// ```text
/// MANAGE plugins/Essentials/config.yml: home.bed-respawn, homes.max-homes
/// ```
#[derive(Debug, Clone)]
pub struct ManageDirective {
    /// Server-root-relative config file path (e.g. `plugins/Essentials/config.yml`).
    pub config_path: String,
    /// Dot-separated key paths owned by this bundle (e.g. `["home.bed-respawn", "homes.max-homes"]`).
    pub keys: Vec<String>,
}

// ── Stage ─────────────────────────────────────────────────────────────────────

/// One stage in a (potentially multi-stage) Bundlefile.
///
/// A new stage begins with each `FROM` directive.
#[derive(Debug, Clone)]
pub struct Stage {
    pub from: String,
    pub name: Option<String>,
    pub adds: Vec<AddDirective>,
    pub copies: Vec<CopyDirective>,
    pub manages: Vec<ManageDirective>,
    /// Key-value labels to embed in the OCI image config (`LABEL` directives).
    /// Later stages override earlier ones for the same key.
    pub labels: HashMap<String, String>,
}

impl Stage {
    pub fn new(from: impl Into<String>, name: Option<String>) -> Self {
        Stage {
            from: from.into(),
            name,
            adds: Vec::new(),
            copies: Vec::new(),
            manages: Vec::new(),
            labels: HashMap::new(),
        }
    }
}

// ── Bundlefile ────────────────────────────────────────────────────────────────

/// The parsed, argument-substituted representation of a `Bundlefile`.
#[derive(Debug, Clone)]
pub struct Bundlefile {
    /// All `ARG` declarations with their resolved values (defaults overridden
    /// by CLI `--build-arg`).
    #[allow(dead_code)]
    pub build_args: BuildArgs,

    /// Stages in declaration order; there is always at least one stage.
    pub stages: Vec<Stage>,
}
