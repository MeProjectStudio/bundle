//! Encoding, decoding, and merging of the `bundle.preserve-keys` OCI annotation.
//!
//! The annotation is stored on the OCI image manifest as a JSON-encoded map:
//!
//! ```json
//! {
//!   "plugins/Essentials/config.yml": ["home.bed-respawn", "homes.max-homes"],
//!   "plugins/LuckPerms/config.yml":  ["storage-method"]
//! }
//! ```
//!
//! During multi-stage builds the annotation is accumulated across stages using
//! a last-writer-wins strategy per config path: if stage N declares a `PRESERVE`
//! for `plugins/A/config.yml` with keys `[k1]`, and stage N+1 declares the
//! same path with keys `[k2]`, the final annotation contains only `[k2]` for
//! that path.

use std::collections::HashMap;

use anyhow::{Context, Result};

/// The OCI manifest annotation key under which mcpm stores managed-keys info.
pub const PRESERVE_KEYS_ANNOTATION: &str = "bundle.preserve-keys";

/// A map from server-root-relative config file path to the list of
/// dot-separated key paths that this bundle owns.
///
/// Example:
/// ```text
/// "plugins/Essentials/config.yml" → ["home.bed-respawn", "homes.max-homes"]
/// ```
pub type PreserveKeys = HashMap<String, Vec<String>>;

/// Encode a `PreserveKeys` map to a compact JSON string suitable for storing as
/// an OCI manifest annotation value.
///
/// Keys are sorted for deterministic output (important for content-addressable
/// manifests).
pub fn encode(keys: &PreserveKeys) -> Result<String> {
    // Sort config-path keys so the output is deterministic.
    let sorted: std::collections::BTreeMap<&str, &Vec<String>> =
        keys.iter().map(|(k, v)| (k.as_str(), v)).collect();

    serde_json::to_string(&sorted).context("encoding bundle.preserve-keys annotation to JSON")
}

/// Decode a JSON string (previously produced by [`encode`]) back into a
/// `PreserveKeys` map.
///
/// Returns an empty map for an empty or whitespace-only string so that callers
/// do not need to special-case missing annotations.
pub fn decode(s: &str) -> Result<PreserveKeys> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(PreserveKeys::new());
    }
    serde_json::from_str(trimmed)
        .with_context(|| format!("decoding bundle.preserve-keys annotation: {:?}", trimmed))
}

/// Merge `override_keys` on top of `base_keys` using last-writer-wins per
/// config path.
///
/// This is called during multi-stage builds: `base_keys` represents the
/// accumulated annotation from all earlier stages, and `override_keys` is the
/// contribution from the current stage.
///
/// For any config path present in both maps, the value from `override_keys`
/// wins.  Config paths present only in `base_keys` are preserved unchanged.
pub fn merge(mut base: PreserveKeys, overrides: PreserveKeys) -> PreserveKeys {
    for (path, keys) in overrides {
        base.insert(path, keys);
    }
    base
}

/// Build a `PreserveKeys` map from the `PRESERVE` directives of a single stage.
///
/// This is a convenience wrapper used in `bundle/build.rs`.
pub fn from_preserve_directives(
    directives: &[crate::bundlefile::types::PreserveDirective],
) -> PreserveKeys {
    let mut map = PreserveKeys::new();
    for directive in directives {
        map.insert(directive.config_path.clone(), directive.keys.clone());
    }
    map
}

/// Extract the `PreserveKeys` annotation from an OCI manifest's annotation map.
///
/// Returns an empty map if the annotation is absent.
pub fn from_manifest_annotations(
    annotations: &Option<HashMap<String, String>>,
) -> Result<PreserveKeys> {
    match annotations {
        None => Ok(PreserveKeys::new()),
        Some(map) => match map.get(PRESERVE_KEYS_ANNOTATION) {
            None => Ok(PreserveKeys::new()),
            Some(value) => decode(value),
        },
    }
}

