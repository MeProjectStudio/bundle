use bundle::cmd;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueHint};

// ── CLI definition ────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "bundle",
    bin_name = "bundle",
    about = "Minecraft server bundle manager via OCI containers",
    author
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Print version and git revision.
    Version,

    /// Log in to a container registry and save credentials.
    ///
    /// Credentials are written to `$XDG_RUNTIME_DIR/containers/auth.json`
    /// (Linux) or `$HOME/.config/containers/auth.json`.
    /// Override with `--authfile` or `REGISTRY_AUTH_FILE`.
    Login {
        /// Registry to log in to. May include a path: `registry.example.com/private`.
        #[arg(value_name = "REGISTRY")]
        registry: String,

        #[arg(short = 'u', long)]
        username: Option<String>,

        #[arg(short = 'p', long)]
        password: Option<String>,

        /// Read password from stdin.
        #[arg(long)]
        password_stdin: bool,

        /// Path to the auth file. Overrides the default and `REGISTRY_AUTH_FILE`.
        #[arg(long, value_name = "PATH", value_hint = ValueHint::FilePath)]
        authfile: Option<PathBuf>,
    },

    /// Scaffold a Bundlefile in the current directory.
    Init,

    /// Build an OCI bundle image from a Bundlefile.
    ///
    /// Fetches or copies all declared sources, packs them into OCI layers,
    /// and stores the result in the local cache.
    ///
    /// Use -t to tag the result:
    ///
    ///   -t myplugin:latest           store as a local tag (no registry needed)
    ///   -t ghcr.io/org/plugin:1.0   tag and push to a registry immediately
    ///
    /// Both forms may be combined in a single invocation.
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

        /// Tag the built image. May be repeated.
        ///
        /// Bare names (no registry hostname) are stored as local tags and
        /// can be referenced directly in bundle.toml without a registry:
        ///   -t myplugin:latest
        ///
        /// Names with a registry hostname are pushed to that registry:
        ///   -t ghcr.io/org/plugin:latest
        ///
        /// The image is always cached locally regardless of which form is used.
        #[arg(
            short = 't',
            long = "tag",
            value_name = "IMAGE:TAG",
            action = clap::ArgAction::Append,
        )]
        tag: Vec<String>,

        /// Build context directory. Defaults to the current directory.
        #[arg(value_name = "PATH", value_hint = ValueHint::DirPath)]
        path: Option<PathBuf>,

        /// Explicit path to the Bundlefile. Overrides the context directory.
        #[arg(
            short = 'f',
            long = "file",
            value_name = "FILE",
            value_hint = ValueHint::FilePath,
        )]
        file: Option<PathBuf>,
    },

    /// Push a built image to a registry.
    ///
    /// One argument — push the most recently built image:
    ///
    ///   bundle push ghcr.io/org/plugin:latest
    ///
    /// Two arguments — push a locally-tagged image (from `bundle build -t NAME`):
    ///
    ///   bundle push myplugin:latest ghcr.io/org/plugin:1.0
    ///               local source    registry destination
    Push {
        /// Registry destination (single-argument form), or local source tag
        /// when a registry destination is given as the second argument
        /// (e.g. `myplugin:latest`).
        #[arg(value_name = "SOURCE_OR_DEST", value_hint = ValueHint::Other)]
        source_or_dest: String,

        /// Registry destination when a local source tag is given as the first
        /// argument, e.g. `ghcr.io/org/plugin:1.0`.
        #[arg(value_name = "DEST", value_hint = ValueHint::Other)]
        dest: Option<String>,
    },

    /// Inspect a bundle image — shows layers, labels, and managed config keys.
    ///
    /// Accepts three source forms:
    ///
    ///   ghcr.io/org/plugin:tag    remote OCI registry
    ///   oci:./path/to/dir         local OCI Image Layout directory
    ///   myplugin:latest           local tag  (built with `bundle build -t NAME`)
    Inspect {
        /// Image to inspect: a registry reference, an `oci:` local directory
        /// prefix, or a bare local tag name.
        #[arg(value_name = "IMAGE", value_hint = ValueHint::Other)]
        image: String,
    },

    /// Manage OCI bundles on a Minecraft server (pull, apply, run).
    #[command(subcommand)]
    Server(ServerCommands),

    /// Update the bundle binary to the latest release from GitHub.
    #[command(name = "selfupdate")]
    SelfUpdate {
        /// Skip the confirmation prompt before replacing the binary.
        #[arg(long)]
        not_interactive: bool,
    },
}

#[derive(Subcommand, Debug)]
enum ServerCommands {
    /// Scaffold a bundle.toml in the current directory.
    Init,

    /// Resolve all bundle sources, download blobs, and write bundle.lock.
    ///
    /// Registry references are resolved over the network.
    /// Local tags (e.g. myplugin:latest set with `bundle build -t NAME`)
    /// are resolved from the local cache — no network access required.
    ///
    /// Makes no changes to the server directory.
    /// Use `bundle server apply` to extract the locked layers.
    Pull,

