//! Draft 2020-12 conformance for `$ref` bundling.
//!
//! Discipline: every assertion here is checked against the `jsonschema`
//! crate as the Draft 2020-12 oracle — accept/reject verdicts or
//! meta-schema validity — never against our own emitted schema text.
//! Text-shaped assertions rot: the original resource-rebasing bug survived
//! a green suite precisely because tests asserted that `"$ref": "#"`
//! *survived* bundling, not what it *denoted* afterwards.
//!
//! Cases marked `#[ignore]` document known-broken behavior of the current
//! bundler; the commit that rebuilds bundling on the upstream referencing
//! engine removes the attributes.

use std::fs;
use std::path::Path;

use serde_json::{json, Value};
use tempfile::TempDir;
use ucp_schema::{bundle_refs, load_schema};

/// Bundle `root` (a file inside `dir`) and return the self-contained schema.
fn bundle(dir: &Path, root: &str) -> Value {
    let path = dir.join(root);
    let mut schema = load_schema(&path).expect("fixture loads");
    bundle_refs(&mut schema, path.parent().expect("fixture has a parent"))
        .expect("bundling succeeds");
    schema
}

/// Compile the bundled schema with the oracle and classify an instance.
fn oracle_is_valid(schema: &Value, instance: &Value) -> bool {
    jsonschema::validator_for(schema)
        .expect("bundled output must compile under a Draft 2020-12 validator")
        .is_valid(instance)
}

/// The #744 shape: an external *fragment* ref whose target contains a
/// recursive `$ref: "#"`. Draft 2020-12 §8.2.1: the nested `#` denotes the
/// *referenced* resource's root, not the referencing document's root.
///
/// resource A (constraint grammar): forbids `path` everywhere.
/// resource B (outer binding): allows `path` at its own root only.
#[test]
#[ignore = "resource identity is rebased by the current bundler; fixed by the referencing-engine rewrite"]
fn fragment_ref_into_recursive_resource_preserves_target_root() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("a.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.test/a.json",
            "type": "object",
            "properties": {
                "properties": {
                    "type": "object",
                    "additionalProperties": { "$ref": "#" }
                }
            },
            "additionalProperties": false
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        dir.path().join("b.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.test/b.json",
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "properties": { "$ref": "a.json#/properties/properties" }
            },
            "additionalProperties": false
        })
        .to_string(),
    )
    .unwrap();

    let bundled = bundle(dir.path(), "b.json");

    // `path` inside a nested constraint is governed by A's root, which
    // forbids it. The buggy bundler rebased `#` onto B, which allows it.
    let bad = json!({ "properties": { "line": { "path": "$" } } });
    let good = json!({ "path": "$", "properties": { "line": { "properties": {} } } });
    assert!(
        !oracle_is_valid(&bundled, &bad),
        "nested `path` must be rejected by resource A's root"
    );
    assert!(oracle_is_valid(&bundled, &good));
}

/// Draft 2020-12 evaluates `$ref` siblings conjunctively: both the target's
/// and the sibling's constraints apply. The legacy bundler let sibling keys
/// *replace* target keys, silently dropping the target's `maximum: 50`.
#[test]
#[ignore = "sibling keywords replace target constraints in the current bundler; fixed by the referencing-engine rewrite"]
fn ref_siblings_apply_conjunctively() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("amount.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.test/amount.json",
            "type": "integer",
            "maximum": 50
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        dir.path().join("host.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.test/host.json",
            "type": "object",
            "properties": {
                "v": { "$ref": "amount.json", "maximum": 100 }
            }
        })
        .to_string(),
    )
    .unwrap();

    let bundled = bundle(dir.path(), "host.json");

    // Conjunction: max(50) AND max(100) — 75 violates the target's bound.
    assert!(
        !oracle_is_valid(&bundled, &json!({ "v": 75 })),
        "target's maximum: 50 must survive bundling alongside the sibling"
    );
    assert!(oracle_is_valid(&bundled, &json!({ "v": 42 })));
}