/// Insert (or replace) the `bundle.preserve-keys` annotation in a mutable
/// annotation map, encoding `keys` as JSON.
///
/// If `keys` is empty the annotation is removed (absent annotation is
/// equivalent to an empty map).
pub fn set_in_annotations(
    annotations: &mut HashMap<String, String>,
    keys: &PreserveKeys,
) -> Result<()> {
    if keys.is_empty() {
        annotations.remove(PRESERVE_KEYS_ANNOTATION);
    } else {
        let encoded = encode(keys)?;
        annotations.insert(PRESERVE_KEYS_ANNOTATION.to_string(), encoded);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_keys() -> PreserveKeys {
        let mut m = PreserveKeys::new();
        m.insert(
            "plugins/Essentials/config.yml".to_string(),
            vec![
                "home.bed-respawn".to_string(),
                "homes.max-homes".to_string(),
            ],
        );
        m.insert(
            "plugins/LuckPerms/config.yml".to_string(),
            vec!["storage-method".to_string()],
        );
        m
    }

    #[test]
    fn round_trip_non_empty() {
        let keys = sample_keys();
        let encoded = encode(&keys).unwrap();
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded, keys);
    }

    #[test]
    fn round_trip_empty() {
        let keys = PreserveKeys::new();
        let encoded = encode(&keys).unwrap();
        let decoded = decode(&encoded).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn decode_empty_string_gives_empty_map() {
        let decoded = decode("").unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn decode_whitespace_string_gives_empty_map() {
        let decoded = decode("   \t\n").unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn decode_invalid_json_is_error() {
        assert!(decode("not-json").is_err());
    }

    #[test]
    fn decode_array_instead_of_object_is_error() {
        assert!(decode("[1,2,3]").is_err());
    }

    #[test]
    fn encode_is_deterministic() {
        // Run twice; output should be identical.
        let keys = sample_keys();
        let a = encode(&keys).unwrap();
        let b = encode(&keys).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn encode_is_sorted_by_key() {
        // Even when inserted in reverse order, encoded JSON should be sorted.
        let mut keys = PreserveKeys::new();
        keys.insert("z_path".to_string(), vec!["z".to_string()]);
        keys.insert("a_path".to_string(), vec!["a".to_string()]);
        keys.insert("m_path".to_string(), vec!["m".to_string()]);

        let encoded = encode(&keys).unwrap();
        let a_pos = encoded.find("a_path").unwrap();
        let m_pos = encoded.find("m_path").unwrap();
        let z_pos = encoded.find("z_path").unwrap();

        assert!(a_pos < m_pos, "a_path should come before m_path");
        assert!(m_pos < z_pos, "m_path should come before z_path");
    }

    #[test]
    fn merge_disjoint_paths_are_unioned() {
        let mut base = PreserveKeys::new();
        base.insert(
            "plugins/A/config.yml".to_string(),
            vec!["key.a".to_string()],
        );

        let mut overrides = PreserveKeys::new();
        overrides.insert(
            "plugins/B/config.yml".to_string(),
            vec!["key.b".to_string()],
        );

        let merged = merge(base, overrides);

        assert_eq!(merged.len(), 2);
        assert!(merged.contains_key("plugins/A/config.yml"));
        assert!(merged.contains_key("plugins/B/config.yml"));
    }

    #[test]
    fn merge_same_path_override_wins() {
        let mut base = PreserveKeys::new();
        base.insert(
            "plugins/A/config.yml".to_string(),
            vec!["key.old".to_string()],
        );

        let mut overrides = PreserveKeys::new();
        overrides.insert(
            "plugins/A/config.yml".to_string(),
            vec!["key.new".to_string()],
        );

        let merged = merge(base, overrides);

        assert_eq!(merged["plugins/A/config.yml"], vec!["key.new"]);
    }

    #[test]
    fn merge_empty_override_preserves_base() {
        let mut base = PreserveKeys::new();
        base.insert("plugins/A/config.yml".to_string(), vec!["k".to_string()]);

        let merged = merge(base.clone(), PreserveKeys::new());
        assert_eq!(merged, base);
    }

    #[test]
    fn merge_empty_base_is_override() {
        let mut overrides = PreserveKeys::new();
        overrides.insert("plugins/A/config.yml".to_string(), vec!["k".to_string()]);

        let merged = merge(PreserveKeys::new(), overrides.clone());
        assert_eq!(merged, overrides);
    }

    #[test]
    fn merge_three_stages_last_wins() {
        let mut stage1 = PreserveKeys::new();
        stage1.insert("plugins/A/config.yml".to_string(), vec!["s1".to_string()]);

        let mut stage2 = PreserveKeys::new();
        stage2.insert("plugins/A/config.yml".to_string(), vec!["s2".to_string()]);

        let mut stage3 = PreserveKeys::new();
        stage3.insert("plugins/A/config.yml".to_string(), vec!["s3".to_string()]);

        let merged = merge(merge(stage1, stage2), stage3);
        assert_eq!(merged["plugins/A/config.yml"], vec!["s3"]);
    }

    #[test]
    fn from_manifest_annotations_none() {
        let result = from_manifest_annotations(&None).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn from_manifest_annotations_missing_key() {
        let mut map = HashMap::new();
        map.insert("other.annotation".to_string(), "value".to_string());
        let result = from_manifest_annotations(&Some(map)).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn from_manifest_annotations_present() {
        let mut keys = PreserveKeys::new();
        keys.insert(
            "plugins/Test/config.yml".to_string(),
            vec!["test.key".to_string()],
        );

        let mut map = HashMap::new();
        map.insert(PRESERVE_KEYS_ANNOTATION.to_string(), encode(&keys).unwrap());

        let result = from_manifest_annotations(&Some(map)).unwrap();
        assert_eq!(result, keys);
    }

    #[test]
    fn set_in_annotations_inserts_encoded() {
        let mut map = HashMap::new();
        let mut keys = PreserveKeys::new();
        keys.insert("plugins/X/config.yml".to_string(), vec!["x".to_string()]);

        set_in_annotations(&mut map, &keys).unwrap();

        assert!(map.contains_key(PRESERVE_KEYS_ANNOTATION));
        let decoded = decode(map.get(PRESERVE_KEYS_ANNOTATION).unwrap()).unwrap();
        assert_eq!(decoded, keys);
    }

    #[test]
    fn set_in_annotations_removes_when_empty() {
        let mut map = HashMap::new();
        map.insert(PRESERVE_KEYS_ANNOTATION.to_string(), "{}".to_string());

        set_in_annotations(&mut map, &PreserveKeys::new()).unwrap();

        assert!(!map.contains_key(PRESERVE_KEYS_ANNOTATION));
    }

    #[test]
    fn from_preserve_directives_basic() {
        use crate::bundlefile::types::PreserveDirective;

        let directives = vec![
            PreserveDirective {
                config_path: "plugins/A/config.yml".to_string(),
                keys: vec!["k1".to_string(), "k2".to_string()],
            },
            PreserveDirective {
                config_path: "plugins/B/config.yml".to_string(),
                keys: vec!["kb".to_string()],
            },
        ];

        let result = from_preserve_directives(&directives);
        assert_eq!(result["plugins/A/config.yml"], vec!["k1", "k2"]);
        assert_eq!(result["plugins/B/config.yml"], vec!["kb"]);
    }

    #[test]
    fn from_preserve_directives_empty() {
        let result = from_preserve_directives(&[]);
        assert!(result.is_empty());
    }
}
