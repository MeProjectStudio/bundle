//! `bundle` — Minecraft server bundle manager via OCI containers.
//!
//! Two command groups, mirroring `docker` + `docker compose`:
//!
//! ```sh
//! # ── Image authoring (like docker build/push) ─────────────────────────────
//! bundle init                           # scaffold a Bundlefile
//! bundle build                          # build OCI image from Bundlefile
//! bundle push ghcr.io/me/plugin:v1      # publish to a registry
//!
//! # ── Server management (like docker compose) ──────────────────────────────
//! bundle server init                    # scaffold bundle.toml in server dir
//! bundle server pull                    # resolve tags → digests, cache blobs
//! bundle server apply                   # pull + extract bundles onto server FS
//! bundle server diff                    # preview what apply would change
//! bundle server run                     # apply + start server (compose up)
//! ```

mod apply;
mod bundle;
mod bundlefile;
mod cmd;
mod project;
mod registry;
mod util;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueHint};

// ── CLI definition ────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "bundle",
    bin_name = "bundle",
    about = "Minecraft server bundle manager via OCI containers",
    long_about = "Two command groups, mirroring `docker` + `docker compose`:\n\n\
                  Image authoring  →  bundle init / build / push\n\
                  Server management →  bundle server init / pull / apply / diff / run",
    version,
    author
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

// ── Root commands (image authoring) ──────────────────────────────────────────

#[derive(Subcommand, Debug)]
enum Commands {
    /// Print version and git revision information.
    ///
    /// Shows the release version from Cargo.toml and the git commit hash
    /// embedded at compile time via `git describe`.
    Version,

    /// Scaffold a Bundlefile in the current directory.
    ///
    /// Creates a commented `Bundlefile` template ready to be filled in with
    /// ADD, COPY, and MANAGE directives.  Does not create bundle.toml — use
    /// `bundle server init` for that.
    Init,

    /// Build an OCI bundle image from a Bundlefile.
    ///
    /// Like `docker build -t myorg/myplugin:latest .` — fetches or copies all
    /// declared sources, packs them into gzip-compressed OCI layers, writes the
    /// image to the local cache (~/.cache/bundle/), and optionally pushes it to
    /// one or more registry tags.
    ///
    /// ## Examples
    ///
    /// ```sh
    /// bundle build .
    /// bundle build -t ghcr.io/me/myplugin:latest .
    /// bundle build -t ghcr.io/me/myplugin:latest -t ghcr.io/me/myplugin:nightly .
    /// bundle build --bundlefile path/to/Bundlefile
    /// ```
    Build {
        /// Override a Bundlefile ARG: --build-arg KEY=VALUE.
        ///
        /// May be specified multiple times.  Takes precedence over the
        /// ARG defaults declared in the Bundlefile.
        #[arg(
            long = "build-arg",
            value_name = "KEY[=VALUE]",
            value_parser = parse_key_val,
            action = clap::ArgAction::Append,
        )]
        build_arg: Vec<(String, String)>,

        /// Tag the built image and push it to the registry.
        ///
        /// May be specified multiple times to push to several tags at once:
        ///   -t ghcr.io/me/myplugin:latest -t ghcr.io/me/myplugin:v1.2.0
        ///
        /// Equivalent to running `bundle push <IMAGE:TAG>` for each tag after
        /// a successful build.  The image is always stored in the local cache
        /// (~/.cache/bundle/built/) regardless of whether -t is used, so you
        /// can push to additional tags later with `bundle push`.
        #[arg(
            short = 't',
            long = "tag",
            value_name = "IMAGE:TAG",
            action = clap::ArgAction::Append,
        )]
        tag: Vec<String>,

        /// Build context directory.  The Bundlefile is looked up inside this
        /// directory.  Defaults to the current directory, mirroring
        /// `docker build .`
        #[arg(value_name = "PATH", value_hint = ValueHint::DirPath)]
        path: Option<PathBuf>,

        /// Explicit path to the Bundlefile, overriding auto-detection from
        /// the build context directory.
        #[arg(
            long,
            value_name = "FILE",
            value_hint = ValueHint::FilePath,
        )]
        bundlefile: Option<PathBuf>,
    },

    /// Push the most recently built image to an OCI registry.
    ///
    /// The image must have been built first with `bundle build`.
    ///
    /// ## Authentication
    ///
    /// Credentials are resolved from environment variables or
    /// ~/.docker/config.json.  Set GHCR_IO_USERNAME + GHCR_IO_PASSWORD for
    /// ghcr.io, or REGISTRY_USERNAME + REGISTRY_PASSWORD generically.
    Push {
        /// The fully-qualified OCI image reference to push to.
        ///
        /// Examples:
        ///   ghcr.io/someauthor/essentials:v2.20.1
        ///   docker.io/myorg/my-server-bundle:latest
        #[arg(value_name = "IMAGE:TAG", value_hint = ValueHint::Other)]
        image_tag: String,
    },

    // ── Server subcommand group ───────────────────────────────────────────────
    /// Manage OCI bundles on a Minecraft server.
    ///
    /// Like `docker compose` but for Minecraft — pulls bundle images defined
    /// in bundle.toml, extracts them onto the server filesystem, and manages
    /// the server process.
    ///
    /// Run `bundle server --help` for a list of subcommands.
    #[command(subcommand)]
    Server(ServerCommands),
}

