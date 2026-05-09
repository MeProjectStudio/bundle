//! Format-aware config file merging.
//!
//! When `bundle apply` encounters a config file that already exists on disk, it
//! merges the on-disk version with the incoming bundle version.  The bundle is
//! the authoritative source: its values win for every key by default.  Keys
//! declared in a `PRESERVE` directive are the exception — for those, the
//! user's on-disk value is kept unchanged.
//!
//! - **All keys by default**: bundle's value wins (on-disk value replaced).
//! - **Preserved keys** (declared via `PRESERVE` in the Bundlefile, stored in
//!   the OCI image `bundle.preserve-keys` annotation): user's on-disk value
//!   is kept unchanged.
//!
//! ## Preserve patterns
//!
//! Keys in a `PRESERVE` directive may be exact paths or glob-style patterns:
//!
//! | Pattern   | Matches (YAML / TOML / JSON)                        |
//! |-----------|-----------------------------------------------------|
//! | `key`     | exactly the key `key`                               |
//! | `a.b`     | the nested path `a → b`                             |
//! | `a.*`     | `a.foo`, `a.bar`, … (any single child of `a`)       |
//! | `a.**`    | `a.foo`, `a.foo.bar`, … (any descendant of `a`)     |
//! | `**`      | every key at every depth                            |
//!
//! For `.properties` (flat format) patterns use standard glob semantics:
//! `*` matches any sequence of characters in the key name (including `.`),
//! so `*` alone preserves every key.  The dot is part of the key name, not
//! a path separator.
//!
//! ## Supported formats
//!
//! | Extension(s)      | Parser / serialiser   |
//! |-------------------|-----------------------|
//! | `.yml`, `.yaml`   | `serde_yaml`          |
//! | `.toml`           | `toml`                |
//! | `.json`           | `serde_json`          |
//! | `.properties`     | built-in key=value    |
//!
//! Non-config files (jars, `.so`, binaries, …) are not handled here — callers
//! should simply overwrite them with the bundle version.
//!
//! ## Key path convention
//!
//! For YAML / TOML / JSON a key path like `homes.max-homes` is a
//! **dot-separated path** into the value tree.
//!
//! For `.properties` the key is the **literal property key** (the dot is part
//! of the key name, not a path separator).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};

/// Merge `on_disk` and `from_bundle` bytes, returning the merged bytes.
///
/// `preserve_keys` is a list of dot-separated key paths (or glob patterns)
/// whose on-disk values should be kept unchanged.  All other keys take the
/// bundle's value.
///
/// `path` is used only to detect the config format via its extension.
///
/// Returns `None` when the path has an unrecognised extension — the caller
/// should overwrite with the bundle version in that case.
pub fn merge_config(
    on_disk: &[u8],
    from_bundle: &[u8],
    preserve_keys: &[String],
    path: &Path,
) -> Result<Option<Vec<u8>>> {
    match detect_format(path) {
        Some(ConfigFormat::Yaml) => merge_yaml(on_disk, from_bundle, preserve_keys).map(Some),
        Some(ConfigFormat::Toml) => merge_toml(on_disk, from_bundle, preserve_keys).map(Some),
        Some(ConfigFormat::Json) => merge_json(on_disk, from_bundle, preserve_keys).map(Some),
        Some(ConfigFormat::Properties) => {
            merge_properties(on_disk, from_bundle, preserve_keys).map(Some)
        }
        None => Ok(None),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFormat {
    Yaml,
    Toml,
    Json,
    Properties,
}

pub fn detect_format(path: &Path) -> Option<ConfigFormat> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    match ext.as_str() {
        "yml" | "yaml" => Some(ConfigFormat::Yaml),
        "toml" => Some(ConfigFormat::Toml),
        "json" => Some(ConfigFormat::Json),
        "properties" => Some(ConfigFormat::Properties),
        _ => None,
    }
}

// ── Pattern matching ──────────────────────────────────────────────────────────

/// Return `true` if the dot-joined `path` matches any of the `patterns`.
///
/// Used for structured formats (YAML / TOML / JSON) where paths are
/// dot-separated sequences of key names.
fn path_matches_any(path: &str, patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let path_segs: Vec<&str> = path.split('.').collect();
    patterns.iter().any(|pattern| {
        let pat_segs: Vec<&str> = pattern.split('.').collect();
        segs_match(&path_segs, &pat_segs)
    })
}

/// Segment-by-segment recursive matcher.
///
/// - A literal segment matches only the identical segment.
/// - `*` matches any single segment.
/// - `**` matches zero or more consecutive segments.
fn segs_match(path: &[&str], pattern: &[&str]) -> bool {
    if pattern.is_empty() {
        return path.is_empty();
    }
    if pattern[0] == "**" {
        let rest = &pattern[1..];
        // Try consuming 0, 1, 2, … path segments with **.
        for i in 0..=path.len() {
            if segs_match(&path[i..], rest) {
                return true;
            }
        }
        return false;
    }
    if path.is_empty() {
        return false;
    }
    if pattern[0] == "*" || pattern[0] == path[0] {
        segs_match(&path[1..], &pattern[1..])
    } else {
        false
    }
}

/// Return `true` if the flat `.properties` `key` matches any of the `patterns`.
///
/// Uses standard glob semantics via the `glob` crate.  Since `.properties`
/// keys have no nesting, `*` matches any sequence of characters (including
/// the literal `.` that appears in many Java property key names).
fn properties_key_matches_any(key: &str, patterns: &[String]) -> bool {
    let opts = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: false,
        require_literal_leading_dot: false,
    };
    patterns.iter().any(|pattern| {
        glob::Pattern::new(pattern)
            .map(|p| p.matches_with(key, opts))
            .unwrap_or(false)
    })
}