    /// Pull bundles then extract them onto the server directory.
    /// Merges config files according to MANAGE annotations.
    Apply {
        /// Server root directory. Defaults to the current directory.
        #[arg(long, value_name = "PATH", value_hint = ValueHint::DirPath)]
        server_dir: Option<PathBuf>,

        /// Skip pull and apply from the local cache only.
        #[arg(long)]
        no_pull: bool,

        /// Skip files that would override paths listed in server.deny-override
        /// instead of failing hard. The dangerous files are never written; the
        /// operation just continues instead of aborting.
        #[arg(long)]
        ignore_dangerous_override_attempts: bool,
    },

    /// Show what `bundle server apply` would change without writing anything.
    Diff {
        /// Skip pull and diff against the local cache only.
        #[arg(long)]
        no_pull: bool,
    },

    /// Pull, apply, then start the server process declared in bundle.toml.
    /// On Unix the server replaces the current process via execvp(2).
    Run {
        /// Skip pull, apply from local cache, then start the server.
        #[arg(long)]
        no_pull: bool,

        /// Skip apply and start the server immediately.
        #[arg(long)]
        no_apply: bool,

        /// Server root directory. Defaults to the current directory.
        #[arg(long, value_name = "PATH", value_hint = ValueHint::DirPath)]
        server_dir: Option<PathBuf>,

        /// Skip files that would override paths listed in server.deny-override
        /// instead of failing hard. The dangerous files are never written; the
        /// operation just continues instead of aborting.
        #[arg(long)]
        ignore_dangerous_override_attempts: bool,
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
        Commands::Version => {
            cmd::version::run();
        }

        Commands::SelfUpdate { not_interactive } => {
            cmd::selfupdate::run(not_interactive).context("bundle selfupdate failed")?;
        }

        Commands::Login {
            registry,
            username,
            password,
            password_stdin,
            authfile,
        } => {
            cmd::login::run(cmd::login::LoginArgs {
                registry,
                username,
                password,
                password_stdin,
                authfile,
            })
            .await
            .context("bundle login failed")?;
        }

        Commands::Init => {
            cmd::init::run_bundlefile().context("bundle init failed")?;
        }

        Commands::Build {
            build_arg,
            tag,
            path,
            file,
        } => {
            cmd::build::run(cmd::build::BuildArgs {
                build_args: build_arg,
                tags: tag,
                context: path,
                file,
            })
            .await
            .context("bundle build failed")?;
        }

        Commands::Push {
            source_or_dest,
            dest,
        } => {
            // One arg  → dest only (load from built/ slot).
            // Two args → source_or_dest is a local tag, dest is the registry ref.
            let (local_tag, image_ref) = match dest {
                Some(d) => (Some(source_or_dest), d),
                None => (None, source_or_dest),
            };
            cmd::push::run(cmd::push::PushArgs {
                image_ref,
                local_tag,
            })
            .await
            .context("bundle push failed")?;
        }

        Commands::Inspect { image } => {
            cmd::inspect::run(image)
                .await
                .context("bundle inspect failed")?;
        }

        Commands::Server(server_cmd) => match server_cmd {
            ServerCommands::Init => {
                cmd::init::run_server_config().context("bundle server init failed")?;
            }

            ServerCommands::Pull => {
                cmd::pull::run()
                    .await
                    .context("bundle server pull failed")?;
            }

            ServerCommands::Apply {
                server_dir,
                no_pull,
                ignore_dangerous_override_attempts,
            } => {
                cmd::apply::run(cmd::apply::ApplyArgs {
                    server_dir,
                    dry_run: false,
                    no_pull,
                    ignore_dangerous_override_attempts,
                })
                .await
                .context("bundle server apply failed")?;
            }

            ServerCommands::Diff { no_pull } => {
                cmd::diff::run(no_pull)
                    .await
                    .context("bundle server diff failed")?;
            }

            ServerCommands::Run {
                no_pull,
                no_apply,
                server_dir,
                ignore_dangerous_override_attempts,
            } => {
                cmd::run::run(cmd::run::RunArgs {
                    no_pull,
                    no_apply,
                    server_dir,
                    ignore_dangerous_override_attempts,
                })
                .await
                .context("bundle server run failed")?;
            }
        },
    }

    Ok(())
}

