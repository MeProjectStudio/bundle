//! Encoding, decoding, and merging of the `bundle.managed-keys` OCI annotation.
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
//! a last-writer-wins strategy per config path: if stage N declares a `MANAGE`
//! for `plugins/A/config.yml` with keys `[k1]`, and stage N+1 declares the
//! same path with keys `[k2]`, the final annotation contains only `[k2]` for
//! that path.

use std::collections::HashMap;

use anyhow::{Context, Result};

/// The OCI manifest annotation key under which mcpm stores managed-keys info.
pub const MANAGED_KEYS_ANNOTATION: &str = "bundle.managed-keys";

/// A map from server-root-relative config file path to the list of
/// dot-separated key paths that this bundle owns.
///
/// Example:
/// ```
/// "plugins/Essentials/config.yml" → ["home.bed-respawn", "homes.max-homes"]
/// ```
pub type ManagedKeys = HashMap<String, Vec<String>>;

/// Encode a `ManagedKeys` map to a compact JSON string suitable for storing as
/// an OCI manifest annotation value.
///
/// Keys are sorted for deterministic output (important for content-addressable
/// manifests).
pub fn encode(keys: &ManagedKeys) -> Result<String> {
    // Sort config-path keys so the output is deterministic.
    let sorted: std::collections::BTreeMap<&str, &Vec<String>> =
        keys.iter().map(|(k, v)| (k.as_str(), v)).collect();

    serde_json::to_string(&sorted).context("encoding bundle.managed-keys annotation to JSON")
}

/// Decode a JSON string (previously produced by [`encode`]) back into a
/// `ManagedKeys` map.
///
/// Returns an empty map for an empty or whitespace-only string so that callers
/// do not need to special-case missing annotations.
pub fn decode(s: &str) -> Result<ManagedKeys> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(ManagedKeys::new());
    }
    serde_json::from_str(trimmed)
        .with_context(|| format!("decoding bundle.managed-keys annotation: {:?}", trimmed))
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
pub fn merge(mut base: ManagedKeys, overrides: ManagedKeys) -> ManagedKeys {
    for (path, keys) in overrides {
        base.insert(path, keys);
    }
    base
}

/// Build a `ManagedKeys` map from the `MANAGE` directives of a single stage.
///
/// This is a convenience wrapper used in `bundle/build.rs`.
pub fn from_manage_directives(
    directives: &[crate::bundlefile::types::ManageDirective],
) -> ManagedKeys {
    let mut map = ManagedKeys::new();
    for directive in directives {
        map.insert(directive.config_path.clone(), directive.keys.clone());
    }
    map
}

/// Extract the `ManagedKeys` annotation from an OCI manifest's annotation map.
///
/// Returns an empty map if the annotation is absent.
pub fn from_manifest_annotations(
    annotations: &Option<HashMap<String, String>>,
) -> Result<ManagedKeys> {
    match annotations {
        None => Ok(ManagedKeys::new()),
        Some(map) => match map.get(MANAGED_KEYS_ANNOTATION) {
            None => Ok(ManagedKeys::new()),
            Some(value) => decode(value),
        },
    }
}

/// Insert (or replace) the `bundle.managed-keys` annotation in a mutable
/// annotation map, encoding `keys` as JSON.
///
/// If `keys` is empty the annotation is removed (absent annotation is
/// equivalent to an empty map).
pub fn set_in_annotations(
    annotations: &mut HashMap<String, String>,
    keys: &ManagedKeys,
) -> Result<()> {
    if keys.is_empty() {
        annotations.remove(MANAGED_KEYS_ANNOTATION);
    } else {
        let encoded = encode(keys)?;
        annotations.insert(MANAGED_KEYS_ANNOTATION.to_string(), encoded);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_keys() -> ManagedKeys {
        let mut m = ManagedKeys::new();
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
        let keys = ManagedKeys::new();
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
        let mut keys = ManagedKeys::new();
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
        let mut base = ManagedKeys::new();
        base.insert(
            "plugins/A/config.yml".to_string(),
            vec!["key.a".to_string()],
        );

        let mut overrides = ManagedKeys::new();
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
        let mut base = ManagedKeys::new();
        base.insert(
            "plugins/A/config.yml".to_string(),
            vec!["key.old".to_string()],
        );

        let mut overrides = ManagedKeys::new();
        overrides.insert(
            "plugins/A/config.yml".to_string(),
            vec!["key.new".to_string()],
        );

        let merged = merge(base, overrides);

        assert_eq!(merged["plugins/A/config.yml"], vec!["key.new"]);
    }

    #[test]
    fn merge_empty_override_preserves_base() {
        let mut base = ManagedKeys::new();
        base.insert("plugins/A/config.yml".to_string(), vec!["k".to_string()]);

        let merged = merge(base.clone(), ManagedKeys::new());
        assert_eq!(merged, base);
    }

    #[test]
    fn merge_empty_base_is_override() {
        let mut overrides = ManagedKeys::new();
        overrides.insert("plugins/A/config.yml".to_string(), vec!["k".to_string()]);

        let merged = merge(ManagedKeys::new(), overrides.clone());
        assert_eq!(merged, overrides);
    }

    #[test]
    fn merge_three_stages_last_wins() {
        let mut stage1 = ManagedKeys::new();
        stage1.insert("plugins/A/config.yml".to_string(), vec!["s1".to_string()]);

        let mut stage2 = ManagedKeys::new();
        stage2.insert("plugins/A/config.yml".to_string(), vec!["s2".to_string()]);

        let mut stage3 = ManagedKeys::new();
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
        let mut keys = ManagedKeys::new();
        keys.insert(
            "plugins/Test/config.yml".to_string(),
            vec!["test.key".to_string()],
        );

        let mut map = HashMap::new();
        map.insert(MANAGED_KEYS_ANNOTATION.to_string(), encode(&keys).unwrap());

        let result = from_manifest_annotations(&Some(map)).unwrap();
        assert_eq!(result, keys);
    }

    #[test]
    fn set_in_annotations_inserts_encoded() {
        let mut map = HashMap::new();
        let mut keys = ManagedKeys::new();
        keys.insert("plugins/X/config.yml".to_string(), vec!["x".to_string()]);

        set_in_annotations(&mut map, &keys).unwrap();

        assert!(map.contains_key(MANAGED_KEYS_ANNOTATION));
        let decoded = decode(map.get(MANAGED_KEYS_ANNOTATION).unwrap()).unwrap();
        assert_eq!(decoded, keys);
    }

    #[test]
    fn set_in_annotations_removes_when_empty() {
        let mut map = HashMap::new();
        map.insert(MANAGED_KEYS_ANNOTATION.to_string(), "{}".to_string());

        set_in_annotations(&mut map, &ManagedKeys::new()).unwrap();

        assert!(!map.contains_key(MANAGED_KEYS_ANNOTATION));
    }

    #[test]
    fn from_manage_directives_basic() {
        use crate::bundlefile::types::ManageDirective;

        let directives = vec![
            ManageDirective {
                config_path: "plugins/A/config.yml".to_string(),
                keys: vec!["k1".to_string(), "k2".to_string()],
            },
            ManageDirective {
                config_path: "plugins/B/config.yml".to_string(),
                keys: vec!["kb".to_string()],
            },
        ];

        let result = from_manage_directives(&directives);
        assert_eq!(result["plugins/A/config.yml"], vec!["k1", "k2"]);
        assert_eq!(result["plugins/B/config.yml"], vec!["kb"]);
    }

    #[test]
    fn from_manage_directives_empty() {
        let result = from_manage_directives(&[]);
        assert!(result.is_empty());
    }
}
