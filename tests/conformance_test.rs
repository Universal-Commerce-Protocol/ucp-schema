//! Draft 2020-12 conformance for `$ref` bundling.
//!
//! Discipline: every assertion here is checked against the `jsonschema`
//! crate as the Draft 2020-12 oracle — accept/reject verdicts or
//! meta-schema validity — never against our own emitted schema text.
//! Text-shaped assertions rot: the original resource-rebasing bug survived
//! a green suite precisely because tests asserted that `"$ref": "#"`
//! *survived* bundling, not what it *denoted* afterwards.
//!
//! These cases were introduced `#[ignore]`d against the legacy transplanting
//! bundler; the referencing-engine rewrite made them pass and removed the
//! attributes.

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
    let v = jsonschema::validator_for(schema)
        .expect("bundled output must compile under a Draft 2020-12 validator");
    let ok = v.is_valid(instance);
    if !ok {
        for e in v.iter_errors(instance) {
            eprintln!("oracle: {} at {}", e, e.instance_path());
        }
    }
    ok
}

/// The #744 shape: an external *fragment* ref whose target contains a
/// recursive `$ref: "#"`. Draft 2020-12 §8.2.1: the nested `#` denotes the
/// *referenced* resource's root, not the referencing document's root.
///
/// resource A (constraint grammar): forbids `path` everywhere.
/// resource B (outer binding): allows `path` at its own root only.
#[test]
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

/// `$ref`-shaped objects inside instance-data keywords (`const`, `enum`,
/// `default`) are payload, not references. Bundling must neither rewrite
/// them structurally nor try to load the documents they appear to name.
#[test]
fn ref_shaped_instance_data_is_left_verbatim() {
    let dir = TempDir::new().unwrap();
    // `./phantom.json` deliberately does not exist: chasing it fails loudly.
    fs::write(
        dir.path().join("schema.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.test/schema.json",
            "type": "object",
            "properties": {
                "template": {
                    "const": { "$ref": "phantom.json", "note": "instance data" }
                },
                "kind": {
                    "enum": [ { "$ref": "#" }, "plain" ],
                    "default": { "$ref": "also-not-a-ref.json" }
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let bundled = bundle(dir.path(), "schema.json");

    assert_eq!(
        bundled["properties"]["template"]["const"],
        json!({ "$ref": "phantom.json", "note": "instance data" }),
        "const value must survive byte-identical"
    );
    assert_eq!(
        bundled["properties"]["kind"]["enum"][0],
        json!({ "$ref": "#" }),
        "enum member must survive byte-identical"
    );

    // And the oracle agrees the instance data still matches exactly.
    assert!(oracle_is_valid(
        &bundled,
        &json!({ "template": { "$ref": "phantom.json", "note": "instance data" } })
    ));
    assert!(!oracle_is_valid(
        &bundled,
        &json!({ "template": { "$ref": "phantom.json" } })
    ));
}

/// Sibling hoisting must also skip instance data: an object value that
/// happens to contain a `$ref` key plus other members would otherwise be
/// rewritten into an `allOf` conjunction, corrupting the payload.
#[test]
fn hoisting_does_not_restructure_enum_members() {
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
        dir.path().join("schema.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.test/schema.json",
            "type": "object",
            "properties": {
                // Real reference with siblings: must hoist + re-merge.
                "name": { "$ref": "leaf.json", "description": "display name" },
                // Instance data that merely looks like one: must not move.
                "shape": { "const": { "$ref": "leaf.json", "extra": 1 } }
            }
        })
        .to_string(),
    )
    .unwrap();

    let bundled = bundle(dir.path(), "schema.json");

    assert_eq!(
        bundled["properties"]["shape"]["const"],
        json!({ "$ref": "leaf.json", "extra": 1 })
    );
    // The real ref materialized with its sibling annotation intact.
    assert_eq!(bundled["properties"]["name"]["type"], json!("string"));
    assert_eq!(
        bundled["properties"]["name"]["description"],
        json!("display name")
    );
    assert!(oracle_is_valid(&bundled, &json!({ "name": "x" })));
    assert!(!oracle_is_valid(&bundled, &json!({ "name": 7 })));
}

/// The transitive closure: a fetched document's *own* external refs must
/// also resolve (root → mid → leaf). Distilled from the corpus chain
/// capability.json → ucp.json → common types, which the registry must be
/// pre-populated to satisfy.
#[test]
fn transitively_referenced_documents_resolve() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("leaf.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.test/leaf.json",
            "type": "integer",
            "minimum": 1
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        dir.path().join("mid.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.test/mid.json",
            "type": "object",
            "properties": { "leaf": { "$ref": "leaf.json" } }
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
            "properties": { "mid": { "$ref": "mid.json" } }
        })
        .to_string(),
    )
    .unwrap();

    let bundled = bundle(dir.path(), "root.json");
    assert!(oracle_is_valid(&bundled, &json!({ "mid": { "leaf": 2 } })));
    assert!(!oracle_is_valid(&bundled, &json!({ "mid": { "leaf": 0 } })));
}

