use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use super::types::{
    AddDirective, AddSource, Bundlefile, CopyDirective, CopyFrom, ManageDirective, Stage,
};

/// Parse the contents of a `Bundlefile`, substituting `${ARG}` references.
///
/// `cli_overrides` contains values supplied via `--build-arg KEY=VAL` on the
/// command line; they take precedence over `ARG KEY=DEFAULT` defaults declared
/// in the file.
///
/// # Bundlefile format
///
/// ```text
/// ARG VERSION=2.20.1
///
/// FROM ghcr.io/someauthor/base:${VERSION} AS deps
///
/// ADD --checksum=sha256:abc123 https://example.com/Foo-${VERSION}.jar mods/Foo.jar
/// ADD ./local/config/                                                  config/
///
/// COPY --from=deps  mods/Foo.jar  mods/Foo.jar
///
/// MANAGE plugins/Essentials/config.yml: home.bed-respawn, homes.max-homes
/// ```
///
/// Lines starting with `#` are comments and are ignored.
/// A trailing `\` continues the logical line onto the next.
pub fn parse(content: &str, cli_overrides: &HashMap<String, String>) -> Result<Bundlefile> {
    let logical_lines = join_continuations(content);

    // ── Arg scoping (mirrors Docker) ──────────────────────────────────────────
    //
    // ARG directives that appear *before* the first FROM are "global".  They:
    //   • can be referenced in FROM image references (e.g. `FROM base:${TAG}`)
    //   • seed every subsequent stage's arg scope automatically
    //
    // ARG directives that appear *inside* a stage are local to that stage.
    // They can shadow global args, and changes do not bleed into other stages.
    //
    // This matches Docker's scoping model, with one deliberate simplification:
    // pre-FROM args are available in every stage without needing to be
    // re-declared (Docker requires re-declaration; we skip that friction).

    // Args declared before the first FROM.
    let mut global_args: HashMap<String, String> = HashMap::new();

    // Per-stage arg scopes.  `stage_args[i]` is the scope for `stages[i]`.
    // Initialised from `global_args` when the stage's FROM is processed.
    let mut stage_args: Vec<HashMap<String, String>> = Vec::new();

    let mut stages: Vec<Stage> = Vec::new();

    for (lineno, raw) in logical_lines.iter().enumerate() {
        let line = raw.trim();

        // Skip blank lines and comments.
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let (directive, rest) = split_directive(line);

        match directive.to_uppercase().as_str() {
            "ARG" => {
                // Before the first FROM → update global_args.
                // Inside a stage       → update that stage's local scope only.
                if stages.is_empty() {
                    handle_arg(rest, cli_overrides, &mut global_args)
                        .with_context(|| format!("line {}: ARG parse error", lineno + 1))?;
                } else {
                    let scope = stage_args.last_mut().unwrap();
                    handle_arg(rest, cli_overrides, scope)
                        .with_context(|| format!("line {}: ARG parse error", lineno + 1))?;
                }
            }

            "FROM" => {
                // Each stage starts with a fresh copy of the global arg scope so
                // that pre-FROM args are available without re-declaration, and
                // stage-level ARGs don't spill into sibling stages.
                let scope = global_args.clone();
                let image_line = substitute(rest.trim(), &scope);
                if image_line.is_empty() {
                    bail!("line {}: FROM requires an image reference", lineno + 1);
                }
                let parts: Vec<&str> = image_line.split_whitespace().collect();

                // Validate the image reference — catch obvious scratch typos
                // (e.g. "scrath", "Scratch", "scrach") before they turn into
                // confusing network errors at build time.
                let image_ref = parts[0];
                let lower = image_ref.to_lowercase();
                if lower != "scratch" {
                    let looks_like_scratch_typo = lower.len() <= 8
                        && !lower.contains('.')
                        && !lower.contains('/')
                        && !lower.contains(':')
                        && lower.chars().filter(|c| "scratch".contains(*c)).count()
                            >= lower.len().saturating_sub(1);
                    if looks_like_scratch_typo {
                        bail!(
                            "line {}: '{}' is not a valid image reference.\n\
                             Did you mean `FROM scratch`?\n\
                             `FROM scratch` is the reserved keyword for a stage \
                             with no base image (no network access required).",
                            lineno + 1,
                            image_ref
                        );
                    }
                }

                let stage = match parts.len() {
                    1 => Stage::new(image_ref, None),
                    3 if parts[1].eq_ignore_ascii_case("as") => {
                        Stage::new(image_ref, Some(parts[2].to_string()))
                    }
                    _ => bail!(
                        "line {}: invalid FROM syntax; expected 'FROM <image>' or \
                         'FROM <image> AS <name>', got '{}'",
                        lineno + 1,
                        image_line
                    ),
                };
                stages.push(stage);
                stage_args.push(scope);
            }

            "ADD" => {
                let scope = stage_args
                    .last()
                    .ok_or_else(|| anyhow::anyhow!("line {}: ADD before any FROM", lineno + 1))?;
                let stage = current_stage_mut(&mut stages, lineno + 1)?;
                let add = handle_add(rest, scope)
                    .with_context(|| format!("line {}: ADD parse error", lineno + 1))?;
                stage.adds.push(add);
            }

            "COPY" => {
                let scope = stage_args
                    .last()
                    .ok_or_else(|| anyhow::anyhow!("line {}: COPY before any FROM", lineno + 1))?;
                let stage = current_stage_mut(&mut stages, lineno + 1)?;
                let copy = handle_copy(rest, scope)
                    .with_context(|| format!("line {}: COPY parse error", lineno + 1))?;
                stage.copies.push(copy);
            }

            "MANAGE" => {
                let scope = stage_args.last().ok_or_else(|| {
                    anyhow::anyhow!("line {}: MANAGE before any FROM", lineno + 1)
                })?;
                let stage = current_stage_mut(&mut stages, lineno + 1)?;
                let manage = handle_manage(rest, scope)
                    .with_context(|| format!("line {}: MANAGE parse error", lineno + 1))?;
                // Same config path in the same stage: last writer wins.
                if let Some(existing) = stage
                    .manages
                    .iter_mut()
                    .find(|m| m.config_path == manage.config_path)
                {
                    existing.keys = manage.keys;
                } else {
                    stage.manages.push(manage);
                }
            }

            other => {
                bail!(
                    "line {}: unknown directive '{}'; \
                     expected one of ARG, FROM, ADD, COPY, MANAGE",
                    lineno + 1,
                    other
                );
            }
        }
    }

    if stages.is_empty() {
        bail!("Bundlefile contains no FROM directive");
    }

    Ok(Bundlefile {
        build_args: global_args,
        stages,
    })
}

