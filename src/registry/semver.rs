//! Semver-aware OCI tag resolution.
//!
//! OCI registries store images under plain string tags (`v2.20.1`, `latest`,
//! …).  This module adds a layer on top that lets `bundle.toml` bundle references
//! use **semver range expressions** as the tag portion of an image reference:
//!
//! ```toml
//! [bundles]
//! essentials = "ghcr.io/someauthor/essentials:2.4"    # latest 2.4.x
//! luckperms  = "ghcr.io/luckperms/luckperms:^5"       # latest 5.x.x
//! sodium     = "ghcr.io/jellysquid/sodium:~0.5.8"     # latest 0.5.x ≥ 0.5.8
//! ```
//!
//! ## Range syntax
//!
//! This crate delegates to the [`semver`] crate (Cargo's flavour).  The
//! following tag strings are treated as **version ranges** and trigger tag
//! listing + resolution:
//!
//! | Tag string     | Meaning                                    |
//! |----------------|--------------------------------------------|
//! | `2`            | `>=2.0.0, <3.0.0`  — latest major          |
//! | `2.4`          | `>=2.4.0, <2.5.0`  — latest patch in 2.4  |
//! | `^2.4`         | `>=2.4.0, <3.0.0`  — semver-compatible     |
//! | `~2.4`         | `>=2.4.0, <2.5.0`  — same minor            |
//! | `~2.4.1`       | `>=2.4.1, <2.5.0`                          |
//! | `>=2.4, <2.6`  | explicit compound range                    |
//! | `*`            | latest stable (any version)                |
//!
//! A tag that looks like a fully-specified three-component version
//! (`2.4.5`, `v2.20.1`) **is not** treated as a range — it is used verbatim
//! as a literal OCI tag, so no network listing is required.
//!
//! ## Tag stripping
//!
//! Registry tags often carry a `v` prefix (`v2.20.1`).  When scanning tags
//! for matches the leading `v` is stripped before parsing with `semver`.
//! When the resolved tag is returned it retains whatever prefix the registry
//! originally used.
//!
//! ## Pre-release versions
//!
//! Pre-release tags (`2.4.5-alpha.1`, `v3.0.0-rc.2`) are **skipped by
//! default** unless the range expression itself includes a pre-release
//! comparator.

use anyhow::{bail, Context, Result};
use semver::{Version, VersionReq};


/// Inspect the tag portion of `image_ref` and decide whether it is a semver
/// range expression that requires tag listing + resolution.
///
/// Returns `false` for exact three-component versions (`2.4.5`, `v2.20.1`) and
/// for non-version strings (`latest`, `main`, `sha-abc123`).
pub fn is_range(image_ref: &str) -> bool {
    match tag_of(image_ref) {
        Some(tag) => tag_is_range(tag),
        None => false,
    }
}

/// Given a list of tags (as returned by the registry) and a range expression
/// `range_tag` (the tag portion of the image reference), return the tag string
/// from `candidates` that best satisfies the range.
///
/// "Best" means the **highest matching stable version**.  Pre-release versions
/// are excluded unless the range expression itself contains a pre-release
/// comparator.
///
/// Returns an error if:
/// - `range_tag` cannot be parsed as a [`VersionReq`].
/// - No candidate tag matches the range.
pub fn resolve(range_tag: &str, candidates: &[String]) -> Result<String> {
    let req = build_req(range_tag)
        .with_context(|| format!("building semver requirement from {:?}", range_tag))?;

    let want_pre = req_mentions_prerelease(&req);

    // Collect all tags that parse as semver versions satisfying the requirement.
    let mut matching: Vec<(Version, &str)> = candidates
        .iter()
        .filter_map(|tag| {
            let ver_str = tag.strip_prefix('v').unwrap_or(tag.as_str());
            let ver = Version::parse(ver_str).ok()?;

            // Skip pre-release unless the range explicitly mentions one.
            if !ver.pre.is_empty() && !want_pre {
                return None;
            }

            if req.matches(&ver) {
                Some((ver, tag.as_str()))
            } else {
                None
            }
        })
        .collect();

    if matching.is_empty() {
        // Build a helpful message listing the tags that were checked.
        let mut samples: Vec<&str> = candidates.iter().map(String::as_str).collect();
        samples.sort_unstable();
        let preview = if samples.len() > 20 {
            format!("{} … ({} total)", samples[..20].join(", "), samples.len())
        } else {
            samples.join(", ")
        };
        bail!(
            "no tag satisfies semver range {:?}\navailable tags: {}",
            range_tag,
            if preview.is_empty() {
                "(none)".to_string()
            } else {
                preview
            }
        );
    }

    // Sort descending by version; the first element is the highest match.
    matching.sort_by(|(a, _), (b, _)| b.cmp(a));
    Ok(matching[0].1.to_string())
}

