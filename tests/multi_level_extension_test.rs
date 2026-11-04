//! Integration tests for extensions of extensions (multi-level `extends`).
//!
//! The fixtures mirror the feed layering: `dev.ucp.common.feed` is a container
//! root, `dev.ucp.shopping.catalog.feed` extends it and adds a `record` shape,
//! and a third capability extends Catalog Feed. These tests pin that contract:
//!   1. An extension of an extension may key its additions on its parent; they
//!      compose onto the root, and the base record constraints still apply.
//!   2. When both the root key and the parent key are present, the root key
//!      wins.
//!   3. An extension with neither key is still rejected.

use std::path::Path;

use serde_json::{json, Value};
use ucp_schema::{compose_from_payload, validate, Direction, ResolveOptions, SchemaBaseConfig};

/// Write the fixture schema tree under `<dir>/schemas/`.
fn write_fixtures(dir: &Path) {
    let common = dir.join("schemas/common");
    let shopping = dir.join("schemas/shopping");
    std::fs::create_dir_all(&common).unwrap();
    std::fs::create_dir_all(&shopping).unwrap();

    // Container root: no object body at the root; shapes live under $defs.
    std::fs::write(
        common.join("feed.json"),
        r##"{
          "$schema": "https://json-schema.org/draft/2020-12/schema",
          "$id": "https://ucp.dev/schemas/common/feed.json",
          "name": "dev.ucp.common.feed",
          "type": "object",
          "$defs": {
            "manifest": {
              "type": "object",
              "required": ["id"],
              "properties": { "id": { "type": "string" } }
            }
          }
        }"##,
    )
    .unwrap();

    // Record type: extends the root and contributes a `record` shape, keyed on
    // the root as a direct extension.
    std::fs::write(
        shopping.join("catalog_feed.json"),
        r##"{
          "$schema": "https://json-schema.org/draft/2020-12/schema",
          "$id": "https://ucp.dev/schemas/shopping/catalog_feed.json",
          "name": "dev.ucp.shopping.catalog.feed",
          "$defs": {
            "record": {
              "type": "object",
              "required": ["id", "title"],
              "properties": {
                "id": { "type": "string" },
                "title": { "type": "string" }
              }
            },
            "dev.ucp.common.feed": {
              "$defs": { "record": { "$ref": "#/$defs/record" } }
            }
          }
        }"##,
    )
    .unwrap();

    // Extension of Catalog Feed keyed only on its parent.
    std::fs::write(
        shopping.join("ads.json"),
        r##"{
          "$schema": "https://json-schema.org/draft/2020-12/schema",
          "$id": "https://ucp.dev/schemas/shopping/ads.json",
          "name": "dev.ucp.shopping.ads",
          "$defs": {
            "ads_record": {
              "allOf": [
                { "$ref": "catalog_feed.json#/$defs/record" },
                { "type": "object", "properties": { "ads_eligible": { "type": "boolean" } } }
              ]
            },
            "dev.ucp.shopping.catalog.feed": {
              "$defs": { "record": { "$ref": "#/$defs/ads_record" } }
            }
          }
        }"##,
    )
    .unwrap();

    // Extension of Catalog Feed with conflicting root and parent keys, so the
    // composed schema reveals which one was used.
    std::fs::write(
        shopping.join("ads_both.json"),
        r##"{
          "$schema": "https://json-schema.org/draft/2020-12/schema",
          "$id": "https://ucp.dev/schemas/shopping/ads_both.json",
          "name": "dev.ucp.shopping.ads_both",
          "$defs": {
            "dev.ucp.common.feed": {
              "$defs": {
                "record": {
                  "type": "object",
                  "properties": { "ads_eligible": { "type": "boolean" } }
                }
              }
            },
            "dev.ucp.shopping.catalog.feed": {
              "$defs": {
                "record": {
                  "type": "object",
                  "properties": { "ads_eligible": { "type": "string" } }
                }
              }
            }
          }
        }"##,
    )
    .unwrap();

    // Extension of Catalog Feed keyed on neither the root nor its parent.
    std::fs::write(
        shopping.join("ads_unkeyed.json"),
        r##"{
          "$schema": "https://json-schema.org/draft/2020-12/schema",
          "$id": "https://ucp.dev/schemas/shopping/ads_unkeyed.json",
          "name": "dev.ucp.shopping.ads_unkeyed",
          "$defs": {
            "dev.ucp.shopping.unrelated": {
              "$defs": { "record": { "type": "object" } }
            }
          }
        }"##,
    )
    .unwrap();
}

fn config(dir: &Path) -> SchemaBaseConfig<'static> {
    // Leak the base path so the borrow is 'static for test convenience.
    let base: &'static Path = Box::leak(dir.join("schemas").into_boxed_path());
    SchemaBaseConfig {
        local_base: Some(base),
        remote_base: Some("https://ucp.dev/schemas"),
    }
}