// ─── Directive handlers ──────────────────────────────────────────────────────

/// `ARG KEY` or `ARG KEY=DEFAULT`
///
/// Resolution order (highest priority first):
///
/// 1. **CLI `--build-arg KEY=VALUE`** — explicit value always wins.
/// 2. **CLI `--build-arg KEY`** (no `=`) — the caller already resolved this to
///    an environment value (or empty string) before inserting it into
///    `cli_overrides`; treated the same as case 1.
/// 3. **Bundlefile default** (`ARG KEY=default`) — expanded with `${VAR}` so
///    that chained ARGs work:
///    ```text
///    ARG BASE=1.0
///    ARG FULL=${BASE}-jre   →  FULL = "1.0-jre"
///    ```
/// 4. **Host environment** — when an ARG has *no* default in the Bundlefile and
///    no CLI override, the host environment variable `$KEY` is consulted.
///    This mirrors Docker's behaviour: `ARG NAME` picks up `$NAME` from the
///    environment that invokes `bundle build`.
/// 5. **Empty string** — last resort when none of the above apply.
fn handle_arg(
    rest: &str,
    cli_overrides: &HashMap<String, String>,
    build_args: &mut HashMap<String, String>,
) -> Result<()> {
    let rest = rest.trim();
    if rest.is_empty() {
        bail!("ARG requires a name");
    }

    // Split into name and optional default.
    let (name, has_default, raw_default) = if let Some((k, v)) = rest.split_once('=') {
        (k.trim(), true, v.trim().to_string())
    } else {
        (rest, false, String::new())
    };

    validate_arg_name(name)?;

    let resolved = if let Some(override_val) = cli_overrides.get(name) {
        // 1 & 2: CLI override (explicit value, or env-lookup already applied by parse_key_val).
        override_val.clone()
    } else if has_default {
        // 3: Bundlefile default with ${VAR} expansion.
        substitute(&raw_default, build_args)
    } else {
        // 4: No default, no CLI override → check the host environment.
        //    Mirrors `ARG NAME` in a Dockerfile picking up $NAME at build time.
        std::env::var(name).unwrap_or_default()
    };

    build_args.insert(name.to_string(), resolved);
    Ok(())
}