// ── Server subcommands (server management) ────────────────────────────────────

#[derive(Subcommand, Debug)]
enum ServerCommands {
    /// Scaffold a bundle.toml in the current directory.
    ///
    /// Creates a commented `bundle.toml` template with a [server] section and
    /// an empty [bundles] section ready to be filled in with OCI image
    /// references.  Does not create a Bundlefile — use `bundle init` for that.
    Init,

    /// Like `docker compose pull` — resolve tags and download layer blobs.
    ///
    /// Resolves all bundle tags in bundle.toml to sha256 digests (semver
    /// ranges are resolved to the highest matching tag), downloads all layer
    /// blobs into the local cache (~/.cache/bundle/), and writes bundle.lock.
    ///
    /// **No filesystem changes** — only the local blob cache and bundle.lock
    /// are updated.  Use `bundle server apply` to also extract layers onto the
    /// server, or `bundle server run` to do everything at once.
    Pull,

    /// Pull new bundle versions then extract them onto the server filesystem.
    ///
    /// Like `docker compose pull` + installing — resolves tags, downloads
    /// layer blobs, extracts them onto the server directory, and merges config
    /// files using the MANAGE annotations.
    ///
    /// Pull is always run first so bundle.lock stays up to date.  Use
    /// `--no-pull` to skip network access and apply from the local cache only.
    /// Use `bundle server run` to also start the server afterwards.
    Apply {
        /// Use this directory as the server root instead of $PWD.
        #[arg(long, value_name = "PATH", value_hint = ValueHint::DirPath)]
        server_dir: Option<PathBuf>,

        /// Skip the automatic pull step and apply from the local cache only.
        /// Useful for offline workflows or when a separate `bundle server pull`
        /// has already been run.
        #[arg(long)]
        no_pull: bool,
    },

    /// Preview what `bundle server apply` would change, without writing anything.
    ///
    /// Pulls current bundle state from the registry first (same as apply),
    /// then reports files that would be created, overwritten, merged, or
    /// deleted — without touching the server directory.
    ///
    /// Exit code is always 0 regardless of whether changes are detected.
    /// Pass `--no-pull` to diff against the local cache only.
    Diff {
        /// Skip the automatic pull step and diff against the local cache only.
        #[arg(long)]
        no_pull: bool,
    },

    /// Like `docker compose up` — apply bundles then start the server.
    ///
    /// Runs `bundle server apply` (pull + extract onto FS) then replaces the
    /// current process with the `server.run` command declared in bundle.toml.
    ///
    /// On Unix the server process replaces the bundle process via execvp(2),
    /// inheriting the same PID and signal handlers — ideal for container
    /// entrypoints.  On non-Unix systems a child process is spawned instead.
    Run {
        /// Skip the pull step (apply from local cache, then start server).
        /// Same as `bundle server apply --no-pull` + exec.
        #[arg(long)]
        no_pull: bool,

        /// Skip apply entirely and exec the server immediately.
        /// Useful when bundles are already up-to-date and you just want to
        /// (re)start the server process.
        #[arg(long)]
        no_apply: bool,

        /// Use this directory as the server root instead of $PWD.
        #[arg(long, value_name = "PATH", value_hint = ValueHint::DirPath)]
        server_dir: Option<PathBuf>,
    },
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        // Print the full error chain for actionable diagnostics.
        eprintln!("error: {:#}", e);
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        // ── version ───────────────────────────────────────────────────────────
        Commands::Version => {
            cmd::version::run();
        }

