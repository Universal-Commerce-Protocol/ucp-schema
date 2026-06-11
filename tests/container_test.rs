//! Integration tests for container-shaped capability composition + validation.
//!
//! A container capability holds several message bodies under
//! `$defs/{op}_{direction}` rather than being a single validatable object, so
//! the body to validate is selected by operation and direction. These tests
//! pin that contract:
//!   1. A container validates against the selected message body (a body that
//!      violates the shape is rejected).
//!   2. `{op}_{direction}` selects the right body across operations (search vs
//!      lookup vs get_product) and directions (request vs response).
//!   3. A requested operation with no body is a hard error, not a pass.
//!   4. An extension that mirrors the container per operation composes per
//!      operation via allOf — base AND extension constraints both apply.
//!   5. Single-object capabilities (checkout) are unaffected.
//!   6. An extension that does not mirror the container is rejected.

use std::path::Path;

use serde_json::{json, Value};
use ucp_schema::{
    compose_from_payload, is_container_schema, select_operation_schema, validate, Direction,
    ResolveOptions, SchemaBaseConfig, ValidateError,
};

/// Write the fixture schema tree under `<dir>/schemas/shopping/` and return the
/// schema base config that maps `https://ucp.dev/schemas/...` to it.
fn write_fixtures(dir: &Path) {
    let shopping = dir.join("schemas/shopping");
    std::fs::create_dir_all(&shopping).unwrap();

    // Base container capability: catalog.search. No object body at root; the
    // request/response shapes live under $defs. `search_response` references a
    // sibling helper def (`product`) to exercise sibling-ref handling.
    std::fs::write(
        shopping.join("catalog_search.json"),
        r##"{
          "$schema": "https://json-schema.org/draft/2020-12/schema",
          "$id": "https://ucp.dev/schemas/shopping/catalog_search.json",
          "name": "dev.ucp.shopping.catalog.search",
          "type": "object",
          "$defs": {
            "search_request": {
              "type": "object",
              "required": ["query"],
              "properties": { "query": { "type": "string" } }
            },
            "search_response": {
              "type": "object",
              "required": ["products"],
              "properties": {
                "products": { "type": "array", "items": { "$ref": "#/$defs/product" } }
              }
            },
            "product": {
              "type": "object",
              "required": ["id", "title"],
              "properties": {
                "id": { "type": "string" },
                "title": { "type": "string" }
              }
            }
          }
        }"##,
    )
    .unwrap();

    // Extension capability: fulfillment. Mirrors the container per operation
    // under $defs[<capability>].$defs, re-$ref-ing the base op shape and adding
    // fields (the fulfillment-branch shape).
    std::fs::write(
        shopping.join("fulfillment.json"),
        r##"{
          "$schema": "https://json-schema.org/draft/2020-12/schema",
          "$id": "https://ucp.dev/schemas/shopping/fulfillment.json",
          "name": "dev.ucp.shopping.fulfillment",
          "$defs": {
            "dev.ucp.shopping.catalog.search": {
              "$defs": {
                "search_request": { "$ref": "#/$defs/ful_search_request" },
                "search_response": { "$ref": "#/$defs/ful_search_response" }
              }
            },
            "ful_search_request": {
              "allOf": [
                { "$ref": "catalog_search.json#/$defs/search_request" },
                { "type": "object", "properties": { "radius_km": { "type": "number" } } }
              ]
            },
            "ful_search_response": {
              "allOf": [
                { "$ref": "catalog_search.json#/$defs/search_response" },
                {
                  "type": "object",
                  "properties": {
                    "products": { "type": "array", "items": { "$ref": "#/$defs/ful_product" } }
                  }
                }
              ]
            },
            "ful_product": {
              "allOf": [
                { "$ref": "catalog_search.json#/$defs/product" },
                {
                  "type": "object",
                  "properties": {
                    "fulfillment_methods": { "type": "array", "items": { "type": "string" } }
                  }
                }
              ]
            }
          }
        }"##,
    )
    .unwrap();

    // Single-object capability: checkout. Root IS the validatable object.
    std::fs::write(
        shopping.join("checkout.json"),
        r##"{
          "$schema": "https://json-schema.org/draft/2020-12/schema",
          "$id": "https://ucp.dev/schemas/shopping/checkout.json",
          "name": "dev.ucp.shopping.checkout",
          "type": "object",
          "required": ["id"],
          "properties": { "id": { "type": "string" } }
        }"##,
    )
    .unwrap();

    // Single-object capability with a sub-type under $defs (cart holds a
    // `checkout` def). Exercises explicit --def on a schema that HAS a root body.
    std::fs::write(
        shopping.join("cart.json"),
        r##"{
          "$schema": "https://json-schema.org/draft/2020-12/schema",
          "$id": "https://ucp.dev/schemas/shopping/cart.json",
          "name": "dev.ucp.shopping.cart",
          "type": "object",
          "required": ["id"],
          "properties": { "id": { "type": "string" } },
          "$defs": {
            "checkout": {
              "type": "object",
              "required": ["token"],
              "properties": { "token": { "type": "string" } }
            }
          }
        }"##,
    )
    .unwrap();

    // Loyalty-style extension that does NOT mirror the container (direct allOf
    // onto the response shape instead of a nested $defs of operation keys).
    std::fs::write(
        shopping.join("loyalty.json"),
        r##"{
          "$schema": "https://json-schema.org/draft/2020-12/schema",
          "$id": "https://ucp.dev/schemas/shopping/loyalty.json",
          "name": "dev.ucp.shopping.loyalty",
          "$defs": {
            "dev.ucp.shopping.catalog.search": {
              "allOf": [
                { "$ref": "catalog_search.json#/$defs/search_response" },
                { "type": "object", "properties": { "rewards": { "type": "array" } } }
              ]
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

fn search_payload(products: Value) -> Value {
    json!({
        "ucp": { "capabilities": {
            "dev.ucp.shopping.catalog.search": [
                { "version": "2026-04-08", "schema": "https://ucp.dev/schemas/shopping/catalog_search.json" }
            ]
        } },
        "products": products
    })
}

fn search_payload_with_fulfillment(products: Value) -> Value {
    json!({
        "ucp": { "capabilities": {
            "dev.ucp.shopping.catalog.search": [
                { "version": "2026-04-08", "schema": "https://ucp.dev/schemas/shopping/catalog_search.json" }
            ],
            "dev.ucp.shopping.fulfillment": [
                { "version": "2026-04-08", "schema": "https://ucp.dev/schemas/shopping/fulfillment.json", "extends": "dev.ucp.shopping.catalog.search" }
            ]
        } },
        "products": products
    })
}

// --- 1. Container validates against its selected message body ---

#[test]
fn base_container_bad_response_is_invalid() {
    let dir = tempfile::tempdir().unwrap();
    write_fixtures(dir.path());
    let cfg = config(dir.path());

    // Product missing required `title`: must be rejected by the search_response
    // body, not waved through against the body-less container root.
    let payload = search_payload(json!([{ "id": "p1", "BOGUS": true }]));
    let schema = compose_from_payload(&payload, &cfg).unwrap();
    let opts = ResolveOptions::new(Direction::Response, "search");

    let result = validate(&schema, &payload, &opts);
    assert!(
        matches!(result, Err(ValidateError::Invalid { .. })),
        "bad catalog search response must be INVALID, got {:?}",
        result
    );
}

#[test]
fn base_container_good_response_is_valid() {
    let dir = tempfile::tempdir().unwrap();
    write_fixtures(dir.path());
    let cfg = config(dir.path());

    let payload = search_payload(json!([{ "id": "p1", "title": "Widget" }]));
    let schema = compose_from_payload(&payload, &cfg).unwrap();
    let opts = ResolveOptions::new(Direction::Response, "search");

    assert!(validate(&schema, &payload, &opts).is_ok());
}

#[test]
fn base_container_request_selects_request_shape() {
    let dir = tempfile::tempdir().unwrap();
    write_fixtures(dir.path());
    let cfg = config(dir.path());

    // A response-shaped payload validated as a REQUEST must fail: search_request
    // requires `query`, not `products`.
    let payload = search_payload(json!([{ "id": "p1", "title": "Widget" }]));
    let schema = compose_from_payload(&payload, &cfg).unwrap();
    let opts = ResolveOptions::new(Direction::Request, "search");

    assert!(matches!(
        validate(&schema, &payload, &opts),
        Err(ValidateError::Invalid { .. })
    ));
}

// --- 3. A requested operation with no body is a hard error ---

#[test]
fn missing_operation_shape_fails_loud() {
    let dir = tempfile::tempdir().unwrap();
    write_fixtures(dir.path());
    let cfg = config(dir.path());

    let payload = search_payload(json!([{ "id": "p1", "title": "Widget" }]));
    let schema = compose_from_payload(&payload, &cfg).unwrap();
    // `read` is not an operation of this container -> read_response doesn't exist.
    let opts = ResolveOptions::new(Direction::Response, "read");

    let result = validate(&schema, &payload, &opts);
    match result {
        Err(ValidateError::Resolve(e)) => {
            let msg = e.to_string();
            assert!(
                msg.contains("read_response") && msg.contains("search_response"),
                "expected fail-loud naming missing + available shapes, got: {msg}"
            );
        }
        other => panic!("expected loud Resolve error, got {:?}", other),
    }
}

// --- 4. Extension composes per operation (base AND extension constraints) ---

#[test]
fn extension_good_response_is_valid() {
    let dir = tempfile::tempdir().unwrap();
    write_fixtures(dir.path());
    let cfg = config(dir.path());

    let payload = search_payload_with_fulfillment(json!([{
        "id": "p1", "title": "Widget", "fulfillment_methods": ["shipping"]
    }]));
    let schema = compose_from_payload(&payload, &cfg).unwrap();
    let opts = ResolveOptions::new(Direction::Response, "search");

    assert!(
        validate(&schema, &payload, &opts).is_ok(),
        "valid fulfillment-extended response should pass"
    );
}

#[test]
fn extension_base_constraint_still_applies() {
    let dir = tempfile::tempdir().unwrap();
    write_fixtures(dir.path());
    let cfg = config(dir.path());

    // Missing `title` (BASE constraint) even though fulfillment fields are fine.
    let payload = search_payload_with_fulfillment(json!([{
        "id": "p1", "fulfillment_methods": ["shipping"]
    }]));
    let schema = compose_from_payload(&payload, &cfg).unwrap();
    let opts = ResolveOptions::new(Direction::Response, "search");

    assert!(
        matches!(
            validate(&schema, &payload, &opts),
            Err(ValidateError::Invalid { .. })
        ),
        "base 'title' requirement must still apply under the extension"
    );
}

#[test]
fn extension_constraint_applies() {
    let dir = tempfile::tempdir().unwrap();
    write_fixtures(dir.path());
    let cfg = config(dir.path());

    // `fulfillment_methods` wrong type (EXTENSION constraint). Base fields valid.
    let payload = search_payload_with_fulfillment(json!([{
        "id": "p1", "title": "Widget", "fulfillment_methods": "NOT_AN_ARRAY"
    }]));
    let schema = compose_from_payload(&payload, &cfg).unwrap();
    let opts = ResolveOptions::new(Direction::Response, "search");

    assert!(
        matches!(
            validate(&schema, &payload, &opts),
            Err(ValidateError::Invalid { .. })
        ),
        "extension 'fulfillment_methods' type must be enforced (proves per-op merge)"
    );
}

// --- 5. Single-object capability is unaffected ---

#[test]
fn single_object_capability_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    write_fixtures(dir.path());
    let cfg = config(dir.path());

    let good = json!({
        "ucp": { "capabilities": { "dev.ucp.shopping.checkout": [
            { "version": "2026-04-08", "schema": "https://ucp.dev/schemas/shopping/checkout.json" } ] } },
        "id": "co_1"
    });
    let schema = compose_from_payload(&good, &cfg).unwrap();
    assert!(
        !is_container_schema(&schema),
        "checkout root is a single object, not a container"
    );
    let opts = ResolveOptions::new(Direction::Response, "read");
    assert!(validate(&schema, &good, &opts).is_ok());

    // Missing required `id` -> invalid (root validation, no selection).
    let bad = json!({
        "ucp": { "capabilities": { "dev.ucp.shopping.checkout": [
            { "version": "2026-04-08", "schema": "https://ucp.dev/schemas/shopping/checkout.json" } ] } }
    });
    assert!(matches!(
        validate(&schema, &bad, &opts),
        Err(ValidateError::Invalid { .. })
    ));
}

// --- 6. Non-mirroring (loyalty-style) container extension is rejected ---

#[test]
fn non_mirroring_container_extension_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    write_fixtures(dir.path());
    let cfg = config(dir.path());

    let payload = json!({
        "ucp": { "capabilities": {
            "dev.ucp.shopping.catalog.search": [
                { "version": "2026-04-08", "schema": "https://ucp.dev/schemas/shopping/catalog_search.json" } ],
            "dev.ucp.shopping.loyalty": [
                { "version": "2026-04-08", "schema": "https://ucp.dev/schemas/shopping/loyalty.json", "extends": "dev.ucp.shopping.catalog.search" } ]
        } },
        "products": []
    });

    let result = compose_from_payload(&payload, &cfg);
    assert!(
        result.is_err(),
        "a container extension that doesn't mirror operation keys must be rejected, got {:?}",
        result
    );
}

// --- select_operation_schema unit behavior ---

#[test]
fn select_is_noop_for_single_object() {
    let schema = json!({ "type": "object", "properties": { "id": { "type": "string" } } });
    let opts = ResolveOptions::new(Direction::Response, "read");
    let selected = select_operation_schema(&schema, &opts).unwrap();
    assert_eq!(
        selected, schema,
        "single-object schemas pass through unchanged"
    );
}

#[test]
fn select_wraps_container_with_ref_and_defs() {
    let schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "$defs": {
            "search_request": { "type": "object" },
            "search_response": { "type": "object", "required": ["products"] }
        }
    });
    let opts = ResolveOptions::new(Direction::Response, "search");
    let selected = select_operation_schema(&schema, &opts).unwrap();
    assert_eq!(selected["$ref"], "#/$defs/search_response");
    assert!(selected["$defs"]["search_response"].is_object());
    assert!(
        selected["$defs"]["search_request"].is_object(),
        "sibling defs retained"
    );
}

