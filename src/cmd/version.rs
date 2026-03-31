use git_version::git_version;

/// The git revision embedded at compile time via `git describe`.
///
/// Examples:
///   - `"abc1234"`            — untagged commit (short hash)
///   - `"v0.1.0"`             — exactly on a tag
///   - `"v0.1.0-3-gabc1234"` — 3 commits after the tag
///   - `"abc1234-modified"`   — uncommitted changes in the working tree
///
/// Falls back to `"unknown"` when git is not available at build time.
const GIT_REVISION: &str = git_version!(
    args = ["--always", "--dirty=-modified"],
    fallback = "unknown"
);

/// Run `bundle version`.
pub fn run() {
    let pkg_version = env!("CARGO_PKG_VERSION");
    println!("bundle {pkg_version} (revision: {GIT_REVISION})");
}
