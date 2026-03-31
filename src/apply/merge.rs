//! Format-aware config file merging.
//!
//! When `bundle apply` encounters a config file that already exists on disk, it
//! merges the on-disk version with the incoming bundle version:
//!
//! - **Managed keys** (declared via `MANAGE` in the Bundlefile, stored in the
//!   OCI image `bundle.managed-keys` annotation): bundle's value wins.
//! - **All other keys**: on-disk value is kept unchanged.
//!
//! Supported formats (detected by file extension):
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
//! For YAML / TOML / JSON a managed key like `homes.max-homes` is a
//! **dot-separated path** into the value tree: navigate into the `homes`
//! mapping and set the `max-homes` leaf.
//!
//! For `.properties` the managed key is the **literal property key** (the dot
//! is part of the key name, not a path separator), because `.properties` is a
//! flat format with no nesting.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};

/// Merge `on_disk` and `from_bundle` bytes, returning the merged bytes.
///
/// `managed_keys` is a list of dot-separated key paths (or literal property
/// keys for `.properties`) that should take their value from the bundle.
///
/// `path` is used only to detect the config format via its extension.
///
/// Returns `None` when the path has an unrecognised extension — the caller
/// should overwrite with the bundle version in that case.
pub fn merge_config(
    on_disk: &[u8],
    from_bundle: &[u8],
    managed_keys: &[String],
    path: &Path,
) -> Result<Option<Vec<u8>>> {
    match detect_format(path) {
        Some(ConfigFormat::Yaml) => merge_yaml(on_disk, from_bundle, managed_keys).map(Some),
        Some(ConfigFormat::Toml) => merge_toml(on_disk, from_bundle, managed_keys).map(Some),
        Some(ConfigFormat::Json) => merge_json(on_disk, from_bundle, managed_keys).map(Some),
        Some(ConfigFormat::Properties) => {
            merge_properties(on_disk, from_bundle, managed_keys).map(Some)
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

fn merge_yaml(on_disk: &[u8], from_bundle: &[u8], managed_keys: &[String]) -> Result<Vec<u8>> {
    let disk_str = std::str::from_utf8(on_disk).context("on-disk YAML is not valid UTF-8")?;
    let bundle_str = std::str::from_utf8(from_bundle).context("bundle YAML is not valid UTF-8")?;

    let mut disk_val: serde_yaml::Value =
        serde_yaml::from_str(disk_str).context("parsing on-disk YAML")?;
    let bundle_val: serde_yaml::Value =
        serde_yaml::from_str(bundle_str).context("parsing bundle YAML")?;

    for key_path in managed_keys {
        let segments: Vec<&str> = key_path.split('.').collect();
        if let Some(bundle_leaf) = get_yaml_nested(&bundle_val, &segments) {
            set_yaml_nested(&mut disk_val, &segments, bundle_leaf.clone());
        }
        // If the bundle doesn't have the managed key, the on-disk value is
        // preserved unchanged.
    }

    let out = serde_yaml::to_string(&disk_val).context("serialising merged YAML")?;
    Ok(out.into_bytes())
}

/// Recursively navigate into `val` following the dot-split `path`.
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

/// Set a leaf deep inside `val` at `path`, creating intermediate mappings if
/// required.
fn set_yaml_nested(val: &mut serde_yaml::Value, path: &[&str], new_val: serde_yaml::Value) {
    if path.is_empty() {
        *val = new_val;
        return;
    }

    // Coerce non-mapping nodes to mappings so we can descend into them.
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

fn merge_toml(on_disk: &[u8], from_bundle: &[u8], managed_keys: &[String]) -> Result<Vec<u8>> {
    let disk_str = std::str::from_utf8(on_disk).context("on-disk TOML is not valid UTF-8")?;
    let bundle_str = std::str::from_utf8(from_bundle).context("bundle TOML is not valid UTF-8")?;

    let mut disk_val: toml::Value = toml::from_str(disk_str).context("parsing on-disk TOML")?;
    let bundle_val: toml::Value = toml::from_str(bundle_str).context("parsing bundle TOML")?;

    for key_path in managed_keys {
        let segments: Vec<&str> = key_path.split('.').collect();
        if let Some(bundle_leaf) = get_toml_nested(&bundle_val, &segments) {
            set_toml_nested(&mut disk_val, &segments, bundle_leaf.clone());
        }
    }

    let out = toml::to_string_pretty(&disk_val).context("serialising merged TOML")?;
    Ok(out.into_bytes())
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

fn merge_json(on_disk: &[u8], from_bundle: &[u8], managed_keys: &[String]) -> Result<Vec<u8>> {
    let disk_str = std::str::from_utf8(on_disk).context("on-disk JSON is not valid UTF-8")?;
    let bundle_str = std::str::from_utf8(from_bundle).context("bundle JSON is not valid UTF-8")?;

    let mut disk_val: serde_json::Value =
        serde_json::from_str(disk_str).context("parsing on-disk JSON")?;
    let bundle_val: serde_json::Value =
        serde_json::from_str(bundle_str).context("parsing bundle JSON")?;

    for key_path in managed_keys {
        let segments: Vec<&str> = key_path.split('.').collect();
        if let Some(bundle_leaf) = get_json_nested(&bundle_val, &segments) {
            set_json_nested(&mut disk_val, &segments, bundle_leaf.clone());
        }
    }

    serde_json::to_vec_pretty(&disk_val).context("serialising merged JSON")
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

/// Java `.properties` merge.
///
/// The format is a flat key=value file — there is no nesting.  A managed key
/// like `home.bed-respawn` is the *literal* property key; the dot is part of
/// the key name.
///
/// Ordering and comments from the on-disk file are preserved line-by-line.
/// Bundle-only managed keys that do not appear on disk are appended at the end.
fn merge_properties(
    on_disk: &[u8],
    from_bundle: &[u8],
    managed_keys: &[String],
) -> Result<Vec<u8>> {
    let disk_str =
        std::str::from_utf8(on_disk).context("on-disk .properties is not valid UTF-8")?;
    let bundle_str =
        std::str::from_utf8(from_bundle).context("bundle .properties is not valid UTF-8")?;

    let bundle_props = parse_properties(bundle_str)?;
    let managed_set: HashSet<&str> = managed_keys.iter().map(String::as_str).collect();

    // Replay the on-disk file line by line, substituting managed keys.
    let mut output = String::new();
    let mut written_keys: HashSet<String> = HashSet::new();

    for logical in logical_property_lines(disk_str) {
        match logical {
            PropertyLine::Comment(c) => {
                output.push_str(&c);
                output.push('\n');
            }
            PropertyLine::Blank => {
                output.push('\n');
            }
            PropertyLine::KeyValue { key, raw_line } => {
                if managed_set.contains(key.as_str()) {
                    // Replace with bundle value if present, otherwise keep disk.
                    if let Some(bundle_val) = bundle_props.get(&key) {
                        output.push_str(&format!(
                            "{}={}\n",
                            escape_property_key(&key),
                            escape_property_value(bundle_val)
                        ));
                        written_keys.insert(key);
                        continue;
                    }
                }
                output.push_str(&raw_line);
                output.push('\n');
                written_keys.insert(key);
            }
        }
    }

    // Append bundle-only managed keys not present on disk.
    for key in managed_keys {
        if !written_keys.contains(key) {
            if let Some(bundle_val) = bundle_props.get(key) {
                output.push_str(&format!(
                    "{}={}\n",
                    escape_property_key(key),
                    escape_property_value(bundle_val)
                ));
            }
        }
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
    /// trailing newline) for faithful round-tripping of non-managed keys.
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
        } else {
            if let Some((key, _val)) = parse_property_kv(trimmed) {
                result.push(PropertyLine::KeyValue {
                    key,
                    raw_line: raw.to_string(),
                });
            } else {
                // Malformed line — preserve it as a comment.
                result.push(PropertyLine::Comment(raw.to_string()));
            }
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
/// Escapes `=`, `:`, `#`, `!`, `\`, and leading/trailing whitespace.
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
/// Escapes `\\`, newlines, and form-feeds.  Leading whitespace is preserved
/// as-is (the reader trims leading separator whitespace, so we don't need to
/// escape it here).
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

    #[test]
    fn yaml_managed_key_taken_from_bundle() {
        let disk = b"homes:\n  max-homes: 3\n  bed-respawn: false\n";
        let bundle = b"homes:\n  max-homes: 10\n  bed-respawn: true\n";
        let managed = vec!["homes.max-homes".to_string()];
        let path = Path::new("config.yml");

        let merged = merge_config(disk, bundle, &managed, path).unwrap().unwrap();
        let merged_val: serde_yaml::Value = serde_yaml::from_slice(&merged).unwrap();

        // Managed key: bundle wins.
        assert_eq!(
            merged_val["homes"]["max-homes"],
            serde_yaml::Value::from(10)
        );
        // Non-managed key: disk wins.
        assert_eq!(
            merged_val["homes"]["bed-respawn"],
            serde_yaml::Value::from(false)
        );
    }

    #[test]
    fn yaml_non_managed_keys_preserved() {
        let disk = b"user-key: disk-value\nbundle-key: disk-version\n";
        let bundle = b"user-key: bundle-value\nbundle-key: bundle-version\n";
        let managed = vec!["bundle-key".to_string()];
        let path = Path::new("plugins/A/config.yml");

        let merged = merge_config(disk, bundle, &managed, path).unwrap().unwrap();
        let merged_val: serde_yaml::Value = serde_yaml::from_slice(&merged).unwrap();

        assert_eq!(
            merged_val["user-key"],
            serde_yaml::Value::String("disk-value".to_string())
        );
        assert_eq!(
            merged_val["bundle-key"],
            serde_yaml::Value::String("bundle-version".to_string())
        );
    }

    #[test]
    fn yaml_missing_managed_key_in_bundle_preserved_from_disk() {
        let disk = b"key: disk-value\n";
        let bundle = b"other: something\n";
        let managed = vec!["key".to_string()];
        let path = Path::new("config.yml");

        let merged = merge_config(disk, bundle, &managed, path).unwrap().unwrap();
        let merged_val: serde_yaml::Value = serde_yaml::from_slice(&merged).unwrap();

        assert_eq!(
            merged_val["key"],
            serde_yaml::Value::String("disk-value".to_string())
        );
    }

    #[test]
    fn yaml_deeply_nested_key() {
        let disk = b"a:\n  b:\n    c: disk\n    d: disk\n";
        let bundle = b"a:\n  b:\n    c: bundle\n    d: bundle\n";
        let managed = vec!["a.b.c".to_string()];
        let path = Path::new("config.yml");

        let merged = merge_config(disk, bundle, &managed, path).unwrap().unwrap();
        let merged_val: serde_yaml::Value = serde_yaml::from_slice(&merged).unwrap();

        assert_eq!(
            merged_val["a"]["b"]["c"],
            serde_yaml::Value::String("bundle".to_string())
        );
        assert_eq!(
            merged_val["a"]["b"]["d"],
            serde_yaml::Value::String("disk".to_string())
        );
    }

    #[test]
    fn yaml_empty_managed_keys_leaves_disk_unchanged() {
        let disk = b"key: disk-value\n";
        let bundle = b"key: bundle-value\n";
        let path = Path::new("config.yml");

        let merged = merge_config(disk, bundle, &[], path).unwrap().unwrap();
        let merged_val: serde_yaml::Value = serde_yaml::from_slice(&merged).unwrap();
        assert_eq!(
            merged_val["key"],
            serde_yaml::Value::String("disk-value".to_string())
        );
    }

    #[test]
    fn toml_managed_key_from_bundle() {
        let disk = b"[database]\nurl = \"disk-url\"\nport = 3306\n";
        let bundle = b"[database]\nurl = \"bundle-url\"\nport = 5432\n";
        let managed = vec!["database.url".to_string()];
        let path = Path::new("config.toml");

        let merged = merge_config(disk, bundle, &managed, path).unwrap().unwrap();
        let merged_val: toml::Value =
            toml::from_str(std::str::from_utf8(&merged).unwrap()).unwrap();

        assert_eq!(
            merged_val["database"]["url"],
            toml::Value::String("bundle-url".to_string())
        );
        assert_eq!(merged_val["database"]["port"], toml::Value::Integer(3306));
    }

    #[test]
    fn toml_top_level_key() {
        let disk = b"name = \"disk-name\"\nversion = \"1.0\"\n";
        let bundle = b"name = \"bundle-name\"\nversion = \"2.0\"\n";
        let managed = vec!["version".to_string()];
        let path = Path::new("plugin.toml");

        let merged = merge_config(disk, bundle, &managed, path).unwrap().unwrap();
        let merged_val: toml::Value =
            toml::from_str(std::str::from_utf8(&merged).unwrap()).unwrap();

        assert_eq!(
            merged_val["name"],
            toml::Value::String("disk-name".to_string())
        );
        assert_eq!(
            merged_val["version"],
            toml::Value::String("2.0".to_string())
        );
    }

    #[test]
    fn json_managed_key_from_bundle() {
        let disk = br#"{"config":{"timeout":30,"retries":3}}"#;
        let bundle = br#"{"config":{"timeout":60,"retries":5}}"#;
        let managed = vec!["config.timeout".to_string()];
        let path = Path::new("settings.json");

        let merged = merge_config(disk, bundle, &managed, path).unwrap().unwrap();
        let merged_val: serde_json::Value = serde_json::from_slice(&merged).unwrap();

        assert_eq!(merged_val["config"]["timeout"], 60);
        assert_eq!(merged_val["config"]["retries"], 3);
    }

    #[test]
    fn json_non_config_extension_returns_none() {
        let merged = merge_config(b"data", b"data", &[], Path::new("file.jar")).unwrap();
        assert!(merged.is_none());
    }

    #[test]
    fn properties_managed_key_from_bundle() {
        let disk = b"# Server config\nserver-port=25565\nmax-players=20\n";
        let bundle = b"server-port=25566\nmax-players=100\n";
        let managed = vec!["max-players".to_string()];
        let path = Path::new("server.properties");

        let merged = merge_config(disk, bundle, &managed, path).unwrap().unwrap();
        let merged_str = std::str::from_utf8(&merged).unwrap();
        let merged_props = parse_properties(merged_str).unwrap();

        // Managed key: bundle wins.
        assert_eq!(
            merged_props.get("max-players").map(String::as_str),
            Some("100")
        );
        // Non-managed key: disk value preserved.
        assert_eq!(
            merged_props.get("server-port").map(String::as_str),
            Some("25565")
        );
    }

    #[test]
    fn properties_comments_preserved() {
        let disk = b"# This is a comment\nkey=disk-value\n";
        let bundle = b"key=bundle-value\n";
        let managed = vec!["key".to_string()];
        let path = Path::new("server.properties");

        let merged = merge_config(disk, bundle, &managed, path).unwrap().unwrap();
        let merged_str = std::str::from_utf8(&merged).unwrap();
        assert!(merged_str.contains("# This is a comment"));
    }

    #[test]
    fn properties_bundle_only_key_appended() {
        // The disk file doesn't have `new-key` but bundle declares it as managed.
        let disk = b"existing=value\n";
        let bundle = b"existing=other\nnew-key=bundle-new\n";
        let managed = vec!["new-key".to_string()];
        let path = Path::new("server.properties");

        let merged = merge_config(disk, bundle, &managed, path).unwrap().unwrap();
        let merged_props = parse_properties(std::str::from_utf8(&merged).unwrap()).unwrap();

        assert_eq!(
            merged_props.get("existing").map(String::as_str),
            Some("value")
        );
        assert_eq!(
            merged_props.get("new-key").map(String::as_str),
            Some("bundle-new")
        );
    }

    #[test]
    fn properties_dotted_key_is_literal() {
        // In .properties, `home.bed-respawn` is the literal key — not a path.
        let disk = b"home.bed-respawn=false\nhomes.max-homes=3\n";
        let bundle = b"home.bed-respawn=true\nhomes.max-homes=10\n";
        let managed = vec!["homes.max-homes".to_string()];
        let path = Path::new("essentials.properties");

        let merged = merge_config(disk, bundle, &managed, path).unwrap().unwrap();
        let merged_props = parse_properties(std::str::from_utf8(&merged).unwrap()).unwrap();

        assert_eq!(
            merged_props.get("home.bed-respawn").map(String::as_str),
            Some("false")
        );
        assert_eq!(
            merged_props.get("homes.max-homes").map(String::as_str),
            Some("10")
        );
    }

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