        // ── init ──────────────────────────────────────────────────────────────
        Commands::Init => {
            cmd::init::run_bundlefile().context("bundle init failed")?;
        }

        // ── build ─────────────────────────────────────────────────────────────
        Commands::Build {
            build_arg,
            tag,
            path,
            bundlefile,
        } => {
            cmd::build::run(cmd::build::BuildArgs {
                build_args: build_arg,
                tags: tag,
                context: path,
                bundlefile,
            })
            .await
            .context("bundle build failed")?;
        }

        // ── push ──────────────────────────────────────────────────────────────
        Commands::Push { image_tag } => {
            cmd::push::run(cmd::push::PushArgs {
                image_ref: image_tag,
            })
            .await
            .context("bundle push failed")?;
        }

        // ── server subcommands ────────────────────────────────────────────────
        Commands::Server(server_cmd) => match server_cmd {
            // bundle server init
            ServerCommands::Init => {
                cmd::init::run_server_config().context("bundle server init failed")?;
            }

            // bundle server pull
            ServerCommands::Pull => {
                cmd::pull::run()
                    .await
                    .context("bundle server pull failed")?;
            }

            // bundle server apply
            ServerCommands::Apply {
                server_dir,
                no_pull,
            } => {
                cmd::apply::run(cmd::apply::ApplyArgs {
                    server_dir,
                    dry_run: false,
                    no_pull,
                })
                .await
                .context("bundle server apply failed")?;
            }

            // bundle server diff
            ServerCommands::Diff { no_pull } => {
                cmd::diff::run(no_pull)
                    .await
                    .context("bundle server diff failed")?;
            }

            // bundle server run
            ServerCommands::Run {
                no_pull,
                no_apply,
                server_dir,
            } => {
                cmd::run::run(cmd::run::RunArgs {
                    no_pull,
                    no_apply,
                    server_dir,
                })
                .await
                .context("bundle server run failed")?;
            }
        },
    }

    Ok(())
}

// ── Value parsers ─────────────────────────────────────────────────────────────