/// A fragment ref into a fetched resource whose target pointer-refs a
/// *sibling* definition: `#/$defs/wrapper` inside lib.json refs
/// `#/$defs/base` — both pointers must keep resolving within lib.json.
#[test]
fn fragment_target_referencing_sibling_def_resolves_in_source_resource() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("lib.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.test/lib.json",
            "$defs": {
                "base": { "type": "string", "minLength": 2 },
                "wrapper": {
                    "type": "object",
                    "properties": { "value": { "$ref": "#/$defs/base" } }
                }
            }
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
            "properties": { "w": { "$ref": "lib.json#/$defs/wrapper" } }
        })
        .to_string(),
    )
    .unwrap();

    let bundled = bundle(dir.path(), "root.json");
    assert!(oracle_is_valid(
        &bundled,
        &json!({ "w": { "value": "ok" } })
    ));
    assert!(
        !oracle_is_valid(&bundled, &json!({ "w": { "value": "x" } })),
        "sibling-def minLength must survive materialization"
    );
}

/// Materialization must not swallow UCP resolver diagnostics: a base
/// requirement weakened by an extension branch is a monotonicity violation
/// whether the base arrives inline or through an external `$ref`.
#[test]
fn external_allof_monotonicity_violation_still_errors() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("base.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.test/base.json",
            "type": "object",
            "required": ["id"],
            "properties": { "id": { "type": "string" } }
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        dir.path().join("ext.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.test/ext.json",
            "allOf": [
                { "$ref": "base.json" },
                {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "ucp_response": "omit" }
                    }
                }
            ]
        })
        .to_string(),
    )
    .unwrap();

    let bundled = bundle(dir.path(), "ext.json");
    let opts = ucp_schema::ResolveOptions::new(ucp_schema::Direction::Response, "search");
    assert!(
        matches!(
            ucp_schema::resolve(&bundled, &opts),
            Err(ucp_schema::ResolveError::MonotonicityViolation { .. })
        ),
        "omitting a base-required field must stay a monotonicity violation after bundling"
    );
}

/// Same for type conflicts across externally referenced allOf branches.
#[test]
fn external_allof_type_conflict_still_errors() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("base.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.test/base.json",
            "type": "object",
            "properties": { "count": { "type": "string" } }
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        dir.path().join("ext.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.test/ext.json",
            "allOf": [
                { "$ref": "base.json" },
                {
                    "type": "object",
                    "properties": { "count": { "type": "number" } }
                }
            ]
        })
        .to_string(),
    )
    .unwrap();

    let bundled = bundle(dir.path(), "ext.json");
    let opts = ucp_schema::ResolveOptions::new(ucp_schema::Direction::Response, "search");
    assert!(matches!(
        ucp_schema::resolve(&bundled, &opts),
        Err(ucp_schema::ResolveError::TypeConflict { .. })
    ));
}