// --- explicit --def selection (the Job B / named-shape escape hatch) ---

#[test]
fn explicit_def_selects_subtype_on_schema_with_body() {
    // cart has a root body AND a `checkout` sub-type. --def must select the
    // sub-type, not fall through to the (container-check-failing) root.
    let schema = json!({
        "type": "object",
        "required": ["id"],
        "properties": { "id": { "type": "string" } },
        "$defs": { "checkout": { "type": "object", "required": ["token"] } }
    });
    let opts =
        ResolveOptions::new(Direction::Request, "create").def_name(Some("checkout".to_string()));
    let selected = select_operation_schema(&schema, &opts).unwrap();
    assert_eq!(selected["$ref"], "#/$defs/checkout");
}

#[test]
fn explicit_def_overrides_derivation() {
    // op/direction would derive search_response; --def search_request wins.
    let schema = json!({
        "type": "object",
        "$defs": {
            "search_request": { "type": "object", "required": ["query"] },
            "search_response": { "type": "object", "required": ["products"] }
        }
    });
    let opts = ResolveOptions::new(Direction::Response, "search")
        .def_name(Some("search_request".to_string()));
    let selected = select_operation_schema(&schema, &opts).unwrap();
    assert_eq!(selected["$ref"], "#/$defs/search_request");
}

