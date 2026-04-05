fn main() {
    // TARGET is only available to build scripts, not to the crate itself.
    // Re-export it under a custom key so the rest of the code can reach it
    // via env!("BUNDLE_TARGET").
    let target = std::env::var("TARGET").expect("Cargo always sets TARGET in build scripts");
    println!("cargo:rustc-env=BUNDLE_TARGET={target}");

    // No need to re-run on source changes — only re-run if TARGET itself
    // changes (i.e. cross-compilation target switches), which triggers a
    // full rebuild anyway.
    println!("cargo:rerun-if-env-changed=TARGET");
}
