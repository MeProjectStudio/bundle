//! Shared helpers for integration tests.

use std::fs;
use std::path::Path;

/// Write `content` to `base/relative_path`, creating all parent directories.
#[allow(dead_code)]
pub fn write_file(base: &Path, relative_path: &str, content: &[u8]) {
    let full = base.join(relative_path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).expect("create parent dirs");
    }
    fs::write(&full, content).expect("write file");
}

/// Write a `Bundlefile` at `base/Bundlefile`.
pub fn bundlefile(base: &Path, content: &str) {
    fs::write(base.join("Bundlefile"), content.as_bytes()).expect("write Bundlefile");
}