/// Rewrite `image_ref` by replacing its tag with `resolved_tag`.
///
/// The registry and repository portions are preserved verbatim.
///
/// ```
/// # use mcpm::registry::semver::rewrite_tag;   // (doctest only)
/// assert_eq!(
///     rewrite_tag("ghcr.io/author/essentials:2.4", "v2.4.5"),
///     "ghcr.io/author/essentials:v2.4.5",
/// );
/// ```
pub fn rewrite_tag(image_ref: &str, resolved_tag: &str) -> String {
    // Split on the last `:` that is not inside a port spec (i.e. after the
    // first `/`).  The simplest approach: find the last `:` that follows the
    // first `/`.
    if let Some(slash_pos) = image_ref.find('/') {
        if let Some(colon_pos) = image_ref[slash_pos..].rfind(':') {
            let split = slash_pos + colon_pos;
            return format!("{}:{}", &image_ref[..split], resolved_tag);
        }
    }
    // No colon after the first slash → append the tag.
    format!("{}:{}", image_ref, resolved_tag)
}


/// Extract the tag portion of an image reference string.
///
/// Returns `None` for digest references (`repo@sha256:…`) and for references
/// with no colon after the first `/`.
pub fn tag_of(image_ref: &str) -> Option<&str> {
    // Digest references are never ranges.
    if image_ref.contains('@') {
        return None;
    }

    // Find the colon that separates repo from tag (must come after the first
    // `/` so we don't confuse the `host:port` part of the registry).
    let search_from = image_ref.find('/').unwrap_or(0);
    let colon = image_ref[search_from..].rfind(':')?;
    Some(&image_ref[search_from + colon + 1..])
}

/// Return `true` if `tag` should be treated as a semver range expression.
fn tag_is_range(tag: &str) -> bool {
    let t = tag.strip_prefix('v').unwrap_or(tag);

    // Explicit operator prefix.
    if t.starts_with('^')
        || t.starts_with('~')
        || t.starts_with('>')
        || t.starts_with('<')
        || t.starts_with('=')
    {
        return true;
    }

    // Wildcard or compound range.
    if t.contains('*') || t.contains(',') || t.contains("||") {
        return true;
    }

    // Partial version: only 1 or 2 purely-numeric dot-separated components.
    // "2"   → 1 component → range
    // "2.4" → 2 components → range
    // "2.4.5" → 3 components → NOT a range (exact version)
    let parts: Vec<&str> = t.split('.').collect();
    if parts.len() < 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    {
        return true;
    }

    false
}

