//! Initialisation helpers for `bundle init` and `bundle server init`.
//!
//! These two commands scaffold different files:
//!
//! | Command              | Creates        | Purpose                              |
//! |----------------------|----------------|--------------------------------------|
//! | `bundle init`        | `Bundlefile`   | Define one OCI bundle image          |
//! | `bundle server init` | `bundle.toml`  | Configure bundles on a server        |
//!
//! Both are idempotent — existing files are never overwritten.

use std::path::Path;

use anyhow::{Context, Result};

// ── Templates ─────────────────────────────────────────────────────────────────

const BUNDLEFILE_TEMPLATE: &str = r#"# Bundlefile — defines one OCI bundle image.
#
# Build arguments let you parameterise versions across directives.
ARG VERSION=1.0.0

# Every stage begins with FROM.  Use "scratch" for a brand-new image with no
# base layers, or reference an existing bundle image to inherit its layers.
FROM scratch

# ADD <source> <dest>
#
# Copy a local file/directory or download a remote URL into the bundle.
# The destination is a server-root-relative path.
#
# Local file or directory:
#   ADD ./config/MyPlugin/          plugins/MyPlugin/
#   ADD ./natives/somefile.so       plugins/MyPlugin/somefile.so
#
# Remote URL (sha256 digest recorded in bundle.lock on first build):
#   ADD https://example.com/MyPlugin-${VERSION}.jar   plugins/MyPlugin.jar
#
# Remote URL with explicit checksum (verified immediately):
#   ADD --checksum=sha256:<hex>  https://example.com/MyPlugin.jar  plugins/MyPlugin.jar

# COPY <source> <dest>
#
# Copy from the local build context, or from a previous stage with --from.
#
#   COPY ./build/MyPlugin.jar              plugins/MyPlugin.jar
#   COPY --from=0  plugins/MyPlugin.jar    plugins/MyPlugin.jar
#   COPY --from=deps  mods/Sodium.jar      mods/Sodium.jar

# MANAGE <config-path>: <key>, <key>, ...
#
# Declare which config keys this bundle "owns".  On `bundle server apply` the
# declared keys are taken from the bundle; all other keys in the file keep
# their on-disk values (user edits are preserved).
#
#   MANAGE plugins/MyPlugin/config.yml: setting.enabled, setting.value
"#;

const BUNDLE_TOML_TEMPLATE: &str = r#"# bundle.toml — server bundle manifest.
#
# Commit this file alongside bundle.lock for reproducible server setups.
# Run `bundle server init` in any server directory to create this file.

[server]
# Command executed by `bundle server run` to start the Minecraft server.
run = ["java", "-Xmx4G", "-jar", "server.jar", "nogui"]

[bundles]
# Map logical bundle names to OCI image references.
# Tags are resolved to sha256 digests by `bundle server pull`.
# Semver ranges are supported: "2.4" resolves to the latest 2.4.x release.
#
# essentials = "ghcr.io/someauthor/essentials:v2.20.1"
# luckperms  = "ghcr.io/luckperms/luckperms:^5"
# sodium     = "ghcr.io/jellysquid/sodium:~0.5"
"#;

// ── bundle init ───────────────────────────────────────────────────────────────

/// Run `bundle init` in the current working directory.
///
/// Scaffolds a `Bundlefile` for building a new OCI bundle image.  Does not
/// create `bundle.toml` — use `bundle server init` for that.
pub fn run_bundlefile() -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    init_bundlefile(&cwd)
}