/// `ADD [--checksum=sha256:<hex>] <url-or-local-path> <dest>`
///
/// - Remote sources start with `http://` or `https://`.
/// - `--checksum=sha256:<hex>` is only valid for remote sources; using it with
///   a local path is an error.
/// - Exactly one source and one destination are required.
fn handle_add(rest: &str, args: &HashMap<String, String>) -> Result<AddDirective> {
    let rest = substitute(rest.trim(), args);
    let tokens: Vec<&str> = rest.split_whitespace().collect();

    if tokens.is_empty() {
        bail!(
            "ADD requires at least a source and a destination; \
             usage: ADD [--checksum=sha256:<hex>] <url-or-path> <dest>"
        );
    }

    let (flags, positional) = parse_flags(&tokens).context("invalid flag in ADD directive")?;

    // Reject unknown flags before anything else.
    for key in flags.keys() {
        if key.as_str() != "checksum" {
            bail!(
                "ADD: unknown flag '--{}'; the only supported flag is --checksum=sha256:<hex>",
                key
            );
        }
    }

    match positional.len() {
        0 | 1 => bail!(
            "ADD requires exactly 2 positional arguments (<source> <dest>), \
             got {};\n\
             usage: ADD [--checksum=sha256:<hex>] <url-or-path> <dest>",
            positional.len()
        ),
        2 => {}
        n => bail!(
            "ADD expects exactly 2 positional arguments (<source> <dest>), \
             got {};\n\
             usage: ADD [--checksum=sha256:<hex>] <url-or-path> <dest>",
            n
        ),
    }

    let source_str = &positional[0];
    let dest = positional[1].clone();
    let checksum = flags.get("checksum").cloned();

    let source = if is_url(source_str) {
        AddSource::Remote {
            url: source_str.clone(),
            checksum,
        }
    } else {
        if let Some(cs) = &checksum {
            bail!(
                "ADD: --checksum is only valid for remote URLs, not local paths\n\
                 source:   '{}'\n\
                 checksum: '{}'\n\
                 hint: remove --checksum, or change the source to an http:// or https:// URL",
                source_str,
                cs
            );
        }
        AddSource::Local {
            path: PathBuf::from(source_str),
        }
    };

    if dest.is_empty() {
        bail!("ADD destination path must not be empty");
    }

    Ok(AddDirective { source, dest })
}

/// `COPY [--from=<stage>] <src> <dest>`
///
/// - `<src>` is a local build-context path (or a path within the named source stage).
/// - `--from=<stage>` is either a zero-based decimal stage index or a stage name.
/// - Negative numeric indices are rejected at parse time; all other validation
///   (out-of-bounds index, unknown stage name) happens at build time.
fn handle_copy(rest: &str, args: &HashMap<String, String>) -> Result<CopyDirective> {
    let rest = substitute(rest.trim(), args);
    let tokens: Vec<&str> = rest.split_whitespace().collect();

    if tokens.is_empty() {
        bail!(
            "COPY requires at least a source and a destination; \
             usage: COPY [--from=<stage>] <src> <dest>"
        );
    }

    let (flags, positional) = parse_flags(&tokens).context("invalid flag in COPY directive")?;

    // Reject unknown flags before anything else.
    for key in flags.keys() {
        if key.as_str() != "from" {
            bail!(
                "COPY: unknown flag '--{}'; the only supported flag is --from=<stage>",
                key
            );
        }
    }

    match positional.len() {
        0 | 1 => bail!(
            "COPY requires exactly 2 positional arguments (<src> <dest>), \
             got {};\n\
             usage: COPY [--from=<stage>] <src> <dest>",
            positional.len()
        ),
        2 => {}
        n => bail!(
            "COPY expects exactly 2 positional arguments (<src> <dest>), \
             got {};\n\
             usage: COPY [--from=<stage>] <src> <dest>",
            n
        ),
    }

    let src = PathBuf::from(&positional[0]);
    let dest = positional[1].clone();

    let from = match flags.get("from") {
        None => CopyFrom::BuildContext,
        Some(stage_ref) if stage_ref.is_empty() => {
            bail!(
                "COPY: --from requires a value (stage index or name), \
                 e.g. --from=0 or --from=deps"
            );
        }
        Some(stage_ref) => {
            // Reject obviously invalid negative indices at parse time.
            // Out-of-bounds positive indices and unknown names are validated at build time.
            if let Ok(n) = stage_ref.parse::<i64>() {
                if n < 0 {
                    bail!(
                        "COPY: --from stage index must be non-negative (got {}); \
                         stages are zero-indexed",
                        n
                    );
                }
            }
            CopyFrom::Stage(stage_ref.clone())
        }
    };

    if dest.is_empty() {
        bail!("COPY destination path must not be empty");
    }

    Ok(CopyDirective { from, src, dest })
}

