use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The `[server]` section of `bundle.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Command to exec when running the server (e.g. `["java", "-jar", "server.jar"]`).
    pub run: Vec<String>,

    /// Server-root-relative paths that OCI images must never overwrite.
    ///
    /// Any bundle that contains one of these paths triggers either a hard
    /// failure (default) or a skip-and-warn (with
    /// `--ignore-dangerous-override-attempts`).
    ///
    /// The default list covers the bundle binary itself and the two project
    /// files that `bundle` manages, plus the server jar.
    #[serde(rename = "deny-override", default = "default_deny_override")]
    pub deny_override: Vec<String>,
}

/// The default set of paths that bundles are not allowed to overwrite.
fn default_deny_override() -> Vec<String> {
    vec![
        "bundle".into(),
        "bundle.exe".into(),
        "bundle.lock".into(),
        "bundle.toml".into(),
        "server.jar".into(),
    ]
}

/// Top-level `bundle.toml` project manifest.
///
/// ```toml
/// [server]
/// run = ["java", "-Xmx4G", "-jar", "server.jar", "nogui"]
///
/// bundles = [
///   "ghcr.io/someauthor/essentials:v2.20.1",
///   "ghcr.io/me/bundle:latest",
/// ]
/// ```
///
/// ```toml
/// [server]
/// run = ["java", "-Xmx4G", "-jar", "server.jar", "nogui"]
///
/// [bundles]
/// essentials = "ghcr.io/someauthor/essentials:v2.20.1"
/// my-config  = "ghcr.io/me/my-server-bundle:latest"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub server: ServerConfig,

    /// OCI image references to apply to the server.
    #[serde(default)]
    pub bundles: Vec<String>,
}

impl ProjectConfig {
    /// Load `bundle.toml` from `dir` (normally the current working directory).
    pub fn load_from(dir: &Path) -> Result<Self> {
        let path = dir.join("bundle.toml");
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: ProjectConfig =
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        Ok(cfg)
    }

    /// Load `bundle.toml` from the current working directory.
    pub fn load() -> Result<Self> {
        let cwd = std::env::current_dir().context("getting current directory")?;
        Self::load_from(&cwd)
    }

    /// Canonical path of the `bundle.toml` file relative to `dir`.
    #[allow(dead_code)]
    pub fn path_in(dir: &Path) -> PathBuf {
        dir.join("bundle.toml")
    }

    /// Write this config back to `bundle.toml` in `dir`.
    #[allow(dead_code)]
    pub fn save_to(&self, dir: &Path) -> Result<()> {
        let path = dir.join("bundle.toml");
        let raw = toml::to_string_pretty(self).context("serialising project config")?;
        fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn bundles_before_server_section_parses() {
        let dir = TempDir::new().unwrap();
        let content = r#"
bundles = [
  "ghcr.io/me/plugin:latest",
  "registry.example.com/ns/mod:v1",
]

[server]
run = ["java", "-jar", "server.jar"]
"#;
        std::fs::write(dir.path().join("bundle.toml"), content).unwrap();
        let cfg = ProjectConfig::load_from(dir.path()).unwrap();
        assert_eq!(cfg.bundles.len(), 2, "bundles before [server] must parse");
        assert!(cfg
            .bundles
            .contains(&"ghcr.io/me/plugin:latest".to_string()));
    }

    #[test]
    fn bundles_after_server_section_is_invisible() {
        // TOML scoping: keys after [server] belong to the server table,
        // not the root — so bundles ends up empty (the serde default).
        let dir = TempDir::new().unwrap();
        let content = r#"
[server]
run = ["java", "-jar", "server.jar"]

bundles = [
  "ghcr.io/me/plugin:latest",
]
"#;
        std::fs::write(dir.path().join("bundle.toml"), content).unwrap();
        let cfg = ProjectConfig::load_from(dir.path()).unwrap();
        assert_eq!(
            cfg.bundles.len(),
            0,
            "bundles after [server] is scoped to server, not root"
        );
    }

    #[test]
    fn empty_bundles_array_is_valid() {
        let dir = TempDir::new().unwrap();
        let content = r#"
bundles = []

[server]
run = ["java", "-jar", "server.jar"]
"#;
        std::fs::write(dir.path().join("bundle.toml"), content).unwrap();
        let cfg = ProjectConfig::load_from(dir.path()).unwrap();
        assert!(cfg.bundles.is_empty());
    }
}