// ── YAML ──────────────────────────────────────────────────────────────────────

/// Bundle-based YAML merge.
///
/// The result starts as the bundle document.  For every leaf path in the
/// on-disk document that matches a preserve pattern, the disk value is
/// written back into the result.
fn merge_yaml(on_disk: &[u8], from_bundle: &[u8], preserve_keys: &[String]) -> Result<Vec<u8>> {
    let disk_str = std::str::from_utf8(on_disk).context("on-disk YAML is not valid UTF-8")?;
    let bundle_str = std::str::from_utf8(from_bundle).context("bundle YAML is not valid UTF-8")?;

    let disk_val: serde_yaml::Value =
        serde_yaml::from_str(disk_str).context("parsing on-disk YAML")?;
    let mut result_val: serde_yaml::Value =
        serde_yaml::from_str(bundle_str).context("parsing bundle YAML")?;

    // For each leaf in the on-disk document: if its path matches a preserve
    // pattern, overwrite the bundle-seeded result with the disk value.
    for segments in collect_yaml_leaf_paths(&disk_val, &[]) {
        let path_str = segments.join(".");
        if path_matches_any(&path_str, preserve_keys) {
            let seg_refs: Vec<&str> = segments.iter().map(String::as_str).collect();
            if let Some(disk_leaf) = get_yaml_nested(&disk_val, &seg_refs) {
                set_yaml_nested(&mut result_val, &seg_refs, disk_leaf.clone());
            }
        }
    }

    let out = serde_yaml::to_string(&result_val).context("serialising merged YAML")?;
    Ok(out.into_bytes())
}

/// Enumerate every leaf path in a YAML value tree.
///
/// Each returned `Vec<String>` is the sequence of key names leading to one
/// scalar (or non-mapping) node.  Intermediate mapping nodes are not included.
fn collect_yaml_leaf_paths(val: &serde_yaml::Value, prefix: &[String]) -> Vec<Vec<String>> {
    if let serde_yaml::Value::Mapping(map) = val {
        let mut paths = Vec::new();
        for (k, v) in map {
            if let serde_yaml::Value::String(key) = k {
                let mut new_prefix = prefix.to_vec();
                new_prefix.push(key.clone());
                paths.extend(collect_yaml_leaf_paths(v, &new_prefix));
            }
        }
        paths
    } else if prefix.is_empty() {
        vec![]
    } else {
        vec![prefix.to_vec()]
    }
}

fn get_yaml_nested<'a>(val: &'a serde_yaml::Value, path: &[&str]) -> Option<&'a serde_yaml::Value> {
    if path.is_empty() {
        return Some(val);
    }
    if let serde_yaml::Value::Mapping(map) = val {
        let key = serde_yaml::Value::String(path[0].to_string());
        let child = map.get(&key)?;
        get_yaml_nested(child, &path[1..])
    } else {
        None
    }
}

fn set_yaml_nested(val: &mut serde_yaml::Value, path: &[&str], new_val: serde_yaml::Value) {
    if path.is_empty() {
        *val = new_val;
        return;
    }

    if !val.is_mapping() {
        *val = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
    }

    let map = val.as_mapping_mut().unwrap();
    let key = serde_yaml::Value::String(path[0].to_string());

    if path.len() == 1 {
        map.insert(key, new_val);
    } else {
        let child = map
            .entry(key)
            .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
        set_yaml_nested(child, &path[1..], new_val);
    }
}

// ── TOML ──────────────────────────────────────────────────────────────────────

fn merge_toml(on_disk: &[u8], from_bundle: &[u8], preserve_keys: &[String]) -> Result<Vec<u8>> {
    let disk_str = std::str::from_utf8(on_disk).context("on-disk TOML is not valid UTF-8")?;
    let bundle_str = std::str::from_utf8(from_bundle).context("bundle TOML is not valid UTF-8")?;

    let disk_val: toml::Value = toml::from_str(disk_str).context("parsing on-disk TOML")?;
    let mut result_val: toml::Value = toml::from_str(bundle_str).context("parsing bundle TOML")?;

    for segments in collect_toml_leaf_paths(&disk_val, &[]) {
        let path_str = segments.join(".");
        if path_matches_any(&path_str, preserve_keys) {
            let seg_refs: Vec<&str> = segments.iter().map(String::as_str).collect();
            if let Some(disk_leaf) = get_toml_nested(&disk_val, &seg_refs) {
                set_toml_nested(&mut result_val, &seg_refs, disk_leaf.clone());
            }
        }
    }

    let out = toml::to_string_pretty(&result_val).context("serialising merged TOML")?;
    Ok(out.into_bytes())
}

fn collect_toml_leaf_paths(val: &toml::Value, prefix: &[String]) -> Vec<Vec<String>> {
    if let toml::Value::Table(table) = val {
        let mut paths = Vec::new();
        for (k, v) in table {
            let mut new_prefix = prefix.to_vec();
            new_prefix.push(k.clone());
            paths.extend(collect_toml_leaf_paths(v, &new_prefix));
        }
        paths
    } else if prefix.is_empty() {
        vec![]
    } else {
        vec![prefix.to_vec()]
    }
}

fn get_toml_nested<'a>(val: &'a toml::Value, path: &[&str]) -> Option<&'a toml::Value> {
    if path.is_empty() {
        return Some(val);
    }
    if let toml::Value::Table(table) = val {
        let child = table.get(path[0])?;
        get_toml_nested(child, &path[1..])
    } else {
        None
    }
}

