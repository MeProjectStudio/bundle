//! `bundle diff` — show what `bundle apply` would change without writing anything.
//!
//! This is a thin wrapper around [`crate::cmd::apply`] with `dry_run = true`.
//! It reads `bundle.toml` and `bundle.lock`, pulls current bundle state from
//! the registry (same as apply), and reports the set of files that would be
//! created, overwritten, merged, or deleted — without touching the server
//! directory.
//!
//! ## Output format
//!
//! ```text
//! Diff (what `bundle apply` would do, based on current registry state):
//!
//!   + plugins/EssentialsX-2.20.1.jar    (would create)
//!   ~ plugins/LuckPerms-5.4.jar         (would overwrite)
//!   M plugins/Essentials/config.yml     (would merge)
//!   - plugins/OldPlugin.jar             (would delete)
//!
//!   1 created, 1 overwritten, 1 merged, 1 deleted
//! ```
//!
//! Exit code is `0` whether or not there are changes (unlike `git diff`).
//! Pass `--no-pull` to skip the automatic pull and diff against the local
//! cache only.

use anyhow::Result;

use crate::cmd::apply::{run as apply_run, ApplyArgs};

// ── Entry point ───────────────────────────────────────────────────────────────

/// Run `bundle diff`.
///
/// Delegates entirely to [`crate::cmd::apply::run`] with `dry_run = true`.
///
/// `no_pull` — when `true`, skip the automatic `bundle pull` step and diff
/// against whatever is already present in the local cache.
pub async fn run(no_pull: bool) -> Result<()> {
    apply_run(ApplyArgs {
        dry_run: true,
        no_pull,
        server_dir: None,
    })
    .await
}
