macro_rules! log {
    ($($t:tt)*) => { crate::progress!("run", $($t)*) };
}

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

use crate::cmd::apply::{self, ApplyArgs};
use crate::project::config::ProjectConfig;

/// Arguments accepted by `bundle run`.
#[derive(Debug, Clone, Default)]
pub struct RunArgs {
    /// Skip the pull step inside `bundle apply` (use whatever is already
    /// cached).  Equivalent to `bundle apply --no-pull` + exec.
    pub no_pull: bool,

    /// Skip the apply step entirely and exec the server immediately.
    /// Useful when plugin files are already up-to-date and you just want
    /// to (re)start the server process.
    pub no_apply: bool,

    /// If set, treat this directory as the server root instead of `$PWD`.
    pub server_dir: Option<PathBuf>,
}

/// Run `bundle run` — like `docker compose up`.
///
/// Delegates the pull+apply work to [`apply::run`] (which already owns the
/// pull step), then replaces the process with the server command.  This keeps
/// the logic in one place and avoids the double-pull that would occur if `run`
/// had its own separate pull step.
pub async fn run(args: RunArgs) -> Result<()> {
    let server_dir = match &args.server_dir {
        Some(d) => d.clone(),
        None => std::env::current_dir().context("getting current directory")?,
    };

    log!("server directory: {}", server_dir.display());

    // `bundle apply` owns the pull → FS-extract pipeline.  We pass our
    // --no-pull flag straight through so the user has a single knob.
    if args.no_apply {
        log!("skipping apply (--no-apply)");
    } else {
        log!("step 1/2: apply");
        apply::run(ApplyArgs {
            server_dir: Some(server_dir.clone()),
            dry_run: false,
            no_pull: args.no_pull,
        })
        .await
        .context("apply step failed (use --no-apply to skip)")?;
    }

    log!("step 2/2: exec server");

    // Load the project config for the `server.run` command.
    // We load it again here rather than passing it through so that any changes
    // made by the apply step (unlikely but possible) are picked up.
    let project = ProjectConfig::load_from(&server_dir).with_context(|| {
        format!(
            "reading bundle.toml in {} to determine server.run command",
            server_dir.display()
        )
    })?;

    if project.server.run.is_empty() {
        bail!(
            "server.run in bundle.toml is empty — add a run command, e.g.:\n\
             [server]\n\
             run = [\"java\", \"-Xmx4G\", \"-jar\", \"server.jar\", \"nogui\"]"
        );
    }

    let program = &project.server.run[0];
    let argv: &[String] = &project.server.run[1..];
    const ART: &[&str] = &[
        "                    ",
        "      ##*#%%%%%%    ",
        "      ##*+#%%%##%%  ",
        "      *+=#####%%    ",
        "     ###*=-=+**     ",
        "  ##*++++*=*##%%    ",
        "  ##*++++***###%%%  ",
        "  ##*++++++***#%%%  ",
        "  %%##*******###%%  ",
        "    %%#########%    ",
        "                    ",
    ];

    // Build info lines dynamically, filling in bundles up to the art height.
    let mut info: Vec<String> = vec![
        String::new(),
        "  Starting Minecraft server".into(),
        format!("  command : {}", project.server.run.join(" ")),
        format!("  cwd     : {}", server_dir.display()),
        String::new(),
    ];

    if !project.bundles.is_empty() {
        info.push("  bundles :".into());
        let capacity = ART.len().saturating_sub(info.len());
        let total = project.bundles.len();
        if total <= capacity {
            for b in &project.bundles {
                info.push(format!("    {}", b));
            }
        } else {
            let show = capacity.saturating_sub(1);
            for b in project.bundles.iter().take(show) {
                info.push(format!("    {}", b));
            }
            info.push(format!("  and {} more…", total - show));
        }
    }

    println!();
    for (i, art_line) in ART.iter().enumerate() {
        match info.get(i) {
            Some(line) if !line.is_empty() => println!("{}  {}", art_line, line),
            _ => println!("{}", art_line),
        }
    }
    println!();

    exec_server(program, argv, &server_dir)
}

/// Replace the current process with the server command on Unix, or spawn-and-
/// wait on other platforms.
#[cfg(unix)]
fn exec_server(program: &str, argv: &[String], server_dir: &std::path::Path) -> Result<()> {
    use std::ffi::CString;

    // Change into the server directory before exec so relative paths (e.g.
    // `server.jar`) are resolved correctly.
    std::env::set_current_dir(server_dir)
        .with_context(|| format!("changing into server directory: {}", server_dir.display()))?;

    // Build null-terminated C strings for execvp.
    let c_program =
        CString::new(program).with_context(|| format!("invalid program name: {:?}", program))?;

    let mut c_args: Vec<CString> = Vec::with_capacity(argv.len() + 1);
    c_args.push(c_program.clone());
    for arg in argv {
        c_args.push(
            CString::new(arg.as_str()).with_context(|| format!("invalid argument: {:?}", arg))?,
        );
    }

    // execvp replaces the current process image.  On success it never returns.
    let c_argv: Vec<*const libc::c_char> = c_args
        .iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();

    // SAFETY: c_program and c_argv are valid null-terminated C strings.
    let rc = unsafe { libc::execvp(c_program.as_ptr(), c_argv.as_ptr()) };

    // execvp only returns on error.
    Err(std::io::Error::last_os_error()).with_context(|| {
        format!(
            "execvp failed for '{}' (rc={}): make sure the program is in $PATH",
            program, rc
        )
    })
}

#[cfg(not(unix))]
fn exec_server(program: &str, argv: &[String], server_dir: &std::path::Path) -> Result<()> {
    use std::process::Command;

    eprintln!(
        "[run] note: process replacement (exec) is not available on this platform; \
         using child-process spawn instead."
    );

    let status = Command::new(program)
        .args(argv)
        .current_dir(server_dir)
        .status()
        .with_context(|| format!("spawning server process: {}", program))?;

    let code = status.code().unwrap_or(1);
    if code != 0 {
        bail!("server exited with non-zero status: {}", code);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke-test: exec_server with `echo` should not panic (non-Unix uses
    /// spawn-and-wait, Unix replaces the process — we can't easily test that in
    /// a unit test without forking).
    #[cfg(not(unix))]
    #[test]
    fn exec_server_echo_succeeds() {
        let dir = tempfile::TempDir::new().unwrap();
        // "cmd /C echo hello" on Windows.
        exec_server(
            "cmd",
            &["/C".to_string(), "echo hello".to_string()],
            dir.path(),
        )
        .unwrap();
    }

    #[test]
    fn run_args_default() {
        let args = RunArgs::default();
        assert!(!args.no_pull, "no_pull should default to false");
        assert!(!args.no_apply, "no_apply should default to false");
        assert!(args.server_dir.is_none());
    }

    #[test]
    fn run_no_pull_is_passed_to_apply() {
        // Verify that RunArgs carries the no_pull flag correctly — the
        // actual threading into apply::run is tested via integration.
        let args = RunArgs {
            no_pull: true,
            no_apply: false,
            server_dir: None,
        };
        assert!(args.no_pull);
        assert!(!args.no_apply);
    }
}
