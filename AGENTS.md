# AGENTS.md

## Project Overview

`bundle` is a Rust CLI tool for declarative mod management of Minecraft servers using OCI images. It lets you define plugins/mods in a `Bundlefile` (Docker-inspired syntax), build them into OCI-compliant images, push to any OCI registry (GHCR, Docker Hub, self-hosted), and deploy them to a server directory.

Binary name: **`bundle`**
Crate name: **`bundle`** (`src/main.rs`)

---

## Repository Layout

```
src/
  main.rs           – CLI definition (clap), top-level dispatch, and unit tests for arg parsing
  cmd/              – One file per subcommand; each exposes a single async entry-point function
    apply.rs        – `bundle server apply`
    build.rs        – `bundle build`
    diff.rs         – `bundle server diff`
    init.rs         – `bundle [server] init`
    login.rs        – `bundle login`
    pull.rs         – `bundle server pull`
    push.rs         – `bundle push`
    run.rs          – `bundle server run`
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
| `COPY [--from=<index\|name>] <src> <dest>` | Copy from the build context or a previous stage. |
| `LABEL <key>=<value> …` | Embed metadata in the OCI image config. |
| `MANAGE <config-path>: <key>, …` | Declare config keys this bundle owns (for merge). |

Line continuations (`\`) and `#` comments are supported.

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

---

## Adding a New Bundlefile Directive

1. Add the AST type (if needed) to `src/bundlefile/types.rs`.
2. Add a `handle_<directive>` function in `src/bundlefile/parser.rs`; call it from `parse()`.
3. Add unit tests in `parser.rs`'s `mod tests` covering happy path and error cases.
4. Consume the new directive in `src/bundle/build.rs` and/or `src/apply/`.

## Adding a New CLI Subcommand

1. Create `src/cmd/<name>.rs` with a public `async fn run(…) -> anyhow::Result<()>`.
2. Export it in `src/cmd/mod.rs`.
3. Add the variant to the relevant `enum` in `src/main.rs` (using clap `#[derive(Subcommand)]`).
4. Dispatch it in `run()` in `src/main.rs`.
5. Add CLI-parse tests in `main.rs`'s `mod tests`.

---

## CI Pipeline (`.github/workflows/cicd.yml`)

Two jobs run on every push/PR:

1. **Static analysis** – `cargo clippy`, `cargo deny check`, `cargo fmt --check`. Clippy results are uploaded as a SARIF report to GitHub Code Scanning.
2. **Release** – cross-compiles with `actions-rust-cross` for all five targets and publishes binaries via `actions-rust-release`.