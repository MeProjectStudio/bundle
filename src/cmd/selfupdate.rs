use anyhow::{Context, Result};
use self_update::cargo_crate_version;

/// Run `bundle selfupdate`.
///
/// Queries the GitHub release API for `MeProjectStudio/bundle`, and if a
/// newer version is available, downloads it and atomically replaces the
/// running binary.
///
/// Pass `no_confirm = true` (via `--not-interactive`) to skip the
/// confirmation prompt, e.g. when running from a script.
pub fn run(no_confirm: bool) -> Result<()> {
    let status = self_update::backends::github::Update::configure()
        .repo_owner("MeProjectStudio")
        .repo_name("bundle")
        .bin_name("bundle")
        .show_download_progress(true)
        .no_confirm(no_confirm)
        .current_version(cargo_crate_version!())
        .build()
        .context("building self-update configuration")?
        .update()
        .context("performing self-update")?;

    match status {
        self_update::Status::UpToDate(v) => {
            println!("Already up to date ({v}) — no update needed.");
        }
        self_update::Status::Updated(v) => {
            println!("Updated successfully to {v}.");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    // Self-update requires live network access and a published GitHub release,
    // so there is nothing meaningful to unit-test here.  CLI-parse coverage
    // lives in main.rs's mod tests alongside all other subcommands.
}
