# AGENTS.md

## Project Overview

`bundle` is a Rust CLI tool for declarative mod management of Minecraft servers using OCI images. It lets you define plugins/mods in a `Bundlefile` (Docker-inspired syntax), build them into OCI-compliant images, push to any OCI registry (GHCR, Docker Hub, self-hosted), and deploy them to a server directory.

Binary name: **`bundle`**
Crate name: **`bundle`** (`src/main.rs`)

---

## Repository Layout

```
build.rs            – Build script; forwards Cargo's TARGET into BUNDLE_TARGET for use via env!()
src/
  main.rs           – CLI definition (clap), top-level dispatch, and unit tests for arg parsing
  cmd/              – One file per subcommand; each exposes a single public entry-point function
    apply.rs        – `bundle server apply`
    build.rs        – `bundle build`
    diff.rs         – `bundle server diff`
    init.rs         – `bundle [server] init`
    login.rs        – `bundle login`
    pull.rs         – `bundle server pull`
    push.rs         – `bundle push`
    run.rs          – `bundle server run`
    selfupdate.rs   – `bundle selfupdate`
    version.rs      – `bundle version`
  bundlefile/       – Parsing and types for the Bundlefile DSL
    parser.rs       – Tokenises and parses a Bundlefile into a `Bundlefile` struct
    types.rs        – All AST types: Stage, AddDirective, CopyDirective, ManageDirective, …
  bundle/           – OCI image building
    build.rs        – Walks stages, resolves ADD/COPY sources, produces OCI layers
    layer.rs        – Low-level tar + gzip layer construction
    annotations.rs  – OCI annotation helpers
  apply/            – Applying a bundle to a live server directory
    merge.rs        – Config key-merge logic (respects MANAGE ownership)
    overlay.rs      – File overlay (copies layer contents onto server dir)
  project/          – Server-side project files
    config.rs       – `bundle.toml` read/write
    lock.rs         – `bundle.lock` read/write
  registry/         – OCI registry client wrappers
    client.rs       – Push / pull / auth against an OCI registry
    semver.rs       – Tag sorting and semver resolution
    types.rs        – Shared registry types
  util/
    digest.rs       – SHA-256 helpers
    fetch.rs        – Async HTTP download with optional checksum verification

tests/              – Integration fixture data (not Rust integration tests)
  bundle-file-build/Bundlefile   – Minimal single-stage Bundlefile fixture
  server-with-bundle/            – Fixture of a real server directory with bundle.lock / bundle.toml
```

---

## Key Concepts

### Bundlefile DSL

Mirrors Dockerfile syntax. Supported directives:

| Directive | Description |
|-----------|-------------|
| `FROM <image> [AS <name>]` | Begin a new stage. `scratch` is the empty base. |
| `ARG <name>[=<default>]` | Declare a build argument; substituted with `${VAR}`. |
| `ADD [--checksum=sha256:<hex>] <src> <dest>` | Copy a local file/dir or download a URL into the layer. |
| `COPY [--from=<index\|name>] <src> <dest>` | Copy from the build context or a previous stage. `src` may contain glob metacharacters (`*`, `?`, `[…]`, `**`). |
| `LABEL <key>=<value> …` | Embed metadata in the OCI image config. |
| `MANAGE <config-path>: <key>, …` | Declare config keys this bundle owns (for merge). |