fn set_toml_nested(val: &mut toml::Value, path: &[&str], new_val: toml::Value) {
    if path.is_empty() {
        *val = new_val;
        return;
    }

    if !matches!(val, toml::Value::Table(_)) {
        *val = toml::Value::Table(toml::value::Table::new());
    }

    if let toml::Value::Table(table) = val {
        if path.len() == 1 {
            table.insert(path[0].to_string(), new_val);
        } else {
            let child = table
                .entry(path[0].to_string())
                .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
            set_toml_nested(child, &path[1..], new_val);
        }
    }
}

// ── JSON ──────────────────────────────────────────────────────────────────────

fn merge_json(on_disk: &[u8], from_bundle: &[u8], preserve_keys: &[String]) -> Result<Vec<u8>> {
    let disk_str = std::str::from_utf8(on_disk).context("on-disk JSON is not valid UTF-8")?;
    let bundle_str = std::str::from_utf8(from_bundle).context("bundle JSON is not valid UTF-8")?;

    let disk_val: serde_json::Value =
        serde_json::from_str(disk_str).context("parsing on-disk JSON")?;
    let mut result_val: serde_json::Value =
        serde_json::from_str(bundle_str).context("parsing bundle JSON")?;

    for segments in collect_json_leaf_paths(&disk_val, &[]) {
        let path_str = segments.join(".");
        if path_matches_any(&path_str, preserve_keys) {
            let seg_refs: Vec<&str> = segments.iter().map(String::as_str).collect();
            if let Some(disk_leaf) = get_json_nested(&disk_val, &seg_refs) {
                set_json_nested(&mut result_val, &seg_refs, disk_leaf.clone());
            }
        }
    }

    serde_json::to_vec_pretty(&result_val).context("serialising merged JSON")
}

fn collect_json_leaf_paths(val: &serde_json::Value, prefix: &[String]) -> Vec<Vec<String>> {
    if let serde_json::Value::Object(map) = val {
        let mut paths = Vec::new();
        for (k, v) in map {
            let mut new_prefix = prefix.to_vec();
            new_prefix.push(k.clone());
            paths.extend(collect_json_leaf_paths(v, &new_prefix));
        }
        paths
    } else if prefix.is_empty() {
        vec![]
    } else {
        vec![prefix.to_vec()]
    }
}

fn get_json_nested<'a>(val: &'a serde_json::Value, path: &[&str]) -> Option<&'a serde_json::Value> {
    if path.is_empty() {
        return Some(val);
    }
    if let serde_json::Value::Object(map) = val {
        let child = map.get(path[0])?;
        get_json_nested(child, &path[1..])
    } else {
        None
    }
}

fn set_json_nested(val: &mut serde_json::Value, path: &[&str], new_val: serde_json::Value) {
    if path.is_empty() {
        *val = new_val;
        return;
    }

    if !val.is_object() {
        *val = serde_json::Value::Object(serde_json::Map::new());
    }

    if let serde_json::Value::Object(map) = val {
        if path.len() == 1 {
            map.insert(path[0].to_string(), new_val);
        } else {
            let child = map
                .entry(path[0].to_string())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            set_json_nested(child, &path[1..], new_val);
        }
    }
}

// ── .properties ───────────────────────────────────────────────────────────────

/// Java `.properties` merge.
///
/// The format is a flat key=value file — there is no nesting.  A preserve
/// pattern like `home.bed-respawn` is matched against the *literal* property
/// key; the dot is part of the key name, not a path separator.
///
/// The bundle file is replayed line-by-line as the authoritative base.
/// Preserved keys (matched via glob) take the disk value when present.
/// Keys present only on disk (user additions absent from the bundle) are
/// appended sorted at the end.
fn merge_properties(
    on_disk: &[u8],
    from_bundle: &[u8],
    preserve_keys: &[String],
) -> Result<Vec<u8>> {
    let disk_str =
        std::str::from_utf8(on_disk).context("on-disk .properties is not valid UTF-8")?;
    let bundle_str =
        std::str::from_utf8(from_bundle).context("bundle .properties is not valid UTF-8")?;

    let disk_props = parse_properties(disk_str)?;

    // Replay the bundle file line by line — bundle is the authoritative base.
    let mut output = String::new();
    let mut written_keys: HashSet<String> = HashSet::new();

    for logical in logical_property_lines(bundle_str) {
        match logical {
            PropertyLine::Comment(c) => {
                output.push_str(&c);
                output.push('\n');
            }
            PropertyLine::Blank => {
                output.push('\n');
            }
            PropertyLine::KeyValue { key, raw_line } => {
                written_keys.insert(key.clone());
                if properties_key_matches_any(&key, preserve_keys) {
                    if let Some(disk_val) = disk_props.get(&key) {
                        // Preserved key with a disk value — keep the user's value.
                        output.push_str(&format!(
                            "{}={}\n",
                            escape_property_key(&key),
                            escape_property_value(disk_val)
                        ));
                        continue;
                    }
                }
                // Non-preserved, or preserved but not present on disk:
                // emit the bundle line verbatim.
                output.push_str(&raw_line);
                output.push('\n');
            }
        }
    }

    // Append disk-only keys (user additions absent from the bundle).
    // Sorted for deterministic output.
    let mut disk_only: Vec<(&String, &String)> = disk_props
        .iter()
        .filter(|(k, _)| !written_keys.contains(*k))
        .collect();
    disk_only.sort_by_key(|(k, _)| k.as_str());
    for (key, val) in disk_only {
        output.push_str(&format!(
            "{}={}\n",
            escape_property_key(key),
            escape_property_value(val)
        ));
    }

    Ok(output.into_bytes())
}

