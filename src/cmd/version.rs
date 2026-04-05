use git_version::git_version;

const GIT_REVISION: &str = git_version!(
    args = ["--always", "--dirty=-modified"],
    fallback = "unknown"
);

const TARGET: &str = env!("BUNDLE_TARGET");

/// Run `bundle version`.
pub fn run() {
    let pkg_version = env!("CARGO_PKG_VERSION");
    println!("bundle {pkg_version} ({TARGET}, revision: {GIT_REVISION})");
}
