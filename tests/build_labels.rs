//! Integration tests: LABEL directives with various value forms must survive
//! the full Bundlefile → build → OCI image config round-trip intact.
//!
//! Tests here cover the parser fix for unquoted multi-word label values:
//! before the fix, `LABEL desc=hello world` was split into two labels
//! (`desc = "hello"`, `world = ""`).  These tests pin the correct behaviour
//! for every form that appears in real CI workflows.

mod common;

use std::collections::HashMap;

use bundle::bundle::build::build;
use bundle::registry::types::ImageConfiguration;
use tempfile::TempDir;

// ── Shared helpers ────────────────────────────────────────────────────────────

/// Build the Bundlefile at `ctx/Bundlefile` with optional `--build-arg`
/// overrides and return the parsed OCI `ImageConfiguration`.
async fn build_config(ctx: &TempDir, overrides: HashMap<String, String>) -> ImageConfiguration {
    let image = build(&ctx.path().join("Bundlefile"), &overrides)
        .await
        .unwrap_or_else(|e| panic!("build failed: {e:#}"));
    ImageConfiguration::from_reader(image.config_data.as_slice()).expect("parse image config")
}

/// Extract a cloned copy of the label map from an `ImageConfiguration`.
/// Returns an empty map when no labels are present.
///
/// `Config::labels()` in oci-spec returns `&Option<HashMap<…>>`, so we use
/// match ergonomics to bind through the reference rather than `.and_then`.
fn labels(config: &ImageConfiguration) -> HashMap<String, String> {
    let Some(cfg) = config.config() else {
        return HashMap::new();
    };
    // cfg.labels() → &Option<HashMap<String,String>>; match ergonomics give
    // us map: &HashMap<String,String> via auto-deref of the &Option.
    let Some(map) = cfg.labels() else {
        return HashMap::new();
    };
    map.clone()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Unquoted multi-word value must be stored as a **single** label — not split
/// into individual words.
///
/// Regression: the old parser stopped at the first whitespace, so
/// `LABEL desc=hello world` produced `{desc: "hello", world: ""}`.
#[tokio::test]
async fn label_unquoted_multiword_value_preserved() {
    let ctx = TempDir::new().unwrap();
    common::bundlefile(
        ctx.path(),
        "FROM scratch\nLABEL description=hello world foo bar\n",
    );

    let labels = labels(&build_config(&ctx, HashMap::new()).await);

    assert_eq!(
        labels.get("description").map(String::as_str),
        Some("hello world foo bar"),
        "unquoted multi-word value must be a single label, not split on spaces"
    );
    for word in ["hello", "world", "foo", "bar"] {
        assert!(
            !labels.contains_key(word),
            "'{word}' must not appear as a standalone label key"
        );
    }
}

/// Quoted values that contain spaces are preserved byte-for-byte.
#[tokio::test]
async fn label_quoted_value_with_spaces_preserved() {
    let ctx = TempDir::new().unwrap();
    common::bundlefile(
        ctx.path(),
        concat!(
            "FROM scratch\n",
            "LABEL org.opencontainers.image.description=\"mcmetrics-exporter for Velocity.",
            " For more info refer to https://github.com/realkarmakun/mcmetrics-exporter\"\n",
        ),
    );

    let labels = labels(&build_config(&ctx, HashMap::new()).await);

    assert_eq!(
        labels
            .get("org.opencontainers.image.description")
            .map(String::as_str),
        Some("mcmetrics-exporter for Velocity. For more info refer to https://github.com/realkarmakun/mcmetrics-exporter"),
    );
}

/// ARG substitution into an unquoted LABEL value — the motivating real-world
/// case from the CI workflow where DESCRIPTION contained a full sentence.
///
/// The Bundlefile has `LABEL …description=${DESCRIPTION}` and the value is
/// supplied via `--build-arg`.  After substitution the label line looks like:
///
///   LABEL org.opencontainers.image.description=mcmetrics-exporter for Velocity…
///
/// The whole sentence must end up as one label value.
#[tokio::test]
async fn label_arg_substituted_multiword_description() {
    let ctx = TempDir::new().unwrap();
    common::bundlefile(
        ctx.path(),
        concat!(
            "ARG DESCRIPTION\n",
            "FROM scratch\n",
            "LABEL org.opencontainers.image.description=${DESCRIPTION}\n",
        ),
    );

    let description = concat!(
        "mcmetrics-exporter for Velocity. ",
        "This image was created using bundle declarative mod management system, ",
        "and it's purpose is to be used with bundle to manage mcmetrics-exporter installation.",
    );

    let overrides = HashMap::from([("DESCRIPTION".to_string(), description.to_string())]);
    let labels = labels(&build_config(&ctx, overrides).await);

    assert_eq!(
        labels
            .get("org.opencontainers.image.description")
            .map(String::as_str),
        Some(description),
        "ARG-substituted description must survive as one label, not be word-split"
    );

    // Spot-check that individual words from the sentence are not bare keys.
    for word in ["for", "This", "was", "using", "bundle", "declarative"] {
        assert!(
            !labels.contains_key(word),
            "'{word}' must not appear as a standalone label key"
        );
    }
}

/// Multiple `key=value` pairs on a single LABEL line are all captured.
#[tokio::test]
async fn label_multiple_pairs_on_one_line() {
    let ctx = TempDir::new().unwrap();
    common::bundlefile(
        ctx.path(),
        concat!(
            "FROM scratch\n",
            "LABEL org.opencontainers.image.vendor=realkarmakun",
            " org.opencontainers.image.licenses=LGPL-3.0-only\n",
        ),
    );

    let labels = labels(&build_config(&ctx, HashMap::new()).await);

    assert_eq!(
        labels
            .get("org.opencontainers.image.vendor")
            .map(String::as_str),
        Some("realkarmakun"),
    );
    assert_eq!(
        labels
            .get("org.opencontainers.image.licenses")
            .map(String::as_str),
        Some("LGPL-3.0-only"),
    );
}

/// A URL containing `://` but no `=` must not be misidentified as a new
/// `key=value` boundary.
#[tokio::test]
async fn label_url_value_not_split() {
    let ctx = TempDir::new().unwrap();
    common::bundlefile(
        ctx.path(),
        concat!(
            "FROM scratch\n",
            "LABEL org.opencontainers.image.source=",
            "https://github.com/realkarmakun/mcmetrics-exporter\n",
        ),
    );

    let labels = labels(&build_config(&ctx, HashMap::new()).await);

    assert_eq!(
        labels
            .get("org.opencontainers.image.source")
            .map(String::as_str),
        Some("https://github.com/realkarmakun/mcmetrics-exporter"),
    );
}

/// Pre-release version strings containing `-`, `+`, and `.` are preserved
/// exactly.
#[tokio::test]
async fn label_version_string_with_special_chars() {
    let ctx = TempDir::new().unwrap();
    common::bundlefile(
        ctx.path(),
        "FROM scratch\nLABEL org.opencontainers.image.version=0.5.0-rc2\n",
    );

    let labels = labels(&build_config(&ctx, HashMap::new()).await);

    assert_eq!(
        labels
            .get("org.opencontainers.image.version")
            .map(String::as_str),
        Some("0.5.0-rc2"),
    );
}

/// Multiple LABEL directives accumulate into a single map; later declarations
/// override earlier ones for the same key.
#[tokio::test]
async fn label_multiple_directives_accumulate() {
    let ctx = TempDir::new().unwrap();
    common::bundlefile(
        ctx.path(),
        concat!(
            "FROM scratch\n",
            "LABEL org.opencontainers.image.vendor=first\n",
            "LABEL org.opencontainers.image.version=1.0.0\n",
            "LABEL org.opencontainers.image.vendor=second\n", // overrides first
        ),
    );

    let labels = labels(&build_config(&ctx, HashMap::new()).await);

    assert_eq!(
        labels
            .get("org.opencontainers.image.vendor")
            .map(String::as_str),
        Some("second"),
        "later LABEL must override earlier one for the same key"
    );
    assert_eq!(
        labels
            .get("org.opencontainers.image.version")
            .map(String::as_str),
        Some("1.0.0"),
    );
}

/// End-to-end reproduction of the full annotation set used in the
/// mcmetrics-exporter CI workflow.  All eight OCI standard labels must be
/// present with their correct values, and no word from the multi-word
/// description must leak out as a standalone key.
#[tokio::test]
async fn label_full_oci_annotation_set_from_ci() {
    let ctx = TempDir::new().unwrap();

    common::bundlefile(
        ctx.path(),
        concat!(
            "ARG BUILD_DATE\n",
            "ARG GIT_REVISION\n",
            "ARG VERSION\n",
            "ARG DESCRIPTION\n",
            "FROM scratch\n",
            "LABEL org.opencontainers.image.created=${BUILD_DATE}\n",
            "LABEL org.opencontainers.image.revision=${GIT_REVISION}\n",
            "LABEL org.opencontainers.image.version=${VERSION}\n",
            "LABEL org.opencontainers.image.vendor=realkarmakun\n",
            "LABEL org.opencontainers.image.authors=\"Cubixity, UnifiedMetrics contributors\"\n",
            "LABEL org.opencontainers.image.source=https://github.com/realkarmakun/mcmetrics-exporter\n",
            "LABEL org.opencontainers.image.description=${DESCRIPTION}\n",
            "LABEL org.opencontainers.image.licenses=LGPL-3.0-only\n",
        ),
    );

    let description = concat!(
        "mcmetrics-exporter for Velocity. ",
        "This image was created using bundle declarative mod management system, ",
        "and it's purpose is to be used with bundle to manage mcmetrics-exporter installation. ",
        "For more information refer to https://github.com/MeProjectStudio/bundle",
    );

    let overrides = HashMap::from([
        ("BUILD_DATE".to_string(), "2026-04-11T05:22:01Z".to_string()),
        (
            "GIT_REVISION".to_string(),
            "aca5ae0b792019810302ca0d0d99081c2b04b8c4".to_string(),
        ),
        ("VERSION".to_string(), "0.5.0-rc3".to_string()),
        ("DESCRIPTION".to_string(), description.to_string()),
    ]);

    let labels = labels(&build_config(&ctx, overrides).await);

    assert_eq!(
        labels
            .get("org.opencontainers.image.created")
            .map(String::as_str),
        Some("2026-04-11T05:22:01Z"),
    );
    assert_eq!(
        labels
            .get("org.opencontainers.image.revision")
            .map(String::as_str),
        Some("aca5ae0b792019810302ca0d0d99081c2b04b8c4"),
    );
    assert_eq!(
        labels
            .get("org.opencontainers.image.version")
            .map(String::as_str),
        Some("0.5.0-rc3"),
    );
    assert_eq!(
        labels
            .get("org.opencontainers.image.vendor")
            .map(String::as_str),
        Some("realkarmakun"),
    );
    assert_eq!(
        labels
            .get("org.opencontainers.image.authors")
            .map(String::as_str),
        Some("Cubixity, UnifiedMetrics contributors"),
    );
    assert_eq!(
        labels
            .get("org.opencontainers.image.source")
            .map(String::as_str),
        Some("https://github.com/realkarmakun/mcmetrics-exporter"),
    );
    assert_eq!(
        labels
            .get("org.opencontainers.image.description")
            .map(String::as_str),
        Some(description),
        "description must survive as a single label — not split on spaces"
    );
    assert_eq!(
        labels
            .get("org.opencontainers.image.licenses")
            .map(String::as_str),
        Some("LGPL-3.0-only"),
    );

    // No word from the description must appear as a standalone label key.
    for word in [
        "for",
        "This",
        "was",
        "using",
        "bundle",
        "declarative",
        "manage",
        "installation.",
        "more",
        "information",
        "refer",
        "to",
    ] {
        assert!(
            !labels.contains_key(word),
            "'{word}' must not appear as a standalone label key"
        );
    }
}