/// Build a [`VersionReq`] from a tag string (which may or may not carry a `v`
/// prefix).
///
/// Partial versions are expanded before parsing:
///
/// | Input | Expanded to         |
/// |-------|---------------------|
/// | `2`   | `>=2.0.0, <3.0.0`  |
/// | `2.4` | `>=2.4.0, <2.5.0`  |
///
/// Strings with explicit operators (`^2.4`, `~2.4.1`, `>=2, <3`) are passed
/// straight to [`VersionReq::parse`].
pub fn build_req(tag: &str) -> Result<VersionReq> {
    let t = tag.strip_prefix('v').unwrap_or(tag);

    // If it already has an explicit operator or is a compound range, parse
    // directly.
    let has_operator = t.starts_with('^')
        || t.starts_with('~')
        || t.starts_with('>')
        || t.starts_with('<')
        || t.starts_with('=')
        || t.contains(',')
        || t.contains("||")
        || t.contains('*');

    if has_operator {
        return VersionReq::parse(t)
            .with_context(|| format!("invalid semver range expression: {:?}", tag));
    }

    // Partial numeric version — expand to a tighter range so that `2.4`
    // means "latest 2.4.x" rather than "anything >=2.4.0".
    let parts: Vec<&str> = t.split('.').collect();

    let req_str = match parts.as_slice() {
        [major] => {
            // "2" → ">=2.0.0, <3.0.0"
            let maj: u64 = major
                .parse()
                .with_context(|| format!("invalid major version in {:?}", tag))?;
            format!(">={}.0.0, <{}.0.0", maj, maj + 1)
        }
        [major, minor] => {
            // "2.4" → ">=2.4.0, <2.5.0"
            let maj: u64 = major
                .parse()
                .with_context(|| format!("invalid major version in {:?}", tag))?;
            let min: u64 = minor
                .parse()
                .with_context(|| format!("invalid minor version in {:?}", tag))?;
            format!(">={}.{}.0, <{}.{}.0", maj, min, maj, min + 1)
        }
        _ => {
            // Should not be reached for valid partial versions, but fall back
            // to a direct parse so we at least give a reasonable error.
            return VersionReq::parse(t)
                .with_context(|| format!("invalid semver range expression: {:?}", tag));
        }
    };

    VersionReq::parse(&req_str)
        .with_context(|| format!("expanded {:?} to {:?} but it failed to parse", tag, req_str))
}

/// Return `true` if any comparator in `req` carries a pre-release identifier.
///
/// When this is true, pre-release tags are included in the candidate set so
/// that ranges like `>=2.4.0-beta, <2.5.0` work correctly.
fn req_mentions_prerelease(req: &VersionReq) -> bool {
    // The semver crate exposes comparators via Debug but not via a public
    // field iterator in all versions.  We use the Display representation as a
    // reliable proxy: if the string contains `-` after a digit it mentions a
    // pre-release.
    let s = req.to_string();
    // Look for the pattern <digit>-<non-digit> which indicates a pre-release.
    let bytes = s.as_bytes();
    for i in 1..bytes.len().saturating_sub(1) {
        if bytes[i] == b'-' && bytes[i - 1].is_ascii_digit() {
            return true;
        }
    }
    false
}