/// `MANAGE <config-path>: <key>, <key>, ...`
fn handle_manage(rest: &str, args: &HashMap<String, String>) -> Result<ManageDirective> {
    let rest = substitute(rest.trim(), args);

    // Split on `:` — the config path is before the colon, the keys after.
    let (config_path_raw, keys_raw) = rest.split_once(':').ok_or_else(|| {
        anyhow::anyhow!("MANAGE requires the form `<config-path>: key1, key2, ...`")
    })?;

    let config_path = config_path_raw.trim().to_string();
    if config_path.is_empty() {
        bail!("MANAGE config path must not be empty");
    }

    let keys: Vec<String> = keys_raw
        .split(',')
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .collect();

    if keys.is_empty() {
        bail!(
            "MANAGE '{}' declares no keys; at least one key is required",
            config_path
        );
    }

    Ok(ManageDirective { config_path, keys })
}

// ─── Substitution ─────────────────────────────────────────────────────────────

/// Replace all `${NAME}` occurrences in `s` with the corresponding value from
/// `args`. Unknown variable references are left as-is so forward references and
/// build-time variables can be resolved downstream without causing parse errors.
pub fn substitute(s: &str, args: &HashMap<String, String>) -> String {
    if !s.contains("${") {
        return s.to_string();
    }

    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut name = String::new();
            let mut closed = false;
            for inner in chars.by_ref() {
                if inner == '}' {
                    closed = true;
                    break;
                }
                name.push(inner);
            }
            if closed {
                if let Some(val) = args.get(&name) {
                    result.push_str(val);
                } else {
                    // Unknown variable — preserve the original `${NAME}` reference.
                    result.push_str("${");
                    result.push_str(&name);
                    result.push('}');
                }
            } else {
                // Unterminated `${` — emit literally.
                result.push_str("${");
                result.push_str(&name);
            }
        } else {
            result.push(ch);
        }
    }

    result
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Separate flag tokens (`--flag=value`) from positional arguments.
///
/// Returns `(flags, positionals)` where `flags` maps each flag name to its
/// value.  Only the `--flag=value` form is supported; `--flag` without `=`
/// stores an empty string as the value.
fn parse_flags(tokens: &[&str]) -> Result<(HashMap<String, String>, Vec<String>)> {
    let mut flags: HashMap<String, String> = HashMap::new();
    let mut positional: Vec<String> = Vec::new();

    for &tok in tokens {
        if let Some(flag_body) = tok.strip_prefix("--") {
            if flag_body.is_empty() {
                bail!("bare '--' is not a valid flag; use '--flag=value'");
            }
            let (key, val) = if let Some((k, v)) = flag_body.split_once('=') {
                if k.is_empty() {
                    bail!("flag '{}' has an empty name", tok);
                }
                (k.to_string(), v.to_string())
            } else {
                (flag_body.to_string(), String::new())
            };
            flags.insert(key, val);
        } else {
            positional.push(tok.to_string());
        }
    }

    Ok((flags, positional))
}

/// Return a mutable reference to the most recently pushed stage.
///
/// Fails with an actionable error when a directive appears before any `FROM`.
fn current_stage_mut(stages: &mut Vec<Stage>, lineno: usize) -> Result<&mut Stage> {
    stages
        .last_mut()
        .ok_or_else(|| anyhow::anyhow!("line {}: directive before any FROM", lineno))
}

/// Split a logical line into `(DIRECTIVE, rest)`.
///
/// The directive is the first whitespace-delimited token; `rest` is everything
/// after it with leading whitespace stripped.
fn split_directive(line: &str) -> (&str, &str) {
    match line.find(char::is_whitespace) {
        Some(idx) => (&line[..idx], line[idx..].trim_start()),
        None => (line, ""),
    }
}

/// Join lines that end with `\` into single logical lines.
///
/// The trailing backslash and any surrounding whitespace are stripped; the
/// following line's content is appended with a single space separator so that
/// split_whitespace() still tokenises correctly.
fn join_continuations(content: &str) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    let mut current = String::new();

    for raw_line in content.lines() {
        // Strip an optional Windows-style `\r`.
        let line = raw_line.trim_end_matches('\r');

        if line.trim_end().ends_with('\\') {
            let without_bs = line.trim_end().trim_end_matches('\\');
            current.push_str(without_bs);
            current.push(' ');
        } else {
            current.push_str(line);
            result.push(std::mem::take(&mut current));
        }
    }

    // Flush any trailing continuation that has no terminating line.
    if !current.is_empty() {
        result.push(current);
    }

    result
}