/// Strict resolution across a materialized external ref: the composed
/// schema must reject unknown fields while accepting fields contributed by
/// every branch (unevaluatedProperties semantics over allOf).
#[test]
fn strict_mode_closes_bundled_composition() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("base.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.test/base.json",
            "type": "object",
            "properties": { "id": { "type": "string" } }
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        dir.path().join("ext.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.test/ext.json",
            "allOf": [
                { "$ref": "base.json" },
                {
                    "type": "object",
                    "properties": { "note": { "type": "string" } }
                }
            ]
        })
        .to_string(),
    )
    .unwrap();

    let bundled = bundle(dir.path(), "ext.json");
    let opts =
        ucp_schema::ResolveOptions::new(ucp_schema::Direction::Response, "search").strict(true);
    let resolved = ucp_schema::resolve(&bundled, &opts).expect("strict resolve succeeds");

    assert!(oracle_is_valid(
        &resolved,
        &json!({ "id": "a", "note": "b" })
    ));
    assert!(
        !oracle_is_valid(&resolved, &json!({ "id": "a", "unknown": true })),
        "strict mode must reject fields no branch declares"
    );
}

/// The #744 shape over HTTP: remote bundling must preserve the fetched
/// resource's root for its internal `$ref: "#"` exactly like local bundling.
#[cfg(feature = "remote")]
#[test]
fn remote_fragment_ref_preserves_target_resource_root() {
    let mut server = mockito::Server::new();
    let a = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": format!("{}/a.json", server.url()),
        "type": "object",
        "properties": {
            "properties": {
                "type": "object",
                "additionalProperties": { "$ref": "#" }
            }
        },
        "additionalProperties": false
    });
    let _a = server
        .mock("GET", "/a.json")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(a.to_string())
        .create();

    let mut schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": format!("{}/b.json", server.url()),
        "type": "object",
        "properties": {
            "path": { "type": "string" },
            "properties": { "$ref": "a.json#/properties/properties" }
        },
        "additionalProperties": false
    });
    ucp_schema::bundle_refs_remote(&mut schema, &format!("{}/b.json", server.url()))
        .expect("remote bundling succeeds");

    assert!(!oracle_is_valid(
        &schema,
        &json!({ "properties": { "line": { "path": "$" } } })
    ));
    assert!(oracle_is_valid(
        &schema,
        &json!({ "path": "$", "properties": { "line": { "properties": {} } } })
    ));
}