/// Parse a `--build-arg` value. Accepts `KEY=VALUE` or bare `KEY` (looks up `$KEY` from env).
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
    fn cli_selfupdate_parses() {
        let cli = Cli::try_parse_from(["bundle", "selfupdate"]);
        assert!(cli.is_ok(), "bundle selfupdate should parse: {:?}", cli);
        assert!(matches!(
            cli.unwrap().command,
            Commands::SelfUpdate {
                not_interactive: false
            }
        ));
    }

    #[test]
    fn cli_selfupdate_not_interactive_parses() {
        let cli = Cli::try_parse_from(["bundle", "selfupdate", "--not-interactive"]);
        assert!(
            cli.is_ok(),
            "bundle selfupdate --not-interactive should parse: {:?}",
            cli
        );
        assert!(matches!(
            cli.unwrap().command,
            Commands::SelfUpdate {
                not_interactive: true
            }
        ));
    }

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
    fn cli_build_with_file_flag_parses() {
        let cli = Cli::try_parse_from(["bundle", "build", "--file", "path/to/Bundlefile"]);
        assert!(cli.is_ok());
        if let Ok(Cli {
            command: Commands::Build { file, .. },
        }) = cli
        {
            assert_eq!(file, Some(PathBuf::from("path/to/Bundlefile")));
        }
    }

    #[test]
    fn cli_build_with_short_file_flag_parses() {
        let cli = Cli::try_parse_from(["bundle", "build", "-f", "path/to/Bundlefile"]);
        assert!(cli.is_ok());
        if let Ok(Cli {
            command: Commands::Build { file, .. },
        }) = cli
        {
            assert_eq!(file, Some(PathBuf::from("path/to/Bundlefile")));
        }
    }

    #[test]
    fn cli_build_bare_local_tag_parses() {
        // Bare names (no registry hostname) are local-only tags — they must be
        // accepted by the CLI and NOT rejected at parse time.
        let cli = Cli::try_parse_from(["bundle", "build", "-t", "myplugin:latest"]);
        assert!(
            cli.is_ok(),
            "bare local tag should be accepted by the CLI: {:?}",
            cli
        );
        if let Ok(Cli {
            command: Commands::Build { tag, .. },
        }) = cli
        {
            assert_eq!(tag, vec!["myplugin:latest".to_string()]);
        }
    }

    #[test]
    fn cli_build_mixed_local_and_registry_tags_parse() {
        // A local tag and a registry tag may be combined in a single build invocation.
        let cli = Cli::try_parse_from([
            "bundle",
            "build",
            "-t",
            "myplugin:latest",
            "-t",
            "ghcr.io/me/myplugin:latest",
        ]);
        assert!(
            cli.is_ok(),
            "mixed local + registry tags should parse: {:?}",
            cli
        );
        if let Ok(Cli {
            command: Commands::Build { tag, .. },
        }) = cli
        {
            assert_eq!(tag.len(), 2);
            assert!(tag.contains(&"myplugin:latest".to_string()));
            assert!(tag.contains(&"ghcr.io/me/myplugin:latest".to_string()));
        }
    }

    #[test]
    fn cli_push_parses() {
        let cli = Cli::try_parse_from(["bundle", "push", "ghcr.io/me/bundle:v1"]);
        assert!(cli.is_ok(), "bundle push should parse: {:?}", cli);
        if let Ok(Cli {
            command:
                Commands::Push {
                    source_or_dest,
                    dest,
                },
        }) = cli
        {
            assert_eq!(source_or_dest, "ghcr.io/me/bundle:v1");
            assert!(dest.is_none(), "single-arg push should have no dest");
        }
    }

    #[test]
    fn cli_push_with_local_source_parses() {
        let cli = Cli::try_parse_from([
            "bundle",
            "push",
            "myplugin:latest",
            "ghcr.io/me/myplugin:1.0",
        ]);
        assert!(cli.is_ok(), "two-arg push should parse: {:?}", cli);
        if let Ok(Cli {
            command:
                Commands::Push {
                    source_or_dest,
                    dest,
                },
        }) = cli
        {
            assert_eq!(source_or_dest, "myplugin:latest");
            assert_eq!(dest, Some("ghcr.io/me/myplugin:1.0".to_string()));
        }
    }

    // ── inspect ───────────────────────────────────────────────────────────────

    #[test]
    fn cli_inspect_registry_ref_parses() {
        let cli = Cli::try_parse_from(["bundle", "inspect", "ghcr.io/org/plugin:latest"]);
        assert!(cli.is_ok(), "registry ref should parse: {:?}", cli);
        if let Ok(Cli {
            command: Commands::Inspect { image },
        }) = cli
        {
            assert_eq!(image, "ghcr.io/org/plugin:latest");
        }
    }

    #[test]
    fn cli_inspect_local_tag_parses() {
        let cli = Cli::try_parse_from(["bundle", "inspect", "myplugin:latest"]);
        assert!(cli.is_ok(), "bare local tag should parse: {:?}", cli);
        if let Ok(Cli {
            command: Commands::Inspect { image },
        }) = cli
        {
            assert_eq!(image, "myplugin:latest");
        }
    }

    #[test]
    fn cli_inspect_oci_dir_parses() {
        let cli = Cli::try_parse_from(["bundle", "inspect", "oci:./output"]);
        assert!(cli.is_ok(), "oci: dir ref should parse: {:?}", cli);
        if let Ok(Cli {
            command: Commands::Inspect { image },
        }) = cli
        {
            assert_eq!(image, "oci:./output");
        }
    }

    #[test]
    fn cli_inspect_requires_image_arg() {
        let cli = Cli::try_parse_from(["bundle", "inspect"]);
        assert!(cli.is_err(), "inspect without an argument must fail");
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
                    ignore_dangerous_override_attempts: _,
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