/// A record payload declaring Common Feed, Catalog Feed, and one extension of
/// Catalog Feed.
fn record_payload(extension: &str, schema_file: &str, record: Value) -> Value {
    let mut payload = json!({
        "ucp": { "capabilities": {
            "dev.ucp.common.feed": [
                { "version": "2026-04-08", "schema": "https://ucp.dev/schemas/common/feed.json" }
            ],
            "dev.ucp.shopping.catalog.feed": [
                { "version": "2026-04-08", "schema": "https://ucp.dev/schemas/shopping/catalog_feed.json", "extends": "dev.ucp.common.feed" }
            ]
        } }
    });
    payload["ucp"]["capabilities"][extension] = json!([{
        "version": "2026-04-08",
        "schema": format!("https://ucp.dev/schemas/shopping/{schema_file}"),
        "extends": "dev.ucp.shopping.catalog.feed"
    }]);
    for (key, value) in record.as_object().unwrap() {
        payload[key] = value.clone();
    }
    payload
}

fn record_opts() -> ResolveOptions {
    ResolveOptions::new(Direction::Response, "list").def_name(Some("record".to_string()))
}

// --- 1. Parent-keyed extension of an extension composes onto the root ---

#[test]
fn parent_keyed_extension_good_record_is_valid() {
    let dir = tempfile::tempdir().unwrap();
    write_fixtures(dir.path());
    let cfg = config(dir.path());

    let payload = record_payload(
        "dev.ucp.shopping.ads",
        "ads.json",
        json!({ "id": "p1", "title": "Widget", "ads_eligible": true }),
    );
    let schema = compose_from_payload(&payload, &cfg).unwrap();

    assert!(
        validate(&schema, &payload, &record_opts()).is_ok(),
        "valid record under a parent-keyed extension should pass"
    );
}

#[test]
fn parent_keyed_extension_constraint_applies() {
    let dir = tempfile::tempdir().unwrap();
    write_fixtures(dir.path());
    let cfg = config(dir.path());

    // `ads_eligible` wrong type (EXTENSION constraint). Base fields valid.
    let payload = record_payload(
        "dev.ucp.shopping.ads",
        "ads.json",
        json!({ "id": "p1", "title": "Widget", "ads_eligible": "yes" }),
    );
    let schema = compose_from_payload(&payload, &cfg).unwrap();

    assert!(
        validate(&schema, &payload, &record_opts()).is_err(),
        "parent-keyed extension's 'ads_eligible' type must be enforced"
    );
}

#[test]
fn parent_keyed_extension_base_constraint_still_applies() {
    let dir = tempfile::tempdir().unwrap();
    write_fixtures(dir.path());
    let cfg = config(dir.path());

    // Missing `title` (record-type constraint) even though the extension field is fine.
    let payload = record_payload(
        "dev.ucp.shopping.ads",
        "ads.json",
        json!({ "id": "p1", "ads_eligible": true }),
    );
    let schema = compose_from_payload(&payload, &cfg).unwrap();

    assert!(
        validate(&schema, &payload, &record_opts()).is_err(),
        "record-type 'title' requirement must still apply under the extension"
    );
}

// --- 2. Root key takes precedence over the parent key ---

#[test]
fn root_key_takes_precedence_over_parent_key() {
    let dir = tempfile::tempdir().unwrap();
    write_fixtures(dir.path());
    let cfg = config(dir.path());

    // The root key declares `ads_eligible` boolean; the parent key declares string.
    let boolean = record_payload(
        "dev.ucp.shopping.ads_both",
        "ads_both.json",
        json!({ "id": "p1", "title": "Widget", "ads_eligible": true }),
    );
    let schema = compose_from_payload(&boolean, &cfg).unwrap();
    assert!(
        validate(&schema, &boolean, &record_opts()).is_ok(),
        "root key must be used when both keys are present"
    );

    let string = record_payload(
        "dev.ucp.shopping.ads_both",
        "ads_both.json",
        json!({ "id": "p1", "title": "Widget", "ads_eligible": "yes" }),
    );
    assert!(
        validate(&schema, &string, &record_opts()).is_err(),
        "parent key must be ignored when the root key is present"
    );
}

// --- 3. Neither key is still rejected ---

#[test]
fn extension_with_neither_key_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    write_fixtures(dir.path());
    let cfg = config(dir.path());

    let payload = record_payload(
        "dev.ucp.shopping.ads_unkeyed",
        "ads_unkeyed.json",
        json!({ "id": "p1", "title": "Widget" }),
    );
    let err = compose_from_payload(&payload, &cfg).unwrap_err();
    assert!(
        err.to_string()
            .contains("missing $defs entry for 'dev.ucp.common.feed'"),
        "got: {err}"
    );
}