/// The UCP capability-container convention: `$defs/<capability-name>` is a
/// *container* whose named members (actor schemas, operation shapes) are
/// schemas one level deeper — the same shape `select_operation_schema`
/// selects through. Refs live only inside container members here, so the
/// crawl, sibling protection, and identity stripping must all traverse it.
#[test]
fn capability_container_members_bundle_with_protected_siblings() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("capability.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.test/capability.json",
            "$defs": {
                "platform_schema": {
                    "type": "object",
                    "required": ["version"],
                    "properties": { "version": { "type": "string" } }
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        dir.path().join("cap.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.test/cap.json",
            "$defs": {
                "dev.example.capability": {
                    "platform_schema": {
                        "$ref": "capability.json#/$defs/platform_schema",
                        "description": "use-site annotation",
                        "ucp_response": "optional"
                    }
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let bundled = bundle(dir.path(), "cap.json");
    let member = &bundled["$defs"]["dev.example.capability"]["platform_schema"];

    // Ref materialized: target constraints present, no $ref left behind.
    assert_eq!(
        member["required"],
        json!(["version"]),
        "target must materialize: {member}"
    );
    // Siblings survived (annotation + UCP applicability annotation).
    assert_eq!(member["description"], json!("use-site annotation"));
    assert_eq!(member["ucp_response"], json!("optional"));
    // Materialized copy shed the source's resource identity.
    assert!(
        member.get("$id").is_none() && member.get("$schema").is_none(),
        "container member must not inherit the source resource identity: {member}"
    );
}

/// An authored `allOf` coexisting with a `$ref` and colliding use-site
/// constraints: everything is one conjunction. The ref joins the authored
/// branches; the use site's stricter `minLength` must keep applying.
#[test]
fn authored_allof_with_ref_sibling_applies_conjunctively() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("target.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.test/target.json",
            "type": "string",
            "minLength": 5
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
                "v": {
                    "$ref": "target.json",
                    "allOf": [ { "maxLength": 10 } ],
                    "minLength": 7
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let bundled = bundle(dir.path(), "host.json");

    // Effective: minLength 7 (use site) AND minLength 5 (target) AND maxLength 10.
    assert!(
        !oracle_is_valid(&bundled, &json!({ "v": "123456" })),
        "len 6 violates use-site minLength 7"
    );
    assert!(oracle_is_valid(&bundled, &json!({ "v": "1234567" })));
    assert!(
        !oracle_is_valid(&bundled, &json!({ "v": "12345678901" })),
        "len 11 violates branch maxLength 10"
    );
}

/// Two different documents claiming the same canonical `$id` is an
/// authoring error; silent first-wins would bind `$id`-relative refs to
/// whichever file the crawl reached first.
#[test]
fn duplicate_canonical_id_claims_error() {
    let dir = TempDir::new().unwrap();
    for (file, max) in [("one.json", 1), ("two.json", 2)] {
        fs::write(
            dir.path().join(file),
            json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "$id": "https://example.test/shared.json",
                "type": "integer",
                "maximum": max
            })
            .to_string(),
        )
        .unwrap();
    }
    fs::write(
        dir.path().join("root.json"),
        json!({
            "type": "object",
            "properties": {
                "a": { "$ref": "one.json" },
                "b": { "$ref": "two.json" }
            }
        })
        .to_string(),
    )
    .unwrap();

    let path = dir.path().join("root.json");
    let mut schema = load_schema(&path).unwrap();
    let err = bundle_refs(&mut schema, path.parent().unwrap()).unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("claim the same canonical $id"),
        "expected duplicate-$id diagnostic, got: {message}"
    );
}

/// The masking sentinel is reserved everywhere: a schema key with that name
/// would otherwise be silently rewritten to `$ref` by the final unmask.
#[test]
fn sentinel_member_name_is_rejected_anywhere() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("schema.json"),
        json!({
            "type": "object",
            "properties": {
                "__ucp_instance_ref__": { "type": "string" }
            }
        })
        .to_string(),
    )
    .unwrap();

    let path = dir.path().join("schema.json");
    let mut schema = load_schema(&path).unwrap();
    let err = bundle_refs(&mut schema, path.parent().unwrap()).unwrap_err();
    assert!(err.to_string().contains("reserved member name"));
}

// ---------------------------------------------------------------------------
// Scenarios adopted from the reported bundler defects (issues #43, #45, #46
// and the accompanying PRs #44, #47, #51), distilled to self-contained
// fixtures and locked as verdict assertions. Fixture shape mirrors the
// shipped chain: checkout.json -> payment.json ->
// payment_instrument.json#/$defs/selected_payment_instrument, whose
// allOf[0] is `$ref: "#"` (the instrument file's own root).
// ---------------------------------------------------------------------------

fn write_instrument_chain(dir: &Path) {
    fs::write(
        dir.join("instrument.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.test/instrument.json",
            "type": "object",
            "required": ["id", "handler_id", "type"],
            "properties": {
                "id": { "type": "string" },
                "handler_id": { "type": "string" },
                "type": { "type": "string" }
            },
            "$defs": {
                "selected": {
                    "allOf": [
                        { "$ref": "#" },
                        {
                            "type": "object",
                            "properties": { "selected": { "type": "boolean" } }
                        }
                    ]
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        dir.join("payment.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.test/payment.json",
            "type": "object",
            "properties": {
                // Deliberately shares the name `instruments` with an
                // instrument-level extra property in the false-reject case.
                "instruments": {
                    "type": "array",
                    "items": { "$ref": "instrument.json#/$defs/selected" }
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        dir.join("checkout.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.test/checkout.json",
            "type": "object",
            "properties": { "payment": { "$ref": "payment.json" } }
        })
        .to_string(),
    )
    .unwrap();
}

/// Issue #43, false accept: an instrument with none of the required fields
/// must fail — under the old bundler `#` bound to the inlining file, so the
/// instrument base schema never applied.
#[test]
fn instrument_chain_rejects_missing_required_fields() {
    let dir = TempDir::new().unwrap();
    write_instrument_chain(dir.path());
    let bundled = bundle(dir.path(), "checkout.json");

    let bad = json!({ "payment": { "instruments": [ { "selected": true } ] } });
    let good = json!({ "payment": { "instruments": [
        { "id": "i1", "handler_id": "h1", "type": "card", "selected": true }
    ] } });
    assert!(
        !oracle_is_valid(&bundled, &bad),
        "instrument base schema must apply"
    );
    assert!(oracle_is_valid(&bundled, &good));
}

/// Issue #43, false reject: a spec-valid instrument carrying an extra
/// property that happens to share a name with the *inlining* file's property
/// (`instruments`) must stay valid. Under the old bundler `#` bound to
/// payment.json, which types that name as an array.
#[test]
fn instrument_chain_accepts_extra_property_aliasing_outer_schema() {
    let dir = TempDir::new().unwrap();
    write_instrument_chain(dir.path());
    let bundled = bundle(dir.path(), "checkout.json");

    let aliased = json!({ "payment": { "instruments": [
        { "id": "i1", "handler_id": "h1", "type": "card",
          "instruments": "bogus-string" }
    ] } });
    assert!(
        oracle_is_valid(&bundled, &aliased),
        "instrument-level extra property must be judged by instrument.json's root, not payment.json's"
    );
}

/// Issue #43, strict mode: a valid instrument must stay valid under strict
/// resolution. The old bundler closed objects against the wrong root, so
/// every instrument field counted as unevaluated.
#[test]
fn instrument_chain_strict_accepts_valid_instrument() {
    let dir = TempDir::new().unwrap();
    write_instrument_chain(dir.path());
    let bundled = bundle(dir.path(), "checkout.json");
    let opts =
        ucp_schema::ResolveOptions::new(ucp_schema::Direction::Response, "create").strict(true);
    let resolved = ucp_schema::resolve(&bundled, &opts).expect("strict resolve succeeds");

    assert!(oracle_is_valid(
        &resolved,
        &json!({ "payment": { "instruments": [
        { "id": "i1", "handler_id": "h1", "type": "card", "selected": true }
    ] } })
    ));
    assert!(!oracle_is_valid(
        &resolved,
        &json!({ "payment": { "instruments": [
        { "id": "i1", "handler_id": "h1", "type": "card", "unknown_field": 1 }
    ] } })
    ));
}

/// Issue #45 as filed (CLI): `validate --def` on a def whose subtree carries
/// `$ref: "#"` must not crash and must validate against the *source* root.
#[test]
fn validate_def_cli_with_self_root_ref() {
    let dir = TempDir::new().unwrap();
    write_instrument_chain(dir.path());
    fs::write(
        dir.path().join("good-inst.json"),
        json!({ "id": "i1", "handler_id": "h", "type": "card" }).to_string(),
    )
    .unwrap();
    fs::write(
        dir.path().join("bad-inst.json"),
        json!({ "selected": true }).to_string(),
    )
    .unwrap();

    let run = |payload: &str| {
        std::process::Command::new(env!("CARGO_BIN_EXE_ucp-schema"))
            .current_dir(dir.path())
            .args([
                "validate",
                payload,
                "--op",
                "create",
                "--schema",
                "instrument.json",
                "--def",
                "selected",
                "--json",
            ])
            .output()
            .expect("binary runs")
    };
    let good = run("good-inst.json");
    assert!(
        good.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&good.stderr)
    );
    let bad = run("bad-inst.json");
    let verdict: Value = serde_json::from_slice(&bad.stdout).expect("json output");
    assert_eq!(
        verdict["valid"],
        json!(false),
        "the source root's required fields must reach the selected def: {verdict}"
    );
}

/// Issue #46: an internal recursive pointer ref (`#/$defs/node` self-cycle)
/// is legal Draft 2020-12 and must bundle and validate, not overflow the
/// stack. (The old bundler guarded only the external-ref branch.)
#[test]
fn internal_recursive_pointer_ref_bundles_and_validates() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("recursive.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.test/recursive.json",
            "type": "object",
            "properties": { "root": { "$ref": "#/$defs/node" } },
            "$defs": {
                "node": {
                    "type": "object",
                    "properties": { "child": { "$ref": "#/$defs/node" } },
                    "additionalProperties": false
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let bundled = bundle(dir.path(), "recursive.json");
    assert!(oracle_is_valid(
        &bundled,
        &json!({ "root": { "child": { "child": {} } } })
    ));
    assert!(
        !oracle_is_valid(&bundled, &json!({ "root": { "child": { "oops": 1 } } })),
        "violations deep in the recursion must be caught"
    );
}

/// Issue #46's remote twin: the same internal self-cycle fetched over HTTP.
#[cfg(feature = "remote")]
#[test]
fn internal_recursive_pointer_ref_bundles_remotely() {
    let mut server = mockito::Server::new();
    let doc = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": format!("{}/recursive.json", server.url()),
        "type": "object",
        "properties": { "root": { "$ref": "#/$defs/node" } },
        "$defs": {
            "node": {
                "type": "object",
                "properties": { "child": { "$ref": "#/$defs/node" } },
                "additionalProperties": false
            }
        }
    });
    let _m = server
        .mock("GET", "/recursive.json")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(doc.to_string())
        .create();

    let mut schema = doc.clone();
    ucp_schema::bundle_refs_remote(&mut schema, &format!("{}/recursive.json", server.url()))
        .expect("remote bundling succeeds");
    assert!(oracle_is_valid(
        &schema,
        &json!({ "root": { "child": {} } })
    ));
    assert!(!oracle_is_valid(&schema, &json!({ "root": { "oops": 1 } })));
}

/// Compose-path twin of the resource-identity fix (issue #43's third organ,
/// PR #44's territory): an extension whose contribution uses a *recursive
/// helper def* (`#/$defs/tree_node` self-cycle). The bundler retains the
/// cycle; extraction into a composed document must anchor it to the source
/// document instead of letting it dangle
/// (`Pointer '/$defs/tree_node' does not exist`) or rebind.
#[test]
fn composed_extension_with_recursive_helper_def_validates() {
    let dir = TempDir::new().unwrap();
    let schemas = dir.path().join("schemas/shopping");
    fs::create_dir_all(&schemas).unwrap();
    fs::copy(
        "tests/fixtures/compose/schemas/shopping/checkout.json",
        schemas.join("checkout.json"),
    )
    .unwrap();
    fs::write(
        schemas.join("treeext.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://ucp.dev/schemas/shopping/treeext.json",
            "name": "dev.ucp.shopping.treeext",
            "version": "2026-01-11",
            "title": "Tree Extension",
            "$defs": {
                "dev.ucp.shopping.checkout": {
                    "type": "object",
                    "additionalProperties": true,
                    "properties": {
                        "category_tree": { "$ref": "#/$defs/tree_node" }
                    }
                },
                "tree_node": {
                    "type": "object",
                    "properties": {
                        "label": { "type": "string" },
                        "children": { "type": "array", "items": { "$ref": "#/$defs/tree_node" } }
                    },
                    "additionalProperties": false
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    let payload = json!({
        "ucp": {
            "capabilities": {
                "dev.ucp.shopping.checkout": [
                    { "version": "2026-01-11", "schema": "https://ucp.dev/schemas/shopping/checkout.json" }
                ],
                "dev.ucp.shopping.treeext": [
                    { "version": "2026-01-11", "schema": "https://ucp.dev/schemas/shopping/treeext.json", "extends": "dev.ucp.shopping.checkout" }
                ]
            }
        },
        "id": "chk_1",
        "status": "incomplete",
        "category_tree": { "label": "root", "children": [ { "label": "kid", "children": [] } ] }
    });
    fs::write(dir.path().join("good.json"), payload.to_string()).unwrap();
    let mut bad = payload;
    bad["category_tree"]["children"][0]["bogus_key"] = json!(1);
    fs::write(dir.path().join("bad.json"), bad.to_string()).unwrap();

    let run = |name: &str| {
        std::process::Command::new(env!("CARGO_BIN_EXE_ucp-schema"))
            .current_dir(dir.path())
            .args([
                "validate",
                name,
                "--response",
                "--op",
                "create",
                "--schema-local-base",
                ".",
                "--schema-remote-base",
                "https://ucp.dev",
                "--json",
            ])
            .output()
            .expect("binary runs")
    };
    let good = run("good.json");
    let verdict: Value = serde_json::from_slice(&good.stdout).expect("json output");
    assert_eq!(
        verdict["valid"],
        json!(true),
        "recursive extension must compose: {verdict}"
    );
    let bad_out = run("bad.json");
    let verdict: Value = serde_json::from_slice(&bad_out.stdout).expect("json output");
    assert_eq!(
        verdict["valid"],
        json!(false),
        "violations inside the recursion must be caught: {verdict}"
    );
}

/// Compose anchoring stays dormant for acyclic helpers: the bundler
/// pre-materializes them, so the extracted contribution carries no
/// root-local refs and passes through without any embedded source
/// (no urn: markers, no capability-name $defs entry).
#[test]
fn composed_extension_with_acyclic_helper_needs_no_anchor() {
    let dir = TempDir::new().unwrap();
    let schemas = dir.path().join("schemas/shopping");
    fs::create_dir_all(&schemas).unwrap();
    fs::copy(
        "tests/fixtures/compose/schemas/shopping/checkout.json",
        schemas.join("checkout.json"),
    )
    .unwrap();
    fs::write(
        schemas.join("acycext.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://ucp.dev/schemas/shopping/acycext.json",
            "name": "dev.ucp.shopping.acycext",
            "version": "2026-01-11",
            "$defs": {
                "dev.ucp.shopping.checkout": {
                    "type": "object",
                    "additionalProperties": true,
                    "properties": { "badge": { "$ref": "#/$defs/badge" } }
                },
                "badge": {
                    "type": "object",
                    "properties": { "label": { "type": "string" } },
                    "additionalProperties": false
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    let payload = json!({
        "ucp": { "capabilities": {
            "dev.ucp.shopping.checkout": [
                { "version": "2026-01-11", "schema": "https://ucp.dev/schemas/shopping/checkout.json" }
            ],
            "dev.ucp.shopping.acycext": [
                { "version": "2026-01-11", "schema": "https://ucp.dev/schemas/shopping/acycext.json", "extends": "dev.ucp.shopping.checkout" }
            ]
        } },
        "id": "c1", "status": "incomplete",
        "badge": { "label": "x" }
    });
    fs::write(dir.path().join("good.json"), payload.to_string()).unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ucp-schema"))
        .current_dir(dir.path())
        .args([
            "resolve",
            "good.json",
            "--response",
            "--op",
            "create",
            "--schema-local-base",
            ".",
            "--schema-remote-base",
            "https://ucp.dev",
        ])
        .output()
        .expect("binary runs");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !text.contains("urn:ucp-schema") && !text.contains("acycext.json\""),
        "anchoring must stay dormant for acyclic helpers"
    );
    let resolved: Value = serde_json::from_str(&text).unwrap();
    let validator = jsonschema::validator_for(&resolved).unwrap();
    assert!(validator
        .is_valid(&json!({ "id": "c1", "status": "incomplete", "badge": { "label": "x" } })));
    assert!(
        !validator.is_valid(&json!({ "id": "c1", "status": "incomplete", "badge": { "oops": 1 } }))
    );
}