#[derive(Debug)]
enum PropertyLine {
    /// A comment or blank comment line (starts with `#` or `!`).
    Comment(String),
    /// A truly blank line.
    Blank,
    /// A key=value pair.  `raw_line` is the original line text (without the
    /// trailing newline) for faithful round-tripping of non-preserved keys.
    KeyValue { key: String, raw_line: String },
}

/// Parse a `.properties` string into a sequence of [`PropertyLine`] values,
/// joining continuation lines.
fn logical_property_lines(content: &str) -> Vec<PropertyLine> {
    let mut result = Vec::new();
    let mut pending_continuation: Option<String> = None;

    for raw in content.lines() {
        if let Some(ref mut acc) = pending_continuation {
            // This line continues the previous logical line.
            let trimmed = raw.trim_start();
            if ends_with_continuation(trimmed) {
                acc.push_str(trimmed.trim_end_matches('\\').trim_end());
                acc.push(' ');
                continue;
            } else {
                acc.push_str(trimmed);
                let logical = std::mem::take(acc);
                pending_continuation = None;
                if let Some(kv) = parse_property_kv(&logical) {
                    result.push(PropertyLine::KeyValue {
                        key: kv.0,
                        raw_line: logical,
                    });
                }
                continue;
            }
        }

        let trimmed = raw.trim();
        if trimmed.is_empty() {
            result.push(PropertyLine::Blank);
        } else if trimmed.starts_with('#') || trimmed.starts_with('!') {
            result.push(PropertyLine::Comment(raw.to_string()));
        } else if ends_with_continuation(trimmed) {
            // Start accumulating a continuation.
            let without_bs = trimmed.trim_end_matches('\\').trim_end();
            pending_continuation = Some(format!("{} ", without_bs));
        } else if let Some((key, _val)) = parse_property_kv(trimmed) {
            result.push(PropertyLine::KeyValue {
                key,
                raw_line: raw.to_string(),
            });
        } else {
            // Malformed line — preserve it as a comment.
            result.push(PropertyLine::Comment(raw.to_string()));
        }
    }

    // Flush any unterminated continuation.
    if let Some(acc) = pending_continuation {
        if let Some((key, _)) = parse_property_kv(&acc) {
            result.push(PropertyLine::KeyValue { key, raw_line: acc });
        }
    }

    result
}

/// Return `true` if the (already-trimmed) line ends with an odd number of `\`.
fn ends_with_continuation(line: &str) -> bool {
    let count = line.chars().rev().take_while(|&c| c == '\\').count();
    count % 2 == 1
}

/// Split a logical property line into `(key, value)`.
///
/// The key/value separator is the first unescaped `=`, `:`, or whitespace.
/// Returns `None` if no key can be extracted.
fn parse_property_kv(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
        return None;
    }

    // Scan for end of key: first unescaped `=`, `:`, or whitespace.
    let mut key_end = line.len();
    let chars = line.char_indices().peekable();
    let mut escaped = false;

    for (i, c) in chars {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            continue;
        }
        if c == '=' || c == ':' || c == ' ' || c == '\t' {
            key_end = i;
            break;
        }
    }

    let raw_key = &line[..key_end];
    if raw_key.is_empty() {
        return None;
    }

    let after_key = line[key_end..].trim_start_matches(['=', ':', ' ', '\t']);
    let value = after_key.to_string();

    Some((unescape_property(raw_key), unescape_property(&value)))
}

fn parse_properties(content: &str) -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    for line in logical_property_lines(content) {
        if let PropertyLine::KeyValue { key, raw_line } = line {
            if let Some((k, v)) = parse_property_kv(raw_line.trim()) {
                map.insert(k, v);
            } else {
                map.insert(key, String::new());
            }
        }
    }
    Ok(map)
}

/// Unescape a `.properties` key or value string.
///
/// Handles `\\`, `\n`, `\r`, `\t`, `\f`, and `\uXXXX`.
fn unescape_property(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '\\' {
            result.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => result.push('\n'),
            Some('r') => result.push('\r'),
            Some('t') => result.push('\t'),
            Some('f') => result.push('\x0C'),
            Some('\\') => result.push('\\'),
            Some('u') => {
                // \uXXXX
                let hex: String = chars.by_ref().take(4).collect();
                if let Ok(n) = u32::from_str_radix(&hex, 16) {
                    if let Some(ch) = char::from_u32(n) {
                        result.push(ch);
                        continue;
                    }
                }
                // Failed to parse — emit literally.
                result.push('\\');
                result.push('u');
                result.push_str(&hex);
            }
            Some(other) => {
                // Unknown escape — emit literally.
                result.push('\\');
                result.push(other);
            }
            None => result.push('\\'),
        }
    }

    result
}