#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn exact_three_component_is_not_a_range() {
        assert!(!is_range("ghcr.io/author/img:2.4.5"));
        assert!(!is_range("ghcr.io/author/img:v2.20.1"));
        assert!(!is_range("ghcr.io/author/img:1.0.0"));
    }

    #[test]
    fn two_component_is_a_range() {
        assert!(is_range("ghcr.io/author/img:2.4"));
        assert!(is_range("ghcr.io/author/img:v2.4"));
    }

    #[test]
    fn one_component_is_a_range() {
        assert!(is_range("ghcr.io/author/img:2"));
        assert!(is_range("ghcr.io/author/img:v5"));
    }

    #[test]
    fn caret_is_a_range() {
        assert!(is_range("ghcr.io/author/img:^2.4"));
        assert!(is_range("ghcr.io/author/img:^2.4.1"));
    }

    #[test]
    fn tilde_is_a_range() {
        assert!(is_range("ghcr.io/author/img:~2.4"));
        assert!(is_range("ghcr.io/author/img:~2.4.1"));
    }

    #[test]
    fn explicit_comparator_is_a_range() {
        assert!(is_range("ghcr.io/author/img:>=2.4.0"));
        assert!(is_range("ghcr.io/author/img:>=2.4, <2.5"));
    }

    #[test]
    fn wildcard_is_a_range() {
        assert!(is_range("ghcr.io/author/img:*"));
    }

    #[test]
    fn latest_tag_is_not_a_range() {
        assert!(!is_range("ghcr.io/author/img:latest"));
    }

    #[test]
    fn digest_ref_is_not_a_range() {
        assert!(!is_range("ghcr.io/author/img@sha256:abc123"));
    }

    #[test]
    fn no_tag_returns_false() {
        assert!(!is_range("ghcr.io/author/img"));
    }


    #[test]
    fn partial_one_component_expands_to_major_range() {
        let req = build_req("2").unwrap();
        // Should match 2.x.x but not 3.x.x or 1.x.x
        assert!(req.matches(&Version::parse("2.0.0").unwrap()));
        assert!(req.matches(&Version::parse("2.99.99").unwrap()));
        assert!(!req.matches(&Version::parse("3.0.0").unwrap()));
        assert!(!req.matches(&Version::parse("1.9.9").unwrap()));
    }

    #[test]
    fn partial_two_component_expands_to_minor_range() {
        let req = build_req("2.4").unwrap();
        // Should match 2.4.x but not 2.5.x or 2.3.x
        assert!(req.matches(&Version::parse("2.4.0").unwrap()));
        assert!(req.matches(&Version::parse("2.4.5").unwrap()));
        assert!(req.matches(&Version::parse("2.4.99").unwrap()));
        assert!(!req.matches(&Version::parse("2.5.0").unwrap()));
        assert!(!req.matches(&Version::parse("2.3.9").unwrap()));
        assert!(!req.matches(&Version::parse("3.0.0").unwrap()));
    }

    #[test]
    fn partial_with_v_prefix_stripped() {
        let req = build_req("v2.4").unwrap();
        assert!(req.matches(&Version::parse("2.4.5").unwrap()));
        assert!(!req.matches(&Version::parse("2.5.0").unwrap()));
    }

    #[test]
    fn caret_range_parsed_directly() {
        let req = build_req("^2.4").unwrap();
        // ^2.4 → >=2.4.0, <3.0.0
        assert!(req.matches(&Version::parse("2.4.0").unwrap()));
        assert!(req.matches(&Version::parse("2.99.0").unwrap()));
        assert!(!req.matches(&Version::parse("3.0.0").unwrap()));
    }

    #[test]
    fn tilde_range_parsed_directly() {
        let req = build_req("~2.4").unwrap();
        // ~2.4 → >=2.4.0, <2.5.0
        assert!(req.matches(&Version::parse("2.4.0").unwrap()));
        assert!(req.matches(&Version::parse("2.4.9").unwrap()));
        assert!(!req.matches(&Version::parse("2.5.0").unwrap()));
    }

    #[test]
    fn tilde_with_patch_range_parsed_directly() {
        let req = build_req("~2.4.1").unwrap();
        // ~2.4.1 → >=2.4.1, <2.5.0
        assert!(req.matches(&Version::parse("2.4.1").unwrap()));
        assert!(req.matches(&Version::parse("2.4.9").unwrap()));
        assert!(!req.matches(&Version::parse("2.4.0").unwrap()));
        assert!(!req.matches(&Version::parse("2.5.0").unwrap()));
    }

    #[test]
    fn compound_range_parsed_directly() {
        let req = build_req(">=2.4.0, <2.6.0").unwrap();
        assert!(req.matches(&Version::parse("2.4.0").unwrap()));
        assert!(req.matches(&Version::parse("2.5.9").unwrap()));
        assert!(!req.matches(&Version::parse("2.6.0").unwrap()));
        assert!(!req.matches(&Version::parse("2.3.9").unwrap()));
    }

    #[test]
    fn wildcard_matches_any_stable() {
        let req = build_req("*").unwrap();
        assert!(req.matches(&Version::parse("1.0.0").unwrap()));
        assert!(req.matches(&Version::parse("99.0.0").unwrap()));
    }


    fn tags(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn resolve_picks_highest_matching_patch() {
        let candidates = tags(&["v2.4.0", "v2.4.1", "v2.4.5", "v2.5.0", "v3.0.0"]);
        // "2.4" should resolve to the highest 2.4.x
        let got = resolve("2.4", &candidates).unwrap();
        assert_eq!(got, "v2.4.5");
    }

    #[test]
    fn resolve_with_v_prefix_in_range() {
        let candidates = tags(&["v2.4.0", "v2.4.3", "v2.5.0"]);
        let got = resolve("v2.4", &candidates).unwrap();
        assert_eq!(got, "v2.4.3");
    }

    #[test]
    fn resolve_one_component_picks_highest_minor() {
        let candidates = tags(&["2.0.0", "2.3.1", "2.9.5", "3.0.0", "1.99.0"]);
        // "2" → latest 2.x.x
        let got = resolve("2", &candidates).unwrap();
        assert_eq!(got, "2.9.5");
    }

    #[test]
    fn resolve_caret_picks_across_minor() {
        let candidates = tags(&["v2.4.0", "v2.7.3", "v2.9.0", "v3.0.0"]);
        // "^2.4" → >=2.4.0, <3.0.0 → highest is v2.9.0
        let got = resolve("^2.4", &candidates).unwrap();
        assert_eq!(got, "v2.9.0");
    }

    #[test]
    fn resolve_tilde_stays_within_minor() {
        let candidates = tags(&["v2.4.0", "v2.4.8", "v2.5.0", "v2.9.0"]);
        // "~2.4" → >=2.4.0, <2.5.0 → highest is v2.4.8
        let got = resolve("~2.4", &candidates).unwrap();
        assert_eq!(got, "v2.4.8");
    }

    #[test]
    fn resolve_skips_prerelease_by_default() {
        let candidates = tags(&["v2.4.0", "v2.4.5-alpha.1", "v2.4.4"]);
        // Pre-release should be skipped
        let got = resolve("2.4", &candidates).unwrap();
        assert_eq!(got, "v2.4.4");
    }

    #[test]
    fn resolve_no_candidates_is_error() {
        let candidates = tags(&["v1.0.0", "v3.0.0"]);
        let err = resolve("2.4", &candidates).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("no tag satisfies"),
            "unexpected error: {}",
            msg
        );
        assert!(msg.contains("2.4"), "should mention the range: {}", msg);
    }

    #[test]
    fn resolve_empty_candidates_is_error() {
        let err = resolve("2.4", &[]).unwrap_err();
        assert!(err.to_string().contains("no tag satisfies"));
    }

    #[test]
    fn resolve_non_semver_tags_are_skipped_gracefully() {
        // Mix of valid semver and non-semver tags; only valid ones are
        // considered for matching.
        let candidates = tags(&["latest", "main", "v2.4.3", "nightly"]);
        let got = resolve("2.4", &candidates).unwrap();
        assert_eq!(got, "v2.4.3");
    }

    #[test]
    fn resolve_tags_without_v_prefix() {
        let candidates = tags(&["2.4.0", "2.4.1", "2.4.5"]);
        let got = resolve("2.4", &candidates).unwrap();
        assert_eq!(got, "2.4.5");
    }

    #[test]
    fn resolve_wildcard_picks_highest_stable() {
        let candidates = tags(&["1.0.0", "2.0.0", "0.9.0", "2.0.0-rc.1"]);
        let got = resolve("*", &candidates).unwrap();
        assert_eq!(got, "2.0.0");
    }


    #[test]
    fn rewrite_tag_replaces_existing_tag() {
        assert_eq!(
            rewrite_tag("ghcr.io/author/essentials:2.4", "v2.4.5"),
            "ghcr.io/author/essentials:v2.4.5"
        );
    }

    #[test]
    fn rewrite_tag_with_port_in_registry() {
        // The `:5000` must not be confused with a tag separator.
        assert_eq!(
            rewrite_tag("localhost:5000/myimage:2.4", "v2.4.5"),
            "localhost:5000/myimage:v2.4.5"
        );
    }

    #[test]
    fn rewrite_tag_no_existing_tag_appends() {
        assert_eq!(
            rewrite_tag("ghcr.io/author/image", "v1.0.0"),
            "ghcr.io/author/image:v1.0.0"
        );
    }

    #[test]
    fn rewrite_tag_caret_range_replaced() {
        assert_eq!(
            rewrite_tag("ghcr.io/author/img:^2.4", "v2.9.0"),
            "ghcr.io/author/img:v2.9.0"
        );
    }


    #[test]
    fn tag_of_standard_ref() {
        assert_eq!(tag_of("ghcr.io/author/img:v2.4.5"), Some("v2.4.5"));
    }

    #[test]
    fn tag_of_no_tag() {
        assert_eq!(tag_of("ghcr.io/author/img"), None);
    }

    #[test]
    fn tag_of_digest_ref() {
        assert_eq!(tag_of("ghcr.io/author/img@sha256:abc"), None);
    }

    #[test]
    fn tag_of_registry_with_port() {
        assert_eq!(tag_of("localhost:5000/img:latest"), Some("latest"));
    }
}