/// Parse a `KEY=VALUE` string into `(key, value)`.
///
/// The split is on the first `=` only, so values that contain `=` are handled
/// correctly.
/// Parse a `--build-arg` value from the command line.
///
/// Accepted forms (mirroring Docker):
///
/// - `KEY=VALUE` — use `VALUE` verbatim.
/// - `KEY=`      — explicit empty string (overrides any Bundlefile default).
/// - `KEY`       — no value: look up `$KEY` from the host environment.
///                 If the variable is set, use its value.
///                 If it is not set, the key is still added to the overrides
///                 map with an empty string, which shadows the Bundlefile
///                 `ARG KEY=default`.  This matches `docker build --build-arg KEY`
///                 behaviour when the env var is absent.
fn parse_key_val(s: &str) -> Result<(String, String), String> {
    if let Some((key, value)) = s.split_once('=') {
        if key.is_empty() {
            return Err(format!("KEY must not be empty in build-arg {:?}", s));
        }
        Ok((key.to_string(), value.to_string()))
    } else {
        // Bare KEY — attempt an environment lookup.
        let key = s.trim();
        if key.is_empty() {
            return Err("build-arg key must not be empty".to_string());
        }
        let value = std::env::var(key).unwrap_or_default();
        Ok((key.to_string(), value))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_key_val_simple() {
        assert_eq!(
            parse_key_val("VERSION=2.20.1").unwrap(),
            ("VERSION".to_string(), "2.20.1".to_string())
        );
    }

    #[test]
    fn parse_key_val_value_contains_equals() {
        // Only the FIRST `=` is the separator.
        assert_eq!(
            parse_key_val("URL=https://example.com/path?a=1&b=2").unwrap(),
            (
                "URL".to_string(),
                "https://example.com/path?a=1&b=2".to_string()
            )
        );
    }

    #[test]
    fn parse_key_val_empty_value() {
        assert_eq!(
            parse_key_val("FLAG=").unwrap(),
            ("FLAG".to_string(), String::new())
        );
    }

    #[test]
    fn parse_key_val_bare_key_found_in_env() {
        // Bare KEY should pick up the value from the host environment.
        std::env::set_var("MCPM_TEST_BUILD_ARG_12345", "from_env");
        let result = parse_key_val("MCPM_TEST_BUILD_ARG_12345").unwrap();
        std::env::remove_var("MCPM_TEST_BUILD_ARG_12345");
        assert_eq!(result.0, "MCPM_TEST_BUILD_ARG_12345");
        assert_eq!(result.1, "from_env");
    }

    #[test]
    fn parse_key_val_bare_key_not_in_env_gives_empty() {
        // When the env var is absent, the value should be an empty string
        // (the override is still recorded, shadowing the Bundlefile default).
        std::env::remove_var("MCPM_TEST_BUILD_ARG_ABSENT_99999");
        let result = parse_key_val("MCPM_TEST_BUILD_ARG_ABSENT_99999").unwrap();
        assert_eq!(result.0, "MCPM_TEST_BUILD_ARG_ABSENT_99999");
        assert_eq!(result.1, "");
    }

    #[test]
    fn parse_key_val_empty_key_is_error() {
        assert!(parse_key_val("=value").is_err());
    }

    #[test]
    fn parse_key_val_spaces_in_value() {
        let (k, v) = parse_key_val("KEY=hello world").unwrap();
        assert_eq!(k, "KEY");
        assert_eq!(v, "hello world");
    }

    // ── Root commands ─────────────────────────────────────────────────────────

    #[test]
    fn cli_init_parses() {
        let cli = Cli::try_parse_from(["bundle", "init"]);
        assert!(cli.is_ok(), "bundle init should parse: {:?}", cli);
        assert!(matches!(cli.unwrap().command, Commands::Init));
    }

    #[test]
    #[test]
    fn cli_version_parses() {
        let cli = Cli::try_parse_from(["bundle", "version"]);
        assert!(cli.is_ok(), "bundle version should parse: {:?}", cli);
        assert!(matches!(cli.unwrap().command, Commands::Version));
    }

    #[test]
    fn cli_build_no_args_parses() {
        let cli = Cli::try_parse_from(["bundle", "build"]);
        assert!(cli.is_ok(), "bundle build should parse: {:?}", cli);
    }

    #[test]
    fn cli_build_with_path_parses() {
        let cli = Cli::try_parse_from(["bundle", "build", "."]);
        assert!(cli.is_ok());
        if let Ok(Cli {
            command: Commands::Build { path, .. },
        }) = cli
        {
            assert_eq!(path, Some(PathBuf::from(".")));
        }
    }

    #[test]
    fn cli_build_single_tag_parses() {
        let cli = Cli::try_parse_from(["bundle", "build", "-t", "ghcr.io/me/plugin:latest"]);
        assert!(cli.is_ok());
        if let Ok(Cli {
            command: Commands::Build { tag, .. },
        }) = cli
        {
            assert_eq!(tag, vec!["ghcr.io/me/plugin:latest".to_string()]);
        }
    }

    #[test]
    fn cli_build_multiple_tags_parses() {
        let cli = Cli::try_parse_from([
            "bundle",
            "build",
            "-t",
            "ghcr.io/me/plugin:latest",
            "-t",
            "ghcr.io/me/plugin:nightly",
            ".",
        ]);
        assert!(cli.is_ok());
        if let Ok(Cli {
            command: Commands::Build { tag, path, .. },
        }) = cli
        {
            assert_eq!(tag.len(), 2);
            assert!(tag.contains(&"ghcr.io/me/plugin:latest".to_string()));
            assert!(tag.contains(&"ghcr.io/me/plugin:nightly".to_string()));
            assert_eq!(path, Some(PathBuf::from(".")));
        }
    }

    #[test]
    fn cli_build_with_build_arg_parses() {
        let cli = Cli::try_parse_from(["bundle", "build", "--build-arg", "VERSION=2.20.1"]);
        assert!(
            cli.is_ok(),
            "bundle build --build-arg should parse: {:?}",
            cli
        );
        if let Ok(Cli {
            command: Commands::Build { build_arg, .. },
        }) = cli
        {
            assert_eq!(
                build_arg,
                vec![("VERSION".to_string(), "2.20.1".to_string())]
            );
        }
    }

    #[test]
    fn cli_build_multiple_build_args_parses() {
        let cli = Cli::try_parse_from([
            "bundle",
            "build",
            "--build-arg",
            "A=1",
            "--build-arg",
            "B=2",
        ]);
        assert!(cli.is_ok());
        if let Ok(Cli {
            command: Commands::Build { build_arg, .. },
        }) = cli
        {
            assert_eq!(build_arg.len(), 2);
        }
    }

    #[test]
    fn cli_build_with_bundlefile_parses() {
        let cli = Cli::try_parse_from(["bundle", "build", "--bundlefile", "path/to/Bundlefile"]);
        assert!(cli.is_ok());
        if let Ok(Cli {
            command: Commands::Build { bundlefile, .. },
        }) = cli
        {
            assert_eq!(bundlefile, Some(PathBuf::from("path/to/Bundlefile")));
        }
    }

    #[test]
    fn cli_push_parses() {
        let cli = Cli::try_parse_from(["bundle", "push", "ghcr.io/me/bundle:v1"]);
        assert!(cli.is_ok(), "bundle push should parse: {:?}", cli);
        if let Ok(Cli {
            command: Commands::Push { image_tag },
        }) = cli
        {
            assert_eq!(image_tag, "ghcr.io/me/bundle:v1");
        }
    }

    #[test]
    fn cli_push_requires_image_tag() {
        let cli = Cli::try_parse_from(["bundle", "push"]);
        assert!(cli.is_err(), "bundle push with no argument should fail");
    }

    // ── Server subcommands ────────────────────────────────────────────────────

    #[test]
    fn cli_server_init_parses() {
        assert!(Cli::try_parse_from(["bundle", "server", "init"]).is_ok());
    }

    #[test]
    fn cli_server_pull_parses() {
        assert!(Cli::try_parse_from(["bundle", "server", "pull"]).is_ok());
    }

    #[test]
    fn cli_server_apply_parses() {
        assert!(Cli::try_parse_from(["bundle", "server", "apply"]).is_ok());
    }

    #[test]
    fn cli_server_apply_with_server_dir_parses() {
        let cli = Cli::try_parse_from([
            "bundle",
            "server",
            "apply",
            "--server-dir",
            "/srv/minecraft",
        ]);
        assert!(cli.is_ok());
        if let Ok(Cli {
            command: Commands::Server(ServerCommands::Apply { server_dir, .. }),
        }) = cli
        {
            assert_eq!(server_dir, Some(PathBuf::from("/srv/minecraft")));
        }
    }

    #[test]
    fn cli_server_apply_no_pull_parses() {
        let cli = Cli::try_parse_from(["bundle", "server", "apply", "--no-pull"]);
        assert!(cli.is_ok());
        if let Ok(Cli {
            command: Commands::Server(ServerCommands::Apply { no_pull, .. }),
        }) = cli
        {
            assert!(no_pull);
        }
    }

    #[test]
    fn cli_server_diff_parses() {
        assert!(Cli::try_parse_from(["bundle", "server", "diff"]).is_ok());
    }

    #[test]
    fn cli_server_diff_no_pull_parses() {
        let cli = Cli::try_parse_from(["bundle", "server", "diff", "--no-pull"]);
        assert!(cli.is_ok());
        if let Ok(Cli {
            command: Commands::Server(ServerCommands::Diff { no_pull }),
        }) = cli
        {
            assert!(no_pull);
        }
    }

    #[test]
    fn cli_server_run_parses() {
        assert!(Cli::try_parse_from(["bundle", "server", "run"]).is_ok());
    }

    #[test]
    fn cli_server_run_with_flags_parses() {
        let cli = Cli::try_parse_from([
            "bundle",
            "server",
            "run",
            "--no-pull",
            "--no-apply",
            "--server-dir",
            "/s",
        ]);
        assert!(cli.is_ok());
        if let Ok(Cli {
            command:
                Commands::Server(ServerCommands::Run {
                    no_pull,
                    no_apply,
                    server_dir,
                }),
        }) = cli
        {
            assert!(no_pull);
            assert!(no_apply);
            assert_eq!(server_dir, Some(PathBuf::from("/s")));
        }
    }

    #[test]
    fn old_pull_apply_run_no_longer_exist_at_root() {
        // These commands have moved under `bundle server`.
        assert!(Cli::try_parse_from(["bundle", "pull"]).is_err());
        assert!(Cli::try_parse_from(["bundle", "apply"]).is_err());
        assert!(Cli::try_parse_from(["bundle", "run"]).is_err());
        assert!(Cli::try_parse_from(["bundle", "diff"]).is_err());
    }
}