/// Escape a key for use in a `.properties` file.
///
/// Escapes `=`, `:`, `#`, `!`, `\`, and leading whitespace.
fn escape_property_key(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for (i, c) in s.chars().enumerate() {
        match c {
            ' ' if i == 0 => result.push_str("\\ "),
            '\\' => result.push_str("\\\\"),
            '=' => result.push_str("\\="),
            ':' => result.push_str("\\:"),
            '#' => result.push_str("\\#"),
            '!' => result.push_str("\\!"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            other => result.push(other),
        }
    }
    result
}

/// Escape a value for use in a `.properties` file.
///
/// Escapes `\\`, newlines, and form-feeds.
fn escape_property_value(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            other => result.push(other),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── format detection ──────────────────────────────────────────────────────

    #[test]
    fn format_yml() {
        assert_eq!(
            detect_format(Path::new("config.yml")),
            Some(ConfigFormat::Yaml)
        );
    }

    #[test]
    fn format_yaml() {
        assert_eq!(
            detect_format(Path::new("config.yaml")),
            Some(ConfigFormat::Yaml)
        );
    }

    #[test]
    fn format_toml() {
        assert_eq!(
            detect_format(Path::new("Cargo.toml")),
            Some(ConfigFormat::Toml)
        );
    }

    #[test]
    fn format_json() {
        assert_eq!(
            detect_format(Path::new("data.json")),
            Some(ConfigFormat::Json)
        );
    }

    #[test]
    fn format_properties() {
        assert_eq!(
            detect_format(Path::new("server.properties")),
            Some(ConfigFormat::Properties)
        );
    }

    #[test]
    fn format_jar_is_none() {
        assert_eq!(detect_format(Path::new("Plugin.jar")), None);
    }

    #[test]
    fn format_so_is_none() {
        assert_eq!(detect_format(Path::new("native.so")), None);
    }

    // ── segs_match ────────────────────────────────────────────────────────────

    #[test]
    fn segs_match_literal_equal() {
        assert!(segs_match(&["a", "b"], &["a", "b"]));
    }

    #[test]
    fn segs_match_literal_not_equal() {
        assert!(!segs_match(&["a", "b"], &["a", "c"]));
    }

    #[test]
    fn segs_match_star_single_segment() {
        assert!(segs_match(&["a", "foo"], &["a", "*"]));
        assert!(segs_match(&["a", "bar"], &["a", "*"]));
    }

    #[test]
    fn segs_match_star_does_not_cross_segment_boundary() {
        // "a.*" must NOT match the three-segment path "a.b.c".
        assert!(!segs_match(&["a", "b", "c"], &["a", "*"]));
    }

    #[test]
    fn segs_match_double_star_any_depth() {
        assert!(segs_match(&["a"], &["**"]));
        assert!(segs_match(&["a", "b"], &["**"]));
        assert!(segs_match(&["a", "b", "c"], &["**"]));
    }

    #[test]
    fn segs_match_double_star_zero_segments() {
        // "a.**" matches "a" itself because ** can match zero segments.
        assert!(segs_match(&["a"], &["a", "**"]));
        assert!(segs_match(&["a", "b"], &["a", "**"]));
        assert!(segs_match(&["a", "b", "c"], &["a", "**"]));
    }

    #[test]
    fn segs_match_double_star_in_middle() {
        assert!(segs_match(&["a", "b"], &["a", "**", "b"]));
        assert!(segs_match(&["a", "x", "b"], &["a", "**", "b"]));
        assert!(segs_match(&["a", "x", "y", "b"], &["a", "**", "b"]));
        assert!(!segs_match(&["a", "x", "c"], &["a", "**", "b"]));
    }

    #[test]
    fn segs_match_mixed_star_and_literal() {
        assert!(segs_match(
            &["key", "foo", "password"],
            &["key", "*", "password"]
        ));
        // Four segments should NOT match the three-segment pattern.
        assert!(!segs_match(
            &["key", "foo", "bar", "password"],
            &["key", "*", "password"]
        ));
    }

    #[test]
    fn segs_match_empty_both() {
        assert!(segs_match(&[], &[]));
    }

    #[test]
    fn segs_match_empty_path_non_empty_literal_pattern() {
        assert!(!segs_match(&[], &["a"]));
    }

    #[test]
    fn segs_match_empty_path_double_star_matches() {
        // ** can match zero segments, so it matches an empty path.
        assert!(segs_match(&[], &["**"]));
    }

    // ── path_matches_any ──────────────────────────────────────────────────────

    #[test]
    fn path_matches_any_exact() {
        assert!(path_matches_any("a.b.c", &["a.b.c".to_string()]));
        assert!(!path_matches_any("a.b.d", &["a.b.c".to_string()]));
    }

    #[test]
    fn path_matches_any_star_pattern() {
        assert!(path_matches_any(
            "settings.volume",
            &["settings.*".to_string()]
        ));
        // Two levels deep — not matched by single *.
        assert!(!path_matches_any(
            "settings.a.b",
            &["settings.*".to_string()]
        ));
    }

    #[test]
    fn path_matches_any_double_star_pattern() {
        assert!(path_matches_any("a.b.c", &["a.**".to_string()]));
        assert!(path_matches_any("a.b", &["a.**".to_string()]));
    }

    #[test]
    fn path_matches_any_global_double_star() {
        assert!(path_matches_any(
            "anything.nested.deep",
            &["**".to_string()]
        ));
    }

    #[test]
    fn path_matches_any_no_patterns() {
        assert!(!path_matches_any("anything", &[]));
    }

    // ── YAML ──────────────────────────────────────────────────────────────────

    #[test]
    fn yaml_preserve_key_kept_from_disk() {
        let disk = b"homes:\n  max-homes: 3\n  bed-respawn: false\n";
        let bundle = b"homes:\n  max-homes: 10\n  bed-respawn: true\n";
        let preserve = vec!["homes.max-homes".to_string()];
        let path = Path::new("config.yml");

        let merged = merge_config(disk, bundle, &preserve, path)
            .unwrap()
            .unwrap();
        let merged_val: serde_yaml::Value = serde_yaml::from_slice(&merged).unwrap();

        // Preserved key: disk wins.
        assert_eq!(merged_val["homes"]["max-homes"], serde_yaml::Value::from(3));
        // Non-preserved key: bundle wins.
        assert_eq!(
            merged_val["homes"]["bed-respawn"],
            serde_yaml::Value::from(true)
        );
    }

    #[test]
    fn yaml_non_preserve_key_overwritten_by_bundle() {
        let disk = b"user-key: disk-value\nbundle-key: disk-version\n";
        let bundle = b"user-key: bundle-value\nbundle-key: bundle-version\n";
        let preserve = vec!["bundle-key".to_string()];
        let path = Path::new("plugins/A/config.yml");

        let merged = merge_config(disk, bundle, &preserve, path)
            .unwrap()
            .unwrap();
        let merged_val: serde_yaml::Value = serde_yaml::from_slice(&merged).unwrap();

        // Not preserved: bundle wins.
        assert_eq!(
            merged_val["user-key"],
            serde_yaml::Value::String("bundle-value".to_string())
        );
        // Preserved: disk wins.
        assert_eq!(
            merged_val["bundle-key"],
            serde_yaml::Value::String("disk-version".to_string())
        );
    }

    #[test]
    fn yaml_preserve_key_missing_from_disk_uses_bundle_value() {
        // When disk does not have the preserved key, the bundle's value is used.
        let disk = b"other: something\n";
        let bundle = b"key: bundle-value\n";
        let preserve = vec!["key".to_string()];
        let path = Path::new("config.yml");

        let merged = merge_config(disk, bundle, &preserve, path)
            .unwrap()
            .unwrap();
        let merged_val: serde_yaml::Value = serde_yaml::from_slice(&merged).unwrap();

        assert_eq!(
            merged_val["key"],
            serde_yaml::Value::String("bundle-value".to_string())
        );
    }

    #[test]
    fn yaml_deeply_nested_preserve_key() {
        let disk = b"a:\n  b:\n    c: disk\n    d: disk\n";
        let bundle = b"a:\n  b:\n    c: bundle\n    d: bundle\n";
        let preserve = vec!["a.b.c".to_string()];
        let path = Path::new("config.yml");

        let merged = merge_config(disk, bundle, &preserve, path)
            .unwrap()
            .unwrap();
        let merged_val: serde_yaml::Value = serde_yaml::from_slice(&merged).unwrap();

        // Preserved: disk wins.
        assert_eq!(
            merged_val["a"]["b"]["c"],
            serde_yaml::Value::String("disk".to_string())
        );
        // Non-preserved: bundle wins.
        assert_eq!(
            merged_val["a"]["b"]["d"],
            serde_yaml::Value::String("bundle".to_string())
        );
    }

    #[test]
    fn yaml_empty_preserve_keys_bundle_wins_all() {
        // No preserve keys → bundle wins every key.
        let disk = b"key: disk-value\n";
        let bundle = b"key: bundle-value\n";
        let path = Path::new("config.yml");

        let merged = merge_config(disk, bundle, &[], path).unwrap().unwrap();
        let merged_val: serde_yaml::Value = serde_yaml::from_slice(&merged).unwrap();

        assert_eq!(
            merged_val["key"],
            serde_yaml::Value::String("bundle-value".to_string())
        );
    }

    #[test]
    fn yaml_glob_star_matches_any_single_segment() {
        let disk = b"settings:\n  volume: 50\n  lang: en-US\n";
        let bundle = b"settings:\n  volume: 100\n  lang: de-DE\n";
        let preserve = vec!["settings.*".to_string()];
        let path = Path::new("config.yml");

        let merged = merge_config(disk, bundle, &preserve, path)
            .unwrap()
            .unwrap();
        let merged_val: serde_yaml::Value = serde_yaml::from_slice(&merged).unwrap();

        // Both direct children of `settings` match `settings.*` → disk wins.
        assert_eq!(
            merged_val["settings"]["volume"],
            serde_yaml::Value::from(50)
        );
        assert_eq!(
            merged_val["settings"]["lang"],
            serde_yaml::Value::String("en-US".to_string())
        );
    }

    #[test]
    fn yaml_glob_star_does_not_cross_segment_boundary() {
        // "a.*" must NOT match "a.b.c" — that path is two levels below `a`.
        let disk = b"a:\n  b:\n    c: disk\n";
        let bundle = b"a:\n  b:\n    c: bundle\n";
        let preserve = vec!["a.*".to_string()];
        let path = Path::new("config.yml");

        let merged = merge_config(disk, bundle, &preserve, path)
            .unwrap()
            .unwrap();
        let merged_val: serde_yaml::Value = serde_yaml::from_slice(&merged).unwrap();

        assert_eq!(
            merged_val["a"]["b"]["c"],
            serde_yaml::Value::String("bundle".to_string())
        );
    }

    #[test]
    fn yaml_glob_double_star_matches_any_depth() {
        let disk = b"a:\n  b:\n    c: disk\n  d: disk\n";
        let bundle = b"a:\n  b:\n    c: bundle\n  d: bundle\n";
        let preserve = vec!["a.**".to_string()];
        let path = Path::new("config.yml");

        let merged = merge_config(disk, bundle, &preserve, path)
            .unwrap()
            .unwrap();
        let merged_val: serde_yaml::Value = serde_yaml::from_slice(&merged).unwrap();

        // Both descendants of `a` match `a.**` → disk wins at any depth.
        assert_eq!(
            merged_val["a"]["b"]["c"],
            serde_yaml::Value::String("disk".to_string())
        );
        assert_eq!(
            merged_val["a"]["d"],
            serde_yaml::Value::String("disk".to_string())
        );
    }

    #[test]
    fn yaml_glob_double_star_alone_preserves_all() {
        let disk = b"x: disk-x\ny: disk-y\n";
        let bundle = b"x: bundle-x\ny: bundle-y\n";
        let preserve = vec!["**".to_string()];
        let path = Path::new("config.yml");

        let merged = merge_config(disk, bundle, &preserve, path)
            .unwrap()
            .unwrap();
        let merged_val: serde_yaml::Value = serde_yaml::from_slice(&merged).unwrap();

        assert_eq!(
            merged_val["x"],
            serde_yaml::Value::String("disk-x".to_string())
        );
        assert_eq!(
            merged_val["y"],
            serde_yaml::Value::String("disk-y".to_string())
        );
    }

    // ── TOML ──────────────────────────────────────────────────────────────────

    #[test]
    fn toml_preserve_key_from_disk() {
        let disk = b"[database]\nurl = \"disk-url\"\nport = 3306\n";
        let bundle = b"[database]\nurl = \"bundle-url\"\nport = 5432\n";
        let preserve = vec!["database.url".to_string()];
        let path = Path::new("config.toml");

        let merged = merge_config(disk, bundle, &preserve, path)
            .unwrap()
            .unwrap();
        let merged_val: toml::Value =
            toml::from_str(std::str::from_utf8(&merged).unwrap()).unwrap();

        // Preserved: disk wins.
        assert_eq!(
            merged_val["database"]["url"],
            toml::Value::String("disk-url".to_string())
        );
        // Non-preserved: bundle wins.
        assert_eq!(merged_val["database"]["port"], toml::Value::Integer(5432));
    }

    #[test]
    fn toml_top_level_preserve_key() {
        let disk = b"name = \"disk-name\"\nversion = \"1.0\"\n";
        let bundle = b"name = \"bundle-name\"\nversion = \"2.0\"\n";
        let preserve = vec!["version".to_string()];
        let path = Path::new("plugin.toml");

        let merged = merge_config(disk, bundle, &preserve, path)
            .unwrap()
            .unwrap();
        let merged_val: toml::Value =
            toml::from_str(std::str::from_utf8(&merged).unwrap()).unwrap();

        // Non-preserved: bundle wins.
        assert_eq!(
            merged_val["name"],
            toml::Value::String("bundle-name".to_string())
        );
        // Preserved: disk wins.
        assert_eq!(
            merged_val["version"],
            toml::Value::String("1.0".to_string())
        );
    }

    // ── JSON ──────────────────────────────────────────────────────────────────

    #[test]
    fn json_preserve_key_from_disk() {
        let disk = br#"{"config":{"timeout":30,"retries":3}}"#;
        let bundle = br#"{"config":{"timeout":60,"retries":5}}"#;
        let preserve = vec!["config.timeout".to_string()];
        let path = Path::new("settings.json");

        let merged = merge_config(disk, bundle, &preserve, path)
            .unwrap()
            .unwrap();
        let merged_val: serde_json::Value = serde_json::from_slice(&merged).unwrap();

        // Preserved: disk wins.
        assert_eq!(merged_val["config"]["timeout"], 30);
        // Non-preserved: bundle wins.
        assert_eq!(merged_val["config"]["retries"], 5);
    }

    #[test]
    fn json_non_config_extension_returns_none() {
        let merged = merge_config(b"data", b"data", &[], Path::new("file.jar")).unwrap();
        assert!(merged.is_none());
    }

    // ── .properties ───────────────────────────────────────────────────────────

    #[test]
    fn properties_preserve_key_from_disk() {
        let disk = b"server-port=25565\nmax-players=20\n";
        let bundle = b"server-port=25566\nmax-players=100\n";
        let preserve = vec!["max-players".to_string()];
        let path = Path::new("server.properties");

        let merged = merge_config(disk, bundle, &preserve, path)
            .unwrap()
            .unwrap();
        let merged_str = std::str::from_utf8(&merged).unwrap();
        let merged_props = parse_properties(merged_str).unwrap();

        // Preserved key: disk wins.
        assert_eq!(
            merged_props.get("max-players").map(String::as_str),
            Some("20")
        );
        // Non-preserved key: bundle wins.
        assert_eq!(
            merged_props.get("server-port").map(String::as_str),
            Some("25566")
        );
    }

    #[test]
    fn properties_bundle_comments_preserved() {
        // Comments are replayed from the bundle (the authoritative base),
        // not from the disk file.
        let disk = b"key=disk-value\n";
        let bundle = b"# Bundle comment\nkey=bundle-value\n";
        let preserve = vec!["key".to_string()];
        let path = Path::new("server.properties");

        let merged = merge_config(disk, bundle, &preserve, path)
            .unwrap()
            .unwrap();
        let merged_str = std::str::from_utf8(&merged).unwrap();

        assert!(merged_str.contains("# Bundle comment"));
        // Preserved key uses the disk value.
        let merged_props = parse_properties(merged_str).unwrap();
        assert_eq!(
            merged_props.get("key").map(String::as_str),
            Some("disk-value")
        );
    }

    #[test]
    fn properties_disk_only_key_appended() {
        // Keys present only on disk (user additions) are appended at the end.
        let disk = b"existing=disk-val\nuser-added=user-val\n";
        let bundle = b"existing=bundle-val\n";
        let preserve: Vec<String> = vec![];
        let path = Path::new("server.properties");

        let merged = merge_config(disk, bundle, &preserve, path)
            .unwrap()
            .unwrap();
        let merged_props = parse_properties(std::str::from_utf8(&merged).unwrap()).unwrap();

        // Non-preserved bundle key: bundle wins.
        assert_eq!(
            merged_props.get("existing").map(String::as_str),
            Some("bundle-val")
        );
        // Disk-only key: appended from disk.
        assert_eq!(
            merged_props.get("user-added").map(String::as_str),
            Some("user-val")
        );
    }

    #[test]
    fn properties_preserve_key_missing_from_disk_uses_bundle_value() {
        // When disk does not have the preserved key, the bundle value is used.
        let disk = b"other=something\n";
        let bundle = b"key=bundle-val\n";
        let preserve = vec!["key".to_string()];
        let path = Path::new("config.properties");

        let merged = merge_config(disk, bundle, &preserve, path)
            .unwrap()
            .unwrap();
        let merged_props = parse_properties(std::str::from_utf8(&merged).unwrap()).unwrap();

        // Preserved but not on disk → bundle value used.
        assert_eq!(
            merged_props.get("key").map(String::as_str),
            Some("bundle-val")
        );
        // Disk-only key → appended.
        assert_eq!(
            merged_props.get("other").map(String::as_str),
            Some("something")
        );
    }

    #[test]
    fn properties_dotted_key_is_literal() {
        // In .properties, `home.bed-respawn` is the literal key — not a path.
        let disk = b"home.bed-respawn=false\nhomes.max-homes=3\n";
        let bundle = b"home.bed-respawn=true\nhomes.max-homes=10\n";
        let preserve = vec!["homes.max-homes".to_string()];
        let path = Path::new("essentials.properties");

        let merged = merge_config(disk, bundle, &preserve, path)
            .unwrap()
            .unwrap();
        let merged_props = parse_properties(std::str::from_utf8(&merged).unwrap()).unwrap();

        // Non-preserved: bundle wins.
        assert_eq!(
            merged_props.get("home.bed-respawn").map(String::as_str),
            Some("true")
        );
        // Preserved literal dotted key: disk wins.
        assert_eq!(
            merged_props.get("homes.max-homes").map(String::as_str),
            Some("3")
        );
    }

    #[test]
    fn properties_glob_star_preserves_all() {
        // "*" matches every key → all disk values are kept.
        let disk = b"server-port=25565\nmax-players=20\n";
        let bundle = b"server-port=25566\nmax-players=100\n";
        let preserve = vec!["*".to_string()];
        let path = Path::new("server.properties");

        let merged = merge_config(disk, bundle, &preserve, path)
            .unwrap()
            .unwrap();
        let merged_props = parse_properties(std::str::from_utf8(&merged).unwrap()).unwrap();

        assert_eq!(
            merged_props.get("server-port").map(String::as_str),
            Some("25565")
        );
        assert_eq!(
            merged_props.get("max-players").map(String::as_str),
            Some("20")
        );
    }

    #[test]
    fn properties_glob_prefix_star_matches() {
        // "plugin.*" preserves keys whose name starts with "plugin."
        // (dot is a literal character in .properties key names).
        let disk = b"plugin.timeout=5000\nplugin.retry=3\nother=yes\n";
        let bundle = b"plugin.timeout=10000\nplugin.retry=10\nother=no\n";
        let preserve = vec!["plugin.*".to_string()];
        let path = Path::new("config.properties");

        let merged = merge_config(disk, bundle, &preserve, path)
            .unwrap()
            .unwrap();
        let merged_props = parse_properties(std::str::from_utf8(&merged).unwrap()).unwrap();

        // plugin.* keys: disk wins.
        assert_eq!(
            merged_props.get("plugin.timeout").map(String::as_str),
            Some("5000")
        );
        assert_eq!(
            merged_props.get("plugin.retry").map(String::as_str),
            Some("3")
        );
        // Not matched: bundle wins.
        assert_eq!(merged_props.get("other").map(String::as_str), Some("no"));
    }

    // ── unescape / escape / parse helpers ─────────────────────────────────────

    #[test]
    fn unescape_newline() {
        assert_eq!(unescape_property("line1\\nline2"), "line1\nline2");
    }

    #[test]
    fn unescape_backslash() {
        assert_eq!(unescape_property("C:\\\\Windows"), "C:\\Windows");
    }

    #[test]
    fn unescape_unicode() {
        assert_eq!(unescape_property("\\u0041"), "A");
    }

    #[test]
    fn unescape_noop_plain() {
        assert_eq!(unescape_property("hello world"), "hello world");
    }

    #[test]
    fn parse_kv_equals_sep() {
        let (k, v) = parse_property_kv("key=value").unwrap();
        assert_eq!(k, "key");
        assert_eq!(v, "value");
    }

    #[test]
    fn parse_kv_colon_sep() {
        let (k, v) = parse_property_kv("key: value").unwrap();
        assert_eq!(k, "key");
        assert_eq!(v, "value");
    }

    #[test]
    fn parse_kv_space_sep() {
        let (k, v) = parse_property_kv("key value").unwrap();
        assert_eq!(k, "key");
        assert_eq!(v, "value");
    }

    #[test]
    fn parse_kv_no_value() {
        let (k, v) = parse_property_kv("bare-key").unwrap();
        assert_eq!(k, "bare-key");
        assert_eq!(v, "");
    }

    #[test]
    fn parse_kv_comment_returns_none() {
        assert!(parse_property_kv("# comment").is_none());
        assert!(parse_property_kv("! comment").is_none());
    }

    #[test]
    fn parse_kv_empty_returns_none() {
        assert!(parse_property_kv("").is_none());
        assert!(parse_property_kv("   ").is_none());
    }
}
