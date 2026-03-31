use anyhow::Result;

use crate::cmd::apply::{run as apply_run, ApplyArgs};

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