/// Scaffold a `Bundlefile` inside `dir`.
///
/// The file is created only if it does not already exist.
pub fn init_bundlefile(dir: &Path) -> Result<()> {
    let path = dir.join("Bundlefile");

    if path.exists() {
        eprintln!("  skip  Bundlefile  (already exists at {})", path.display());
        println!();
        println!("Bundlefile already exists — nothing to do.");
        return Ok(());
    }

    std::fs::write(&path, BUNDLEFILE_TEMPLATE)
        .with_context(|| format!("writing Bundlefile to {}", path.display()))?;

    println!();
    println!("  created  Bundlefile");
    println!();
    println!("✓ Bundlefile created in {}", dir.display());
    println!();
    println!("Next steps:");
    println!("  1. Edit Bundlefile to declare ADD/COPY sources and MANAGE keys.");
    println!("  2. Run `bundle build` to build the OCI image locally.");
    println!("  3. Run `bundle push <image:tag>` to publish to a registry.");
    println!("  4. Reference the image in a server's bundle.toml.");
    println!("     (Run `bundle server init` in your server directory to create one.)");
    println!();

    Ok(())
}

// ── bundle server init ────────────────────────────────────────────────────────

/// Run `bundle server init` in the current working directory.
///
/// Scaffolds a `bundle.toml` for managing OCI bundles on a Minecraft server.
/// Does not create a `Bundlefile` — use `bundle init` for that.
pub fn run_server_config() -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    init_server_config(&cwd)
}