/// Mutual recursion across files is legal JSON Schema (trees, graphs).
/// The bundler must retain the cycle with resource identity intact rather
/// than reject the schema.
#[test]
#[ignore = "cross-file recursion is a hard error in the current bundler; fixed by the referencing-engine rewrite"]
fn cross_file_recursion_bundles_and_validates() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("a.json"),
        json!({
            "$id": "https://example.test/a.json",
            "type": "object",
            "properties": { "b": { "$ref": "b.json" } },
            "additionalProperties": false
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        dir.path().join("b.json"),
        json!({
            "$id": "https://example.test/b.json",
            "type": "object",
            "properties": { "a": { "$ref": "a.json" } },
            "additionalProperties": false
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        dir.path().join("root.json"),
        json!({
            "type": "object",
            "properties": { "root": { "$ref": "a.json" } }
        })
        .to_string(),
    )
    .unwrap();

    let bundled = bundle(dir.path(), "root.json");

    assert!(oracle_is_valid(
        &bundled,
        &json!({ "root": { "b": { "a": { "b": {} } } } })
    ));
    assert!(
        !oracle_is_valid(
            &bundled,
            &json!({ "root": { "b": { "a": { "oops": 1 } } } })
        ),
        "violations deep inside the cycle must still be caught"
    );
}

/// A diamond: two properties reference the same non-recursive resource.
/// Materialized copies are ordinary subschemas; they must not carry the
/// resource's `$id`/`$schema` (Draft 2020-12 §8.1.1 forbids `$schema`
/// outside a resource root, and repeated `$id` claims are degenerate).
#[test]
#[ignore = "the current bundler stamps $id/$schema on every inlined copy; fixed by the referencing-engine rewrite"]
fn materialized_copies_shed_resource_identity() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("amount.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.test/amount.json",
            "type": "integer",
            "minimum": 0
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        dir.path().join("price.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.test/price.json",
            "type": "object",
            "properties": {
                "net": { "$ref": "amount.json" },
                "gross": { "$ref": "amount.json" }
            }
        })
        .to_string(),
    )
    .unwrap();

    let bundled = bundle(dir.path(), "price.json");

    assert_eq!(
        count_nested_identity(&bundled),
        0,
        "no materialized copy of a non-recursive resource may keep $id/$schema: {bundled}"
    );
    assert!(!oracle_is_valid(&bundled, &json!({ "net": -1 })));
    assert!(oracle_is_valid(&bundled, &json!({ "net": 1, "gross": 2 })));
}

/// §8.1.1: `$schema` MUST NOT appear in non-resource-root schema objects.
/// Every `$schema` below the document root must sit beside an `$id`
/// (i.e. on an embedded resource root). Guards the defect class where a
/// bundler strips `$id` but leaves `$schema` orphaned.
#[test]
fn bundled_output_declares_schema_only_at_resource_roots() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("leaf.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.test/leaf.json",
            "type": "string"
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        dir.path().join("root.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.test/root.json",
            "type": "object",
            "properties": { "leaf": { "$ref": "leaf.json" } }
        })
        .to_string(),
    )
    .unwrap();

    let bundled = bundle(dir.path(), "root.json");
    assert_eq!(
        count_orphan_schema(&bundled, true),
        0,
        "orphaned $schema below the document root: {bundled}"
    );
    assert!(
        jsonschema::meta::validate(&bundled).is_ok(),
        "bundled output must satisfy its own meta-schema"
    );
}

/// The most common CLI invocation — a bare relative filename whose
/// `Path::parent()` is the empty string — must keep working.
#[test]
fn bare_filename_schema_argument_resolves() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("leaf.json"),
        json!({ "type": "integer" }).to_string(),
    )
    .unwrap();
    fs::write(
        dir.path().join("schema.json"),
        json!({
            "type": "object",
            "properties": { "leaf": { "$ref": "leaf.json" } }
        })
        .to_string(),
    )
    .unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ucp-schema"))
        .current_dir(dir.path())
        .args([
            "resolve",
            "schema.json",
            "--request",
            "--op",
            "create",
            "--bundle",
        ])
        .output()
        .expect("binary runs");
    assert!(
        output.status.success(),
        "bare-filename bundling failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let bundled: Value = serde_json::from_slice(&output.stdout).expect("emits JSON");
    assert!(oracle_is_valid(&bundled, &json!({ "leaf": 3 })));
    assert!(!oracle_is_valid(&bundled, &json!({ "leaf": "nope" })));
}

/// Count `$schema` keywords below the document root that do not sit on an
/// embedded resource root (no `$id` sibling).
fn count_orphan_schema(value: &Value, is_root: bool) -> usize {
    match value {
        Value::Object(obj) => {
            let own =
                usize::from(!is_root && obj.contains_key("$schema") && !obj.contains_key("$id"));
            own + obj
                .values()
                .map(|v| count_orphan_schema(v, false))
                .sum::<usize>()
        }
        Value::Array(arr) => arr.iter().map(|v| count_orphan_schema(v, false)).sum(),
        _ => 0,
    }
}

/// Count `$id` or `$schema` declarations below the document root.
fn count_nested_identity(value: &Value) -> usize {
    fn walk(value: &Value, is_root: bool) -> usize {
        match value {
            Value::Object(obj) => {
                let own = usize::from(
                    !is_root && (obj.contains_key("$id") || obj.contains_key("$schema")),
                );
                own + obj.values().map(|v| walk(v, false)).sum::<usize>()
            }
            Value::Array(arr) => arr.iter().map(|v| walk(v, false)).sum(),
            _ => 0,
        }
    }
    walk(value, true)
}