/// Return `true` if `s` looks like an HTTP or HTTPS URL.
fn is_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// Validate that `name` is a legal ARG identifier: `[A-Za-z_][A-Za-z0-9_]*`.
fn validate_arg_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("ARG name must not be empty");
    }
    let first = name.chars().next().unwrap();
    if first.is_ascii_digit() {
        bail!("ARG name '{}' must not start with a digit", name);
    }
    for ch in name.chars() {
        if !ch.is_ascii_alphanumeric() && ch != '_' {
            bail!(
                "ARG name '{}' contains invalid character '{}'; \
                 only [A-Za-z0-9_] is allowed",
                name,
                ch
            );
        }
    }
    Ok(())
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn no_overrides() -> HashMap<String, String> {
        HashMap::new()
    }

    // ── ADD ───────────────────────────────────────────────────────────────────

    #[test]
    fn add_local_basic() {
        let src = "FROM scratch\nADD ./config/Essentials/ plugins/Essentials/\n";
        let bf = parse(src, &no_overrides()).unwrap();
        let add = &bf.stages[0].adds[0];
        assert_eq!(add.dest, "plugins/Essentials/");
        match &add.source {
            AddSource::Local { path } => {
                assert_eq!(path, &PathBuf::from("./config/Essentials/"))
            }
            _ => panic!("expected Local source"),
        }
    }

    #[test]
    fn add_remote_no_checksum() {
        let src = "FROM scratch\nADD https://example.com/Plugin.jar plugins/Plugin.jar\n";
        let bf = parse(src, &no_overrides()).unwrap();
        let add = &bf.stages[0].adds[0];
        assert_eq!(add.dest, "plugins/Plugin.jar");
        match &add.source {
            AddSource::Remote { url, checksum } => {
                assert_eq!(url, "https://example.com/Plugin.jar");
                assert!(checksum.is_none());
            }
            _ => panic!("expected Remote source"),
        }
    }

    #[test]
    fn add_remote_with_checksum() {
        let src = concat!(
            "FROM scratch\n",
            "ADD --checksum=sha256:abc123def456 ",
            "https://example.com/Plugin.jar plugins/Plugin.jar\n",
        );
        let bf = parse(src, &no_overrides()).unwrap();
        let add = &bf.stages[0].adds[0];
        assert_eq!(add.dest, "plugins/Plugin.jar");
        match &add.source {
            AddSource::Remote { url, checksum } => {
                assert_eq!(url, "https://example.com/Plugin.jar");
                assert_eq!(checksum.as_deref(), Some("sha256:abc123def456"));
            }
            _ => panic!("expected Remote source"),
        }
    }

    #[test]
    fn add_checksum_on_local_path_is_error() {
        let src = "FROM scratch\nADD --checksum=sha256:abc123 ./local/path.jar plugins/path.jar\n";
        let err = parse(src, &no_overrides()).unwrap_err();
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("--checksum") && msg.contains("remote"),
            "error should mention --checksum and remote, got: {}",
            msg
        );
    }

    #[test]
    fn add_remote_too_few_args_is_error() {
        // Remote URL supplied but no dest.
        let src = "FROM scratch\nADD https://example.com/Plugin.jar\n";
        assert!(
            parse(src, &no_overrides()).is_err(),
            "missing dest should be an error"
        );
    }

    #[test]
    fn add_missing_dest_is_error() {
        // Local ADD with no dest.
        let src = "FROM scratch\nADD ./build/MyPlugin.jar\n";
        assert!(
            parse(src, &no_overrides()).is_err(),
            "missing dest should be an error"
        );
    }

    // ── COPY ──────────────────────────────────────────────────────────────────

    #[test]
    fn copy_local_no_from() {
        let src = "FROM scratch\nCOPY ./build/MyPlugin.jar plugins/MyPlugin.jar\n";
        let bf = parse(src, &no_overrides()).unwrap();
        let copy = &bf.stages[0].copies[0];
        assert_eq!(copy.src, PathBuf::from("./build/MyPlugin.jar"));
        assert_eq!(copy.dest, "plugins/MyPlugin.jar");
        assert!(
            matches!(&copy.from, CopyFrom::BuildContext),
            "expected CopyFrom::BuildContext"
        );
    }

    #[test]
    fn copy_from_numeric_zero() {
        let src = "FROM base:1\nFROM base:2\nCOPY --from=0 plugins/Foo.jar plugins/Foo.jar\n";
        let bf = parse(src, &no_overrides()).unwrap();
        let copy = &bf.stages[1].copies[0];
        match &copy.from {
            CopyFrom::Stage(s) => assert_eq!(s, "0"),
            _ => panic!("expected Stage(\"0\")"),
        }
    }

    #[test]
    fn copy_from_named_stage() {
        let src = concat!(
            "FROM base:1 AS deps\n",
            "FROM base:2\n",
            "COPY --from=deps mods/Bar.jar mods/Bar.jar\n",
        );
        let bf = parse(src, &no_overrides()).unwrap();
        let copy = &bf.stages[1].copies[0];
        match &copy.from {
            CopyFrom::Stage(s) => assert_eq!(s, "deps"),
            _ => panic!("expected Stage(\"deps\")"),
        }
    }

    #[test]
    fn copy_from_numeric_index() {
        // --from=1 referring to the second stage; stored verbatim as "1".
        let src = concat!(
            "FROM base:1 AS first\n",
            "FROM base:2 AS second\n",
            "FROM base:3\n",
            "COPY --from=1 mods/x.jar mods/x.jar\n",
        );
        let bf = parse(src, &no_overrides()).unwrap();
        let copy = &bf.stages[2].copies[0];
        match &copy.from {
            CopyFrom::Stage(s) => assert_eq!(s, "1"),
            _ => panic!("expected Stage(\"1\")"),
        }
    }

    #[test]
    fn copy_from_extra_positional_args_is_error() {
        // Three positional args (src + two destinations?) must be rejected.
        let src = "FROM scratch\nCOPY --from=0 ./a.jar ./b.jar extra\n";
        assert!(
            parse(src, &no_overrides()).is_err(),
            "extra positional arg should be an error"
        );
    }

    // ── FROM ──────────────────────────────────────────────────────────────────

    // ── ARG scoping ───────────────────────────────────────────────────────────
    #[test]
    fn arg_global_scope_available_in_all_stages() {
        // A pre-FROM ARG should be usable in every stage without re-declaration.
        let src = concat!(
            "ARG TAG=v1\n",
            "FROM base:${TAG} AS stage1\n",
            "ADD ./a.jar plugins/a.jar\n",
            "FROM base:${TAG} AS stage2\n",
            "ADD ./b.jar plugins/b.jar\n",
        );
        let bf = parse(src, &no_overrides()).unwrap();
        assert_eq!(bf.stages[0].from, "base:v1");
        assert_eq!(bf.stages[1].from, "base:v1");
        // build_args should record global args.
        assert_eq!(bf.build_args.get("TAG").unwrap(), "v1");
    }

    #[test]
    fn arg_stage_local_does_not_affect_other_stages() {
        // A stage-level ARG override must not bleed into a sibling stage.
        let src = concat!(
            "ARG VER=1\n",
            "FROM base:${VER} AS s1\n",
            "ARG VER=99\n", // local override in s1
            "ADD ./a.jar plugins/a-${VER}.jar\n",
            "FROM base:${VER} AS s2\n", // s2 should still see VER=1 from global
            "ADD ./b.jar plugins/b-${VER}.jar\n",
        );
        let bf = parse(src, &no_overrides()).unwrap();
        // s1's ADD uses the locally-overridden value.
        assert_eq!(bf.stages[0].adds[0].dest, "plugins/a-99.jar");
        // s2's FROM and ADD use the global value — stage s1's override is invisible here.
        assert_eq!(bf.stages[1].from, "base:1");
        assert_eq!(bf.stages[1].adds[0].dest, "plugins/b-1.jar");
    }

    #[test]
    fn arg_cli_override_wins_over_bundlefile_default() {
        let src = "ARG VER=1\nFROM base:${VER}\n";
        let overrides = HashMap::from([("VER".to_string(), "99".to_string())]);
        let bf = parse(src, &overrides).unwrap();
        assert_eq!(bf.stages[0].from, "base:99");
    }

    #[test]
    fn arg_env_fallback_when_no_default() {
        // ARG NAME with no default should pick up $NAME from the environment.
        std::env::set_var("MCPM_TEST_PARSER_ENV_12345", "env_value");
        let src = "ARG MCPM_TEST_PARSER_ENV_12345\nFROM base:${MCPM_TEST_PARSER_ENV_12345}\n";
        let bf = parse(src, &no_overrides()).unwrap();
        std::env::remove_var("MCPM_TEST_PARSER_ENV_12345");
        assert_eq!(bf.stages[0].from, "base:env_value");
    }

    #[test]
    fn arg_env_not_consulted_when_default_present() {
        // ARG NAME=default should NOT pick up the env var — the default wins
        // when no CLI override is present, matching Docker's behaviour.
        std::env::set_var("MCPM_TEST_PARSER_NOENV_12345", "should_not_appear");
        let src = "ARG MCPM_TEST_PARSER_NOENV_12345=bundlefile_default\n\
                   FROM base:${MCPM_TEST_PARSER_NOENV_12345}\n";
        let bf = parse(src, &no_overrides()).unwrap();
        std::env::remove_var("MCPM_TEST_PARSER_NOENV_12345");
        assert_eq!(bf.stages[0].from, "base:bundlefile_default");
    }

    #[test]
    fn arg_stage_level_no_default_checks_env() {
        // A stage-level ARG with no default should also fall back to env.
        std::env::set_var("MCPM_TEST_STAGE_ENV_12345", "from_env");
        let src = concat!(
            "FROM scratch\n",
            "ARG MCPM_TEST_STAGE_ENV_12345\n",
            "ADD ./a.jar plugins/a-${MCPM_TEST_STAGE_ENV_12345}.jar\n",
        );
        let bf = parse(src, &no_overrides()).unwrap();
        std::env::remove_var("MCPM_TEST_STAGE_ENV_12345");
        assert_eq!(bf.stages[0].adds[0].dest, "plugins/a-from_env.jar");
    }

    #[test]
    fn arg_chained_with_global_scope() {
        let src = "ARG BASE=1.0\nARG FULL=${BASE}-jre\nFROM img:${FULL}\n";
        let bf = parse(src, &no_overrides()).unwrap();
        assert_eq!(bf.stages[0].from, "img:1.0-jre");
    }

    // ── FROM ... AS name ──────────────────────────────────────────────────────

    #[test]
    fn from_as_name_parsed() {
        let src = "FROM base:1 AS build\nFROM base:2 AS runtime\n";
        let bf = parse(src, &no_overrides()).unwrap();
        assert_eq!(bf.stages[0].name.as_deref(), Some("build"));
        assert_eq!(bf.stages[1].name.as_deref(), Some("runtime"));
    }

    #[test]
    fn from_without_name() {
        let src = "FROM scratch\n";
        let bf = parse(src, &no_overrides()).unwrap();
        assert!(bf.stages[0].name.is_none());
    }

    #[test]
    fn multistage_from_names() {
        let src = concat!(
            "FROM base:1\n",
            "ADD ./a.jar plugins/a.jar\n",
            "\n",
            "FROM base:2\n",
            "ADD ./b.jar mods/b.jar\n",
        );
        let bf = parse(src, &no_overrides()).unwrap();
        assert_eq!(bf.stages[0].from, "base:1");
        assert_eq!(bf.stages[1].from, "base:2");
    }

    #[test]
    fn multistage_named_stages() {
        let src = "FROM base:1 AS deps\nFROM base:2 AS runtime\n";
        let bf = parse(src, &no_overrides()).unwrap();
        assert_eq!(bf.stages[0].name.as_deref(), Some("deps"));
        assert_eq!(bf.stages[1].name.as_deref(), Some("runtime"));
    }

    // ── ARG ───────────────────────────────────────────────────────────────────

    #[test]
    fn arg_default_substitution() {
        let src = "ARG VERSION=2.20.1\nFROM ghcr.io/author/bundle:${VERSION}\n";
        let bf = parse(src, &no_overrides()).unwrap();
        assert_eq!(bf.stages[0].from, "ghcr.io/author/bundle:2.20.1");
        assert_eq!(bf.build_args.get("VERSION").unwrap(), "2.20.1");
    }

    #[test]
    fn arg_cli_override() {
        let src = "ARG VERSION=2.20.1\nFROM base:${VERSION}\n";
        let overrides = HashMap::from([("VERSION".to_string(), "3.0.0".to_string())]);
        let bf = parse(src, &overrides).unwrap();
        assert_eq!(bf.stages[0].from, "base:3.0.0");
        assert_eq!(bf.build_args.get("VERSION").unwrap(), "3.0.0");
    }

    #[test]
    fn arg_substituted_in_add_dest() {
        let src = "ARG DIR=plugins\nFROM scratch\nADD ./build/P.jar ${DIR}/P.jar\n";
        let bf = parse(src, &no_overrides()).unwrap();
        assert_eq!(bf.stages[0].adds[0].dest, "plugins/P.jar");
    }

    #[test]
    fn arg_chaining() {
        // ARG BASE=1.0 then ARG FULL=${BASE}-jre should resolve to "1.0-jre".
        let src = "ARG BASE=1.0\nARG FULL=${BASE}-jre\nFROM img:${FULL}\n";
        let bf = parse(src, &no_overrides()).unwrap();
        assert_eq!(bf.stages[0].from, "img:1.0-jre");
    }

    // ── MANAGE ────────────────────────────────────────────────────────────────

    #[test]
    fn manage_directive() {
        let src = concat!(
            "FROM scratch\n",
            "MANAGE plugins/Essentials/config.yml: home.bed-respawn, homes.max-homes\n",
        );
        let bf = parse(src, &no_overrides()).unwrap();
        let m = &bf.stages[0].manages[0];
        assert_eq!(m.config_path, "plugins/Essentials/config.yml");
        assert_eq!(m.keys, vec!["home.bed-respawn", "homes.max-homes"]);
    }

    #[test]
    fn manage_same_path_overrides_keys() {
        let src = concat!(
            "FROM scratch\n",
            "MANAGE plugins/A/config.yml: key.a\n",
            "MANAGE plugins/A/config.yml: key.b\n",
        );
        let bf = parse(src, &no_overrides()).unwrap();
        // Second MANAGE for the same config path wins.
        assert_eq!(bf.stages[0].manages.len(), 1);
        assert_eq!(bf.stages[0].manages[0].keys, vec!["key.b"]);
    }

    // ── Line continuation ─────────────────────────────────────────────────────

    #[test]
    fn line_continuation_basic() {
        // dest is provided on the continuation line.
        let src = concat!(
            "FROM scratch\n",
            "ADD https://example.com/Plugin.jar \\\n",
            "  plugins/Plugin.jar\n",
        );
        let bf = parse(src, &no_overrides()).unwrap();
        let add = &bf.stages[0].adds[0];
        assert_eq!(add.dest, "plugins/Plugin.jar");
        match &add.source {
            AddSource::Remote { url, .. } => {
                assert_eq!(url, "https://example.com/Plugin.jar")
            }
            _ => panic!("expected Remote source"),
        }
    }

    #[test]
    fn line_continuation_across_add_flags_and_args() {
        // Flag, source URL, and dest each on their own continuation line.
        let src = concat!(
            "FROM scratch\n",
            "ADD \\\n",
            "  --checksum=sha256:deadbeef \\\n",
            "  https://example.com/Plugin.jar \\\n",
            "  plugins/Plugin.jar\n",
        );
        let bf = parse(src, &no_overrides()).unwrap();
        let add = &bf.stages[0].adds[0];
        assert_eq!(add.dest, "plugins/Plugin.jar");
        match &add.source {
            AddSource::Remote { url, checksum } => {
                assert_eq!(url, "https://example.com/Plugin.jar");
                assert_eq!(checksum.as_deref(), Some("sha256:deadbeef"));
            }
            _ => panic!("expected Remote source"),
        }
    }

    // ── Comments ──────────────────────────────────────────────────────────────

    #[test]
    fn comments_ignored() {
        let src = concat!(
            "# This is a comment\n",
            "FROM scratch\n",
            "# another comment\n",
            "ADD ./x.jar plugins/x.jar\n",
        );
        let bf = parse(src, &no_overrides()).unwrap();
        assert_eq!(bf.stages[0].adds.len(), 1);
    }

    // ── Error cases ───────────────────────────────────────────────────────────

    #[test]
    fn no_from_is_error() {
        let src = "ARG VERSION=1.0\n";
        assert!(parse(src, &no_overrides()).is_err());
    }

    #[test]
    fn directive_before_from_is_error() {
        let src = "ADD ./x.jar plugins/x.jar\nFROM scratch\n";
        assert!(parse(src, &no_overrides()).is_err());
    }

    // ── substitute ────────────────────────────────────────────────────────────

    #[test]
    fn substitute_unknown_var_preserved() {
        let mut args = HashMap::new();
        args.insert("A".to_string(), "hello".to_string());
        let result = substitute("${A}-${B}", &args);
        assert_eq!(result, "hello-${B}");
    }

    // ── Multi-stage build ─────────────────────────────────────────────────────

    #[test]
    fn multistage_build() {
        let src = concat!(
            "FROM base:1\n",
            "ADD ./a.jar plugins/a.jar\n",
            "\n",
            "FROM base:2\n",
            "ADD ./b.jar mods/b.jar\n",
        );
        let bf = parse(src, &no_overrides()).unwrap();
        assert_eq!(bf.stages.len(), 2);
        assert_eq!(bf.stages[0].adds[0].dest, "plugins/a.jar");
        assert_eq!(bf.stages[1].adds[0].dest, "mods/b.jar");
    }
}