/// Scaffold a `bundle.toml` inside `dir`.
///
/// The file is created only if it does not already exist.
pub fn init_server_config(dir: &Path) -> Result<()> {
    let path = dir.join("bundle.toml");

    if path.exists() {
        eprintln!(
            "  skip  bundle.toml  (already exists at {})",
            path.display()
        );
        println!();
        println!("bundle.toml already exists — nothing to do.");
        return Ok(());
    }

    std::fs::write(&path, BUNDLE_TOML_TEMPLATE)
        .with_context(|| format!("writing bundle.toml to {}", path.display()))?;

    println!();
    println!("  created  bundle.toml");
    println!();
    println!("✓ bundle.toml created in {}", dir.display());
    println!();
    println!("Next steps:");
    println!("  1. Edit bundle.toml to add bundle image references under [bundles].");
    println!("  2. Run `bundle server pull`  — resolve tags and download layer blobs.");
    println!("  3. Run `bundle server apply` — extract bundles onto the server directory.");
    println!("  4. Run `bundle server run`   — apply + start the server process.");
    println!();
    println!("Tip: build your own bundles with `bundle init` + `bundle build`.");
    println!();

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── init_bundlefile ───────────────────────────────────────────────────────

    #[test]
    fn creates_bundlefile_in_empty_dir() {
        let dir = TempDir::new().unwrap();
        init_bundlefile(dir.path()).unwrap();
        assert!(
            dir.path().join("Bundlefile").exists(),
            "Bundlefile should be created"
        );
    }

    #[test]
    fn bundlefile_does_not_create_bundle_toml() {
        let dir = TempDir::new().unwrap();
        init_bundlefile(dir.path()).unwrap();
        assert!(
            !dir.path().join("bundle.toml").exists(),
            "`bundle init` must not create bundle.toml"
        );
    }

    #[test]
    fn bundlefile_contains_from_and_add() {
        let dir = TempDir::new().unwrap();
        init_bundlefile(dir.path()).unwrap();
        let content = std::fs::read_to_string(dir.path().join("Bundlefile")).unwrap();
        assert!(
            content.contains("FROM scratch"),
            "should contain FROM scratch"
        );
        assert!(content.contains("ADD"), "should contain ADD example");
        assert!(content.contains("MANAGE"), "should contain MANAGE example");
    }

    #[test]
    fn bundlefile_idempotent() {
        let dir = TempDir::new().unwrap();
        init_bundlefile(dir.path()).unwrap();
        let original = std::fs::read(dir.path().join("Bundlefile")).unwrap();

        // Second call must not overwrite the file.
        init_bundlefile(dir.path()).unwrap();
        let after = std::fs::read(dir.path().join("Bundlefile")).unwrap();
        assert_eq!(
            original, after,
            "Bundlefile must not be modified on second init"
        );
    }

    #[test]
    fn bundlefile_not_overwritten_if_modified() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("Bundlefile");
        std::fs::write(&path, b"FROM my-custom-base:v1\nADD ./jar.jar plugins/\n").unwrap();

        init_bundlefile(dir.path()).unwrap();

        let content = std::fs::read(&path).unwrap();
        assert_eq!(
            content, b"FROM my-custom-base:v1\nADD ./jar.jar plugins/\n",
            "existing Bundlefile must not be overwritten"
        );
    }

    // ── init_server_config ────────────────────────────────────────────────────

    #[test]
    fn creates_bundle_toml_in_empty_dir() {
        let dir = TempDir::new().unwrap();
        init_server_config(dir.path()).unwrap();
        assert!(
            dir.path().join("bundle.toml").exists(),
            "bundle.toml should be created"
        );
    }

    #[test]
    fn server_init_does_not_create_bundlefile() {
        let dir = TempDir::new().unwrap();
        init_server_config(dir.path()).unwrap();
        assert!(
            !dir.path().join("Bundlefile").exists(),
            "`bundle server init` must not create a Bundlefile"
        );
    }

    #[test]
    fn bundle_toml_contains_server_and_bundles_sections() {
        let dir = TempDir::new().unwrap();
        init_server_config(dir.path()).unwrap();
        let content = std::fs::read_to_string(dir.path().join("bundle.toml")).unwrap();
        assert!(content.contains("[server]"), "should contain [server]");
        assert!(content.contains("[bundles]"), "should contain [bundles]");
        assert!(
            content.contains("run ="),
            "should contain server.run example"
        );
    }

    #[test]
    fn bundle_toml_is_valid_toml() {
        let dir = TempDir::new().unwrap();
        init_server_config(dir.path()).unwrap();

        let raw = std::fs::read_to_string(dir.path().join("bundle.toml")).unwrap();
        // Strip comment lines before parsing — some TOML parsers are strict.
        let without_comments: String = raw
            .lines()
            .map(|l| {
                if l.trim_start().starts_with('#') {
                    ""
                } else {
                    l
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        toml::from_str::<toml::Value>(&without_comments)
            .expect("bundle.toml (without comments) should be valid TOML");
    }

    #[test]
    fn bundle_toml_idempotent() {
        let dir = TempDir::new().unwrap();
        init_server_config(dir.path()).unwrap();
        let original = std::fs::read(dir.path().join("bundle.toml")).unwrap();

        init_server_config(dir.path()).unwrap();
        let after = std::fs::read(dir.path().join("bundle.toml")).unwrap();
        assert_eq!(
            original, after,
            "bundle.toml must not be modified on second init"
        );
    }

    #[test]
    fn bundle_toml_not_overwritten_if_modified() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bundle.toml");
        std::fs::write(
            &path,
            b"[server]\nrun = [\"java\"]\n[bundles]\nfoo = \"ghcr.io/foo:v1\"\n",
        )
        .unwrap();

        init_server_config(dir.path()).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("foo"),
            "existing bundle.toml must not be overwritten"
        );
    }

    // ── independence ──────────────────────────────────────────────────────────

    #[test]
    fn both_inits_can_run_independently_in_same_dir() {
        let dir = TempDir::new().unwrap();
        init_bundlefile(dir.path()).unwrap();
        init_server_config(dir.path()).unwrap();

        assert!(dir.path().join("Bundlefile").exists());
        assert!(dir.path().join("bundle.toml").exists());
    }

    #[test]
    fn server_init_then_bundlefile_init_does_not_overwrite_either() {
        let dir = TempDir::new().unwrap();
        init_server_config(dir.path()).unwrap();
        let toml_before = std::fs::read(dir.path().join("bundle.toml")).unwrap();

        init_bundlefile(dir.path()).unwrap();
        let toml_after = std::fs::read(dir.path().join("bundle.toml")).unwrap();

        assert_eq!(
            toml_before, toml_after,
            "bundle.toml must not be touched by bundlefile init"
        );
        assert!(dir.path().join("Bundlefile").exists());
    }
}