#[test]
fn explicit_def_missing_is_loud() {
    let schema = json!({ "type": "object", "$defs": { "a": {}, "b": {} } });
    let opts = ResolveOptions::new(Direction::Request, "create").def_name(Some("nope".to_string()));
    let err = select_operation_schema(&schema, &opts).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("nope") && msg.contains('a') && msg.contains('b'),
        "got: {msg}"
    );
}

#[test]
fn explicit_def_validates_fragment_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    write_fixtures(dir.path());
    let cfg = config(dir.path());

    let payload = json!({
        "ucp": { "capabilities": { "dev.ucp.shopping.cart": [
            { "version": "2026-04-08", "schema": "https://ucp.dev/schemas/shopping/cart.json" } ] } },
        "token": "ok"
    });
    let schema = compose_from_payload(&payload, &cfg).unwrap();

    // Against the cart root, `token` alone is invalid (id required). Against
    // --def checkout, the same fragment is valid (token required).
    let root_opts = ResolveOptions::new(Direction::Request, "create");
    assert!(matches!(
        validate(&schema, &payload, &root_opts),
        Err(ValidateError::Invalid { .. })
    ));

    let def_opts =
        ResolveOptions::new(Direction::Request, "create").def_name(Some("checkout".to_string()));
    assert!(validate(&schema, &payload, &def_opts).is_ok());
}