Line continuations (`\`) and `#` comments are supported.

#### Glob behaviour in `COPY`

When `src` contains glob metacharacters the pattern is expanded at **build time**, not parse time:

- `*` matches any characters within a single path segment; it does not cross `/`.
- `**` matches zero or more path segments recursively.
- The non-wildcard directory prefix of the pattern (e.g. `plugins/` in `plugins/**/*.jar`) is stripped from each match; the remainder is appended to `dest`.
- Zero matches is a hard error.

```text
COPY plugins/*.jar          output/          # flat match — all jars directly in plugins/
COPY plugins/**/*.jar       output/          # recursive — preserves subdirectory structure
COPY --from=0 mods/*.jar    mods/            # glob against a prior stage's file tree
```

### OCI Image Format

Each `bundle build` produces a standard OCI image:
- One gzipped tar layer per stage
- Image manifest + config JSON written to the local store, or pushed directly to a registry

### Server Workflow

```
bundle server init          # writes bundle.toml
bundle server pull          # resolves tags → digests, writes bundle.lock
bundle server apply         # overlays files from locked images onto server dir
bundle server diff          # shows what apply would change
bundle server run           # pull + apply + exec server jar
```

---

## Development Commands

```sh
# Build (debug)
cargo build

# Build (release, optimised for size)
cargo build --release

# Run all tests (unit + doc)
cargo test

# Lint (mirrors CI – must produce zero warnings)
cargo clippy --no-deps --all-targets -- -D warnings

# Format check (mirrors CI)
cargo fmt --all --check

# Auto-format
cargo fmt --all

# Check dependency licences / advisories (requires cargo-deny)
cargo deny check
```

### Cross-compilation targets used in CI

| Platform | Target triple |
|----------|--------------|
| Linux x86\_64 | `x86_64-unknown-linux-musl` |
| Linux aarch64 | `aarch64-unknown-linux-musl` |
| Windows x86\_64 | `x86_64-pc-windows-msvc` |
| macOS x86\_64 | `x86_64-apple-darwin` |
| macOS arm64 | `aarch64-apple-darwin` |

---

## Code Style and Conventions

- **Rust edition 2021**.
- All `async` code uses **Tokio** with the full runtime.
- Errors bubble up via **`anyhow::Result`** at command boundaries; domain types use **`thiserror`**.
- Keep clippy clean with `#[allow(...)]` only when unavoidable; document why.
- Unit tests live in `mod tests` at the bottom of the file they test. Use descriptive snake\_case test names that read like sentences (e.g. `add_checksum_on_local_path_is_error`).
- Avoid `unwrap()` / `expect()` in non-test code; propagate errors.
- New commands go in `src/cmd/<name>.rs`, exported from `src/cmd/mod.rs`, and wired up in `src/main.rs`.
- Commands that only need blocking I/O (e.g. `version`, `init`, `selfupdate`) expose a plain synchronous `pub fn run(…) -> Result<()>`. Commands that perform async I/O expose `pub async fn run(…) -> Result<()>`. Both are called directly from `src/main.rs`'s top-level `async fn run()`.

### Compile-time target triple

`build.rs` reads Cargo's `TARGET` environment variable (only available in build scripts) and re-exports it as `BUNDLE_TARGET`, making the current compilation target accessible anywhere in the crate via `env!("BUNDLE_TARGET")`. This is used in `cmd/version.rs` and is available to any future code that needs to know the target triple at compile time.

---

## Adding a New Bundlefile Directive

1. Add the AST type (if needed) to `src/bundlefile/types.rs`.
2. Add a `handle_<directive>` function in `src/bundlefile/parser.rs`; call it from `parse()`.
3. Add unit tests in `parser.rs`'s `mod tests` covering happy path and error cases.
4. Consume the new directive in `src/bundle/build.rs` and/or `src/apply/`.

## Adding a New CLI Subcommand

1. Create `src/cmd/<name>.rs` with a public entry point:
   - `pub fn run(…) -> anyhow::Result<()>` for commands that only need blocking I/O.
   - `pub async fn run(…) -> anyhow::Result<()>` for commands that perform async I/O.
2. Export it in `src/cmd/mod.rs`.
3. Add the variant to the relevant `enum` in `src/main.rs` (using clap `#[derive(Subcommand)]`). Use `#[command(name = "…")]` whenever the desired subcommand name differs from what clap would derive from the variant name (e.g. `SelfUpdate` → `selfupdate`).
4. Dispatch it in `run()` in `src/main.rs`, calling `.await` only for async entry points.
5. Add CLI-parse tests in `main.rs`'s `mod tests`.

---

## CI Pipeline

Two workflow files live in `.github/workflows/`:

1. **`check.yml`** – runs on every push and PR: `cargo clippy`, `cargo deny check`, `cargo fmt --check`. Clippy results are uploaded as a SARIF report to GitHub Code Scanning.
2. **`release.yml`** – triggers on `v*` tags: cross-compiles with `actions-rust-cross` for all five targets, sets the crate version from the tag with `cargo set-version`, generates a changelog with `git-cliff`, and publishes binaries via `actions-rust-release`. Archives are named `bundle-<target>.tar.gz` (`.zip` on Windows) and contain the `bundle` binary at the archive root.