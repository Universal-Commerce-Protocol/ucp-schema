use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;
use ucp_schema::{compile_types, generate_types, CodegenError, GenerateTypesOptions};

fn fixture_schemas_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/compose/schemas")
}

fn ucp_schemas_dir() -> Option<PathBuf> {
    let candidate = Path::new(env!("CARGO_MANIFEST_DIR")).join("../ucp/source/schemas");
    candidate.exists().then_some(candidate)
}

fn visit_schema_objects(val: &Value, f: &mut impl FnMut(&serde_json::Map<String, Value>)) {
    match val {
        Value::Object(obj) => {
            f(obj);
            for (k, v) in obj {
                if !["const", "enum", "default", "examples"].contains(&k.as_str()) {
                    visit_schema_objects(v, f);
                }
            }
        }
        Value::Array(arr) => arr.iter().for_each(|v| visit_schema_objects(v, f)),
        _ => {}
    }
}

fn assert_bundle_invariants(defs: &std::collections::BTreeMap<String, Value>) {
    let known: BTreeSet<&str> = defs.keys().map(String::as_str).collect();
    for (def_name, schema_val) in defs {
        assert_eq!(
            schema_val.get("title").and_then(Value::as_str),
            Some(def_name.as_str()),
            "def '{def_name}' must have canonical title equal to its $defs key"
        );
        visit_schema_objects(schema_val, &mut |obj| {
            if let Some(r) = obj.get("$ref").and_then(Value::as_str) {
                assert!(
                    !obj.contains_key("properties"),
                    "def '{def_name}' contains hybrid $ref + inline properties node"
                );
                let target = r
                    .strip_prefix("#/$defs/")
                    .unwrap_or_else(|| panic!("def '{def_name}' contains non-local $ref: '{r}'"));
                assert!(
                    known.contains(target),
                    "def '{def_name}' contains dangling $ref '#/$defs/{target}'"
                );
            }
            if let Some(all_of) = obj.get("allOf").and_then(Value::as_array) {
                assert!(
                    all_of.iter().all(|b| b.get("title").is_none()),
                    "def '{def_name}' contains inline allOf branch title"
                );
            }
            assert!(
                !obj.keys().any(|k| {
                    k == "ucp_request"
                        || k == "ucp_response"
                        || k == "ucp_shared_request"
                        || k.starts_with("x-ucp-")
                }),
                "def '{def_name}' still contains UCP annotations"
            );
        });
    }
}

fn assert_has_defs(defs: &std::collections::BTreeMap<String, Value>, names: &[&str]) {
    for &n in names {
        assert!(defs.contains_key(n), "expected def '{n}' in bundle.defs");
    }
}

fn assert_lacks_defs(defs: &std::collections::BTreeMap<String, Value>, names: &[&str]) {
    for &n in names {
        assert!(
            !defs.contains_key(n),
            "expected def '{n}' to be excluded/pruned"
        );
    }
}

#[test]
fn missing_or_nonexistent_schema_dir_returns_expected_errors() {
    let opts = GenerateTypesOptions::default();
    let err = generate_types(&opts).expect_err("expected error when schema_dir is None");
    assert_eq!(err.exit_code(), 2);

    let missing_opts = GenerateTypesOptions::new().schema_dir("/nonexistent/ucp/schemas/path");
    let err = generate_types(&missing_opts).expect_err("expected FileNotFound error");
    assert_eq!(err.exit_code(), 3);
}

#[test]
fn unknown_capability_or_extension_returns_invalid_capability_error() {
    let bad_cap = GenerateTypesOptions::new()
        .schema_dir(fixture_schemas_dir())
        .capabilities(["dev.ucp.shopping.nonexistent"]);
    let err = generate_types(&bad_cap).expect_err("unknown capability must error");
    assert!(matches!(err, CodegenError::ComposeError(_)));
    assert_eq!(err.exit_code(), 2);

    let bad_ext = GenerateTypesOptions::new()
        .schema_dir(fixture_schemas_dir())
        .capabilities(["dev.ucp.shopping.checkout"])
        .extensions(["dev.ucp.shopping.nonexistent_ext"]);
    let err = generate_types(&bad_ext).expect_err("unknown extension must error");
    assert!(matches!(err, CodegenError::ComposeError(_)));
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn fixture_composes_extensions_and_slices_directionally() {
    let opts = GenerateTypesOptions::new()
        .schema_dir(fixture_schemas_dir())
        .capabilities(["dev.ucp.shopping.checkout"])
        .extensions(["dev.ucp.shopping.discount"])
        .title("Custom Shopping Bundle")
        .description("Test bundle");

    let bundle = generate_types(&opts).unwrap();
    assert_eq!(
        bundle.schema_dialect,
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(bundle.title, "Custom Shopping Bundle");
    assert_eq!(bundle.description.as_deref(), Some("Test bundle"));
    assert_bundle_invariants(&bundle.defs);
    assert_has_defs(
        &bundle.defs,
        &["Checkout", "CheckoutCreateRequest", "CheckoutUpdateRequest"],
    );
    assert_lacks_defs(&bundle.defs, &["Cart"]);

    assert!(bundle.defs["Checkout"]["properties"]
        .get("discounts")
        .is_some());
    assert!(bundle.defs["CheckoutCreateRequest"]["properties"]
        .get("discounts")
        .is_some());
    assert!(bundle.defs["CheckoutCreateRequest"]["properties"]
        .get("id")
        .is_none());
    assert!(bundle.defs["CheckoutUpdateRequest"]["properties"]
        .get("id")
        .is_some());
}

#[test]
fn ucp_corpus_checkout_with_discount_scoping_and_slicing() {
    let Some(schema_dir) = ucp_schemas_dir() else {
        return;
    };

    let opts = GenerateTypesOptions::new()
        .schema_dir(schema_dir)
        .capabilities(["dev.ucp.shopping.checkout"])
        .extensions(["dev.ucp.shopping.discount"]);

    let compiled = compile_types(&opts).unwrap();
    assert!(compiled
        .active_capabilities
        .contains("dev.ucp.shopping.checkout"));
    assert!(compiled.active_capabilities.contains("checkout"));
    assert!(!compiled.active_capabilities.contains("cart"));
    assert!(!compiled.active_capabilities.contains("order"));

    let bundle = generate_types(&opts).unwrap();
    assert_bundle_invariants(&bundle.defs);
    assert_has_defs(
        &bundle.defs,
        &[
            "Checkout",
            "CheckoutCreateRequest",
            "CheckoutUpdateRequest",
            "CheckoutCompleteRequest",
            "DiscountsObject",
            "DiscountsObjectCreateRequest",
            "DiscountsObjectUpdateRequest",
            "AppliedDiscount",
            "Allocation",
            "LineItem",
            "LineItemCreateRequest",
            "LineItemUpdateRequest",
            "Buyer",
            "Total",
            "ErrorResponse",
            "UcpBase",
        ],
    );
    assert_lacks_defs(
        &bundle.defs,
        &[
            "Cart",
            "CartCreateRequest",
            "Order",
            "Booking",
            "CatalogSearchRequest",
            "CatalogLookupRequest",
            "UcpBaseCreateRequest",
            "UcpBaseUpdateRequest",
        ],
    );

    assert_eq!(
        bundle.defs["Checkout"]["properties"]["discounts"]["$ref"],
        "#/$defs/DiscountsObject"
    );
    assert_eq!(
        bundle.defs["CheckoutCreateRequest"]["properties"]["discounts"]["$ref"],
        "#/$defs/DiscountsObjectCreateRequest"
    );
    assert_eq!(
        bundle.defs["CheckoutUpdateRequest"]["properties"]["discounts"]["$ref"],
        "#/$defs/DiscountsObjectUpdateRequest"
    );
    assert!(bundle.defs["CheckoutCompleteRequest"]["properties"]
        .get("discounts")
        .is_none());
    assert!(bundle.defs["DiscountsObjectCreateRequest"]["properties"]
        .get("applied")
        .is_none());
    assert!(bundle.defs["DiscountsObject"]["properties"]
        .get("applied")
        .is_some());
}

#[test]
fn ucp_corpus_multi_extension_and_container_composition() {
    let Some(schema_dir) = ucp_schemas_dir() else {
        return;
    };

    let opts = GenerateTypesOptions::new()
        .schema_dir(schema_dir)
        .capabilities([
            "dev.ucp.shopping.checkout",
            "dev.ucp.shopping.catalog.search",
            "dev.ucp.shopping.catalog.lookup",
        ])
        .extensions([
            "dev.ucp.shopping.buyer_consent",
            "dev.ucp.shopping.fulfillment",
            "dev.ucp.common.payment.terms",
            "dev.ucp.common.payment.split_payments",
            "dev.ucp.common.loyalty",
        ]);

    let bundle = generate_types(&opts).unwrap();
    assert_bundle_invariants(&bundle.defs);

    assert_eq!(
        bundle.defs["Buyer"]["properties"]["consent"]["$ref"],
        "#/$defs/Consent"
    );
    assert_has_defs(
        &bundle.defs,
        &[
            "ConsentPurpose",
            "ConsentSegment",
            "BusinessSplitPaymentsConfig",
            "PaymentSplitPaymentsBusinessSchema",
        ],
    );
    assert_lacks_defs(
        &bundle.defs,
        &[
            "ConsentPurposeCreateRequest",
            "ConsentPurposeUpdateRequest",
            "ConsentSegmentCreateRequest",
            "ConsentSegmentUpdateRequest",
            "FulfillmentSearchRequest",
            "FulfillmentSearchResponse",
            "FulfillmentLookupRequest",
            "FulfillmentLookupResponse",
            "FulfillmentGetProductRequest",
            "FulfillmentGetProductResponse",
        ],
    );
    assert!(bundle.defs["CheckoutCompleteRequest"]["properties"]
        .get("buyer")
        .is_some());

    assert!(bundle.defs["Checkout"]["properties"]["payment"]
        .get("properties")
        .is_none());
    assert_eq!(
        bundle.defs["Payment"]["properties"]["instruments"]["items"]["$ref"],
        "#/$defs/SelectedPaymentInstrument"
    );
    assert!(bundle.defs["Payment"]["properties"].get("terms").is_some());
    assert!(bundle.defs["Payment"]["properties"]
        .get("selected_term_id")
        .is_some());
    assert!(bundle.defs["PaymentCreateRequest"]["properties"]
        .get("terms")
        .is_none());
    assert!(bundle.defs["PaymentCreateRequest"]["properties"]
        .get("selected_term_id")
        .is_none());
    assert!(bundle.defs["PaymentUpdateRequest"]["properties"]
        .get("selected_term_id")
        .is_some());
    assert_eq!(
        bundle.defs["PaymentCompleteRequest"]["required"],
        serde_json::json!(["instruments"])
    );
    assert_eq!(
        bundle.defs["PaymentInstrument"]["properties"]["amount"]["$ref"],
        "#/$defs/Amount"
    );

    assert_eq!(
        bundle.defs["CatalogSearchRequest"]["properties"]["filters"]["$ref"],
        "#/$defs/FulfillmentSearchFilters"
    );
    assert_eq!(
        bundle.defs["CatalogSearchResponse"]["properties"]["products"]["items"]["$ref"],
        "#/$defs/FulfillmentProduct"
    );
    assert_eq!(
        bundle.defs["CatalogSearchResponse"]["properties"]["loyalty"]["$ref"],
        "#/$defs/Loyalty"
    );
    assert_eq!(
        bundle.defs["CatalogLookupResponse"]["properties"]["loyalty"]["$ref"],
        "#/$defs/Loyalty"
    );
    assert_eq!(
        bundle.defs["CatalogGetProductResponse"]["properties"]["loyalty"]["$ref"],
        "#/$defs/Loyalty"
    );
    assert_eq!(
        bundle.defs["CatalogGetProductResponse"]["properties"]["product"]["$ref"],
        "#/$defs/FulfillmentDetailProduct"
    );
    assert!(bundle.defs["CatalogLookupRequest"]["properties"]
        .get("loyalty")
        .is_none());
}

#[test]
fn ucp_corpus_full_compilation_has_zero_dangling_refs_or_annotations() {
    let Some(schema_dir) = ucp_schemas_dir() else {
        return;
    };

    let bundle = generate_types(&GenerateTypesOptions::new().schema_dir(schema_dir)).unwrap();
    assert_bundle_invariants(&bundle.defs);

    assert_has_defs(
        &bundle.defs,
        &[
            "Provider",
            "ScopePolicy",
            "ScopeToken",
            "IdentityLinkingPlatformSchema",
            "IdentityLinkingBusinessSchema",
            "PermalinkEndpoint",
            "PermalinkConfig",
            "PermalinkPlatformSchema",
            "PermalinkBusinessSchema",
            "PermalinkResponseSchema",
            "FulfillmentPlatformSchema",
            "FulfillmentBusinessSchema",
            "PaymentSplitPaymentsBusinessSchema",
            "ErrorCode",
            "PaymentAp2MandateErrorCode",
            "ErrorResponse",
            "JsonrpcErrorResponse",
            "Message",
            "A2aMessageMessage",
            "Actions",
            "Instance",
            "Policy",
            "CancellationItem",
            "CatalogSearchRequest",
            "CatalogSearchResponse",
            "CatalogLookupRequest",
            "CatalogLookupResponse",
            "CatalogGetProductRequest",
            "CatalogGetProductResponse",
            "LocationSearchRequest",
            "LocationSearchResponse",
            "LocationLookupRequest",
            "LocationLookupResponse",
        ],
    );
    assert_lacks_defs(
        &bundle.defs,
        &[
            "PaymentActions",
            "FulfillmentSearchRequest",
            "FulfillmentSearchResponse",
            "FulfillmentLookupRequest",
            "FulfillmentLookupResponse",
            "FulfillmentGetProductRequest",
            "FulfillmentGetProductResponse",
            "LocationCreateRequest",
            "LocationUpdateRequest",
            "DailyHourCreateRequest",
            "DailyHourUpdateRequest",
            "ExceptionHourCreateRequest",
            "ExceptionHourUpdateRequest",
            "StayCompleteRequest",
            "TokenCredentialCreateRequest",
            "TokenCredentialUpdateRequest",
            "TokenCredentialCompleteRequest",
        ],
    );

    assert!(bundle.defs["Checkout"]["properties"]["actions"]
        .get("properties")
        .is_none());
    assert_eq!(
        bundle.defs["Checkout"]["properties"]["actions"]["$ref"],
        "#/$defs/Actions"
    );
    assert!(bundle.defs["Actions"]["properties"]
        .get("dev.ucp.common.payment.device_data_collection")
        .is_some());
    assert!(bundle.defs["Actions"]["properties"]
        .get("dev.ucp.common.payment.three_ds_challenge")
        .is_some());

    assert_eq!(
        bundle.defs["Booking"]["properties"]["policies"]["items"],
        serde_json::json!({ "$ref": "#/$defs/Policy" })
    );
    assert_eq!(
        bundle.defs["Policy"]["allOf"][0]["then"]["$ref"],
        "#/$defs/CancellationItem"
    );

    let complete_ap2_allof = bundle.defs["CheckoutCompleteRequest"]["properties"]["ap2"]["allOf"]
        .as_array()
        .expect("CheckoutCompleteRequest.properties.ap2.allOf must be an array");
    assert_eq!(complete_ap2_allof.len(), 1);
    assert_eq!(
        complete_ap2_allof[0]["$ref"],
        "#/$defs/Ap2WithCheckoutMandateCompleteRequest"
    );
}

#[test]
fn ucp_corpus_reclassification_and_cart_checkout_overlay() {
    let Some(schema_dir) = ucp_schemas_dir() else {
        return;
    };

    let checkout_only = generate_types(
        &GenerateTypesOptions::new()
            .schema_dir(&schema_dir)
            .capabilities(["dev.ucp.shopping.checkout"]),
    )
    .unwrap();
    assert_bundle_invariants(&checkout_only.defs);
    assert!(checkout_only.defs["Checkout"]["properties"]
        .get("discounts")
        .is_none());
    assert!(checkout_only.defs["Checkout"]["properties"]
        .get("fulfillment")
        .is_none());

    let reclassified = generate_types(
        &GenerateTypesOptions::new()
            .schema_dir(&schema_dir)
            .capabilities(["dev.ucp.shopping.checkout", "dev.ucp.shopping.fulfillment"]),
    )
    .unwrap();
    assert_bundle_invariants(&reclassified.defs);
    assert_eq!(
        reclassified.defs["Checkout"]["properties"]["fulfillment"]["$ref"],
        "#/$defs/Fulfillment"
    );
    assert!(reclassified.defs["Checkout"]["properties"]
        .get("discounts")
        .is_none());

    let cart_only = generate_types(
        &GenerateTypesOptions::new()
            .schema_dir(&schema_dir)
            .capabilities(["dev.ucp.shopping.cart"]),
    )
    .unwrap();
    assert_bundle_invariants(&cart_only.defs);
    assert_has_defs(&cart_only.defs, &["Cart"]);
    assert_lacks_defs(&cart_only.defs, &["Checkout"]);

    let cart_and_checkout = generate_types(
        &GenerateTypesOptions::new()
            .schema_dir(&schema_dir)
            .capabilities(["dev.ucp.shopping.cart", "dev.ucp.shopping.checkout"]),
    )
    .unwrap();
    assert_bundle_invariants(&cart_and_checkout.defs);
    assert!(
        cart_and_checkout.defs["CheckoutCreateRequest"]["properties"]
            .get("cart_id")
            .is_some()
    );

    let err = generate_types(
        &GenerateTypesOptions::new()
            .schema_dir(&schema_dir)
            .capabilities(["checkout"]),
    )
    .expect_err("short capability names must be rejected");
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn cli_generate_types_stdout_pretty_and_compact() {
    let bin = env!("CARGO_BIN_EXE_ucp-schema");
    let schema_dir = fixture_schemas_dir();

    // 1. Default --pretty is true (multi-line JSON to stdout)
    let out = std::process::Command::new(bin)
        .args([
            "generate-types",
            "-s",
            schema_dir.to_str().unwrap(),
            "--capability",
            "dev.ucp.shopping.checkout",
            "--extension",
            "dev.ucp.shopping.discount",
        ])
        .output()
        .expect("run ucp-schema generate-types");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.lines().count() > 5,
        "expected pretty multi-line JSON by default"
    );
    let doc: ucp_schema::TypesBundleDoc = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        doc.schema_dialect,
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(doc.title, "UCP Schema Types");
    assert_bundle_invariants(&doc.defs);
    assert!(doc.defs.contains_key("Checkout"));
    assert!(doc.defs.contains_key("CheckoutCreateRequest"));
    assert!(doc.defs["Checkout"]["properties"]
        .get("discounts")
        .is_some());
    assert!(!doc.defs.contains_key("Cart"));

    // 2. --pretty=false emits compact single-line JSON, and --capabilities/--extensions aliases work
    let compact_out = std::process::Command::new(bin)
        .args([
            "generate-types",
            "--schema-dir",
            schema_dir.to_str().unwrap(),
            "--capabilities",
            "dev.ucp.shopping.checkout,dev.ucp.shopping.cart",
            "--extensions",
            "dev.ucp.shopping.discount",
            "--pretty=false",
        ])
        .output()
        .expect("run ucp-schema generate-types --pretty=false");
    assert!(
        compact_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&compact_out.stderr)
    );
    let compact_stdout = String::from_utf8(compact_out.stdout).unwrap();
    assert_eq!(
        compact_stdout.trim().lines().count(),
        1,
        "expected single-line JSON with --pretty=false"
    );
    let compact_doc: ucp_schema::TypesBundleDoc = serde_json::from_str(&compact_stdout).unwrap();
    assert_bundle_invariants(&compact_doc.defs);
    assert!(compact_doc.defs.contains_key("Checkout"));
    assert!(compact_doc.defs.contains_key("Cart"));
}

#[test]
fn cli_generate_types_output_file_and_repeatable_flags() {
    let bin = env!("CARGO_BIN_EXE_ucp-schema");
    let schema_dir = fixture_schemas_dir();
    let tmp = tempfile::tempdir().unwrap();
    let output_path = tmp.path().join("bundle.types.json");

    let out = std::process::Command::new(bin)
        .args([
            "generate-types",
            "--schema-dir",
            schema_dir.to_str().unwrap(),
            "--capability",
            "dev.ucp.shopping.checkout",
            "--capability",
            "dev.ucp.shopping.cart",
            "--extension",
            "dev.ucp.shopping.discount",
            "--pretty",
            "-o",
            output_path.to_str().unwrap(),
        ])
        .output()
        .expect("run ucp-schema generate-types -o");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "stdout should be empty when -o is used"
    );

    let file_contents = std::fs::read_to_string(&output_path).unwrap();
    let doc: ucp_schema::TypesBundleDoc = serde_json::from_str(&file_contents).unwrap();
    assert_bundle_invariants(&doc.defs);
    assert!(doc.defs.contains_key("Checkout"));
    assert!(doc.defs.contains_key("Cart"));
    assert!(doc.defs["Checkout"]["properties"]
        .get("discounts")
        .is_some());
}

#[test]
fn cli_generate_types_error_exit_codes() {
    let bin = env!("CARGO_BIN_EXE_ucp-schema");
    let schema_dir = fixture_schemas_dir();

    // 1. Missing --schema-dir without --profile -> exit code 2
    let missing_dir = std::process::Command::new(bin)
        .arg("generate-types")
        .output()
        .unwrap();
    assert_eq!(missing_dir.status.code(), Some(2));

    // 2. Nonexistent --schema-dir -> exit code 3
    let bad_dir = std::process::Command::new(bin)
        .args(["generate-types", "--schema-dir", "/nonexistent/ucp/schemas"])
        .output()
        .unwrap();
    assert_eq!(bad_dir.status.code(), Some(3));

    // 3. Unknown capability -> exit code 2
    let bad_cap = std::process::Command::new(bin)
        .args([
            "generate-types",
            "--schema-dir",
            schema_dir.to_str().unwrap(),
            "--capability",
            "dev.ucp.shopping.nonexistent",
        ])
        .output()
        .unwrap();
    assert_eq!(bad_cap.status.code(), Some(2));

    // 4. Unwritable --output path -> exit code 3
    let bad_out = std::process::Command::new(bin)
        .args([
            "generate-types",
            "--schema-dir",
            schema_dir.to_str().unwrap(),
            "--output",
            "/nonexistent/dir/bundle.json",
        ])
        .output()
        .unwrap();
    assert_eq!(bad_out.status.code(), Some(3));
}

#[test]
fn profile_mode_matches_directory_mode_closure_and_prunes_orphaned_extensions() {
    let Some(schema_dir) = ucp_schemas_dir() else {
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let profile_path = tmp.path().join("profile.json");
    let s = |rel: &str| schema_dir.join(rel).to_string_lossy().into_owned();

    let profile_json = serde_json::json!({
        "ucp": {
            "version": "2026-08-25",
            "services": {
                "dev.ucp.shopping": [{
                    "version": "2026-08-25",
                    "transport": "rest",
                    "endpoint": "https://merchant.example.com/ucp",
                    "schema": "https://ucp.dev/2026-08-25/services/shopping/rest.openapi.json"
                }]
            },
            "capabilities": {
                "dev.ucp.shopping.checkout": [{ "version": "2026-08-25", "schema": s("shopping/checkout.json") }],
                "dev.ucp.shopping.discount": [{
                    "version": "2026-08-25",
                    "schema": s("shopping/discount.json"),
                    "extends": ["dev.ucp.shopping.checkout", "dev.ucp.shopping.cart"]
                }],
                "dev.ucp.shopping.fulfillment": [{
                    "version": "2026-08-25",
                    "schema": s("shopping/fulfillment.json"),
                    "extends": ["dev.ucp.shopping.checkout", "dev.ucp.shopping.order"]
                }],
                "dev.ucp.lodging.policy.cancellation": [{
                    "version": "2026-08-25",
                    "schema": s("lodging/policy_cancellation.json"),
                    "extends": "dev.ucp.lodging.booking"
                }]
            }
        }
    });
    std::fs::write(&profile_path, profile_json.to_string()).unwrap();

    let parsed = ucp_schema::parse_profile_source(profile_path.to_str().unwrap()).unwrap();
    assert_eq!(parsed.capabilities.len(), 3);
    assert_eq!(parsed.rest_services.len(), 1);
    assert_eq!(
        parsed.rest_services[0].endpoint.as_deref(),
        Some("https://merchant.example.com/ucp")
    );

    let profile_bundle =
        generate_types(&GenerateTypesOptions::new().profile(profile_path.to_str().unwrap()))
            .unwrap();
    assert_bundle_invariants(&profile_bundle.defs);
    assert_lacks_defs(&profile_bundle.defs, &["CancellationItem", "Booking"]);

    let directory_bundle = generate_types(
        &GenerateTypesOptions::new()
            .schema_dir(&schema_dir)
            .capabilities(["dev.ucp.shopping.checkout"])
            .extensions(["dev.ucp.shopping.discount", "dev.ucp.shopping.fulfillment"]),
    )
    .unwrap();

    assert_eq!(
        profile_bundle.defs, directory_bundle.defs,
        "--profile mode must produce the exact same $defs bundle as equivalent --capability/--extension flags"
    );
}

#[test]
fn profile_mode_allbirds_multi_origin_shopify_catalog_extension() {
    let Some(schema_dir) = ucp_schemas_dir() else {
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let shopify_catalog_path = tmp.path().join("shopify_catalog.json");
    let ucp_url = |rel: &str| format!("https://ucp.dev/2026-08-25/schemas/{rel}");

    let shopify_catalog_schema = serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://shopify.dev/ucp/schemas/2026-08-25/shopify_catalog.json",
        "name": "dev.shopify.catalog",
        "version": "2026-08-25",
        "title": "Shopify Catalog Extensions",
        "$defs": {
            "collection": {
                "type": "object",
                "required": ["id", "handle", "title", "description"],
                "properties": {
                    "id": { "type": "string" },
                    "handle": { "type": "string" },
                    "title": { "type": "string" },
                    "description": { "$ref": ucp_url("common/types/description.json") },
                    "media": { "$ref": ucp_url("common/types/media.json") }
                }
            },
            "selling_plan_price": {
                "type": "object",
                "required": ["total"],
                "properties": { "total": { "$ref": ucp_url("common/types/price.json") } }
            },
            "selling_plan": {
                "type": "object",
                "required": ["id", "name", "price"],
                "properties": {
                    "id": { "type": "string" },
                    "name": { "type": "string" },
                    "price": { "$ref": "#/$defs/selling_plan_price" }
                }
            },
            "shopify_variant": {
                "title": "Shopify Variant",
                "allOf": [
                    { "$ref": ucp_url("shopping/types/variant.json") },
                    {
                        "type": "object",
                        "properties": {
                            "checkout_url": { "type": "string", "format": "uri" },
                            "selling_plans": {
                                "type": "array",
                                "items": { "$ref": "#/$defs/selling_plan" },
                                "ucp_response": {
                                    "transition": {
                                        "from": "omit",
                                        "to": "optional",
                                        "description": "Planned: subscription selling plans."
                                    }
                                }
                            }
                        }
                    }
                ]
            },
            "shopify_product": {
                "title": "Shopify Product",
                "allOf": [
                    { "$ref": ucp_url("shopping/types/product.json") },
                    {
                        "type": "object",
                        "properties": {
                            "variants": { "type": "array", "items": { "$ref": "#/$defs/shopify_variant" } }
                        }
                    }
                ]
            },
            "storefront_variant": {
                "title": "Storefront Variant",
                "allOf": [{ "$ref": "#/$defs/shopify_variant" }]
            },
            "storefront_product": {
                "title": "Storefront Product",
                "allOf": [
                    { "$ref": "#/$defs/shopify_product" },
                    {
                        "type": "object",
                        "properties": {
                            "gift_card": { "type": "boolean" },
                            "collections": { "type": "array", "items": { "$ref": "#/$defs/collection" } },
                            "variants": { "type": "array", "items": { "$ref": "#/$defs/storefront_variant" } }
                        }
                    }
                ]
            },
            "shopify_filters": {
                "type": "object",
                "title": "Shopify Filters",
                "properties": { "available": { "type": "boolean", "default": true } }
            },
            "shopify_search_filters": {
                "title": "Shopify Search Filters",
                "allOf": [
                    { "$ref": ucp_url("shopping/types/search_filters.json") },
                    { "$ref": "#/$defs/shopify_filters" }
                ]
            },
            "shopify_search_request": {
                "allOf": [
                    { "$ref": ucp_url("shopping/catalog_search.json#/$defs/search_request") },
                    { "type": "object", "properties": { "filters": { "$ref": "#/$defs/shopify_search_filters" } } }
                ]
            },
            "shopify_search_response": {
                "allOf": [
                    { "$ref": ucp_url("shopping/catalog_search.json#/$defs/search_response") },
                    { "type": "object", "properties": { "products": { "type": "array", "items": { "$ref": "#/$defs/shopify_product" } } } }
                ]
            },
            "shopify_lookup_request": {
                "allOf": [
                    { "$ref": ucp_url("shopping/catalog_lookup.json#/$defs/lookup_request") },
                    { "type": "object", "properties": { "filters": { "$ref": "#/$defs/shopify_filters" } } }
                ]
            },
            "shopify_lookup_response": {
                "allOf": [
                    { "$ref": ucp_url("shopping/catalog_lookup.json#/$defs/lookup_response") },
                    { "type": "object", "properties": { "products": { "type": "array", "items": { "$ref": "#/$defs/shopify_product" } } } }
                ]
            },
            "shopify_get_product_request": {
                "allOf": [{ "$ref": ucp_url("shopping/catalog_lookup.json#/$defs/get_product_request") }]
            },
            "shopify_get_product_response": {
                "allOf": [
                    { "$ref": ucp_url("shopping/catalog_lookup.json#/$defs/get_product_response") },
                    { "type": "object", "properties": { "product": { "$ref": "#/$defs/shopify_product" } } }
                ]
            },
            "storefront_search_request": {
                "allOf": [{ "$ref": "#/$defs/shopify_search_request" }],
                "required": ["query"]
            },
            "storefront_search_response": {
                "allOf": [
                    { "$ref": "#/$defs/shopify_search_response" },
                    { "type": "object", "properties": { "products": { "type": "array", "items": { "$ref": "#/$defs/storefront_product" } } } }
                ]
            },
            "storefront_lookup_request": {
                "allOf": [{ "$ref": "#/$defs/shopify_lookup_request" }]
            },
            "storefront_lookup_response": {
                "allOf": [
                    { "$ref": "#/$defs/shopify_lookup_response" },
                    { "type": "object", "properties": { "products": { "type": "array", "items": { "$ref": "#/$defs/storefront_product" } } } }
                ]
            },
            "storefront_get_product_request": {
                "allOf": [{ "$ref": "#/$defs/shopify_get_product_request" }]
            },
            "storefront_get_product_response": {
                "allOf": [
                    { "$ref": "#/$defs/shopify_get_product_response" },
                    { "type": "object", "properties": { "product": { "$ref": "#/$defs/storefront_product" } } }
                ]
            },
            "dev.ucp.shopping.catalog.search": {
                "$defs": {
                    "search_request": { "$ref": "#/$defs/storefront_search_request" },
                    "search_response": { "$ref": "#/$defs/storefront_search_response" }
                }
            },
            "dev.ucp.shopping.catalog.lookup": {
                "$defs": {
                    "lookup_request": { "$ref": "#/$defs/storefront_lookup_request" },
                    "lookup_response": { "$ref": "#/$defs/storefront_lookup_response" },
                    "get_product_request": { "$ref": "#/$defs/storefront_get_product_request" },
                    "get_product_response": { "$ref": "#/$defs/storefront_get_product_response" }
                }
            }
        }
    });
    std::fs::write(&shopify_catalog_path, shopify_catalog_schema.to_string()).unwrap();

    let allbirds_profile_path = tmp.path().join("allbirds_profile.json");
    let s = |rel: &str| schema_dir.join(rel).to_string_lossy().into_owned();
    let allbirds_profile = serde_json::json!({
        "ucp": {
            "version": "2026-08-25",
            "services": {
                "dev.ucp.shopping": [{
                    "version": "2026-08-25",
                    "transport": "mcp",
                    "endpoint": "https://weareallbirds.myshopify.com/api/ucp/mcp",
                    "schema": "https://ucp.dev/2026-08-25/services/shopping/mcp.openrpc.json"
                }]
            },
            "capabilities": {
                "dev.ucp.shopping.checkout": [{ "version": "2026-08-25", "schema": s("shopping/checkout.json") }],
                "dev.ucp.shopping.fulfillment": [{
                    "version": "2026-08-25",
                    "schema": s("shopping/fulfillment.json"),
                    "extends": ["dev.ucp.shopping.checkout", "dev.ucp.shopping.order"]
                }],
                "dev.ucp.shopping.discount": [{
                    "version": "2026-08-25",
                    "schema": s("shopping/discount.json"),
                    "extends": ["dev.ucp.shopping.checkout", "dev.ucp.shopping.cart"]
                }],
                "dev.ucp.shopping.cart": [{ "version": "2026-08-25", "schema": s("shopping/cart.json") }],
                "dev.ucp.shopping.order": [{ "version": "2026-08-25", "schema": s("shopping/order.json") }],
                "dev.ucp.shopping.catalog.search": [{ "version": "2026-08-25", "schema": s("shopping/catalog_search.json") }],
                "dev.ucp.shopping.catalog.lookup": [{ "version": "2026-08-25", "schema": s("shopping/catalog_lookup.json") }],
                "dev.shopify.catalog": [{
                    "version": "2026-08-25",
                    "schema": shopify_catalog_path.to_str().unwrap(),
                    "extends": ["dev.ucp.shopping.catalog.search", "dev.ucp.shopping.catalog.lookup"]
                }],
                "dev.ucp.common.identity_linking": [{ "version": "2026-08-25", "schema": s("common/identity_linking.json") }],
                "dev.ucp.shopping.permalink": [{ "version": "2026-08-25", "schema": s("shopping/permalink.json") }]
            }
        }
    });
    std::fs::write(&allbirds_profile_path, allbirds_profile.to_string()).unwrap();

    let bundle = generate_types(
        &GenerateTypesOptions::new().profile(allbirds_profile_path.to_str().unwrap()),
    )
    .unwrap();
    assert_bundle_invariants(&bundle.defs);

    assert_has_defs(
        &bundle.defs,
        &[
            "Checkout",
            "CheckoutCreateRequest",
            "Cart",
            "Order",
            "CatalogSearchRequest",
            "CatalogSearchResponse",
            "CatalogLookupRequest",
            "CatalogLookupResponse",
            "CatalogGetProductRequest",
            "CatalogGetProductResponse",
            "StorefrontProduct",
            "StorefrontVariant",
            "ShopifyProduct",
            "ShopifyVariant",
            "Collection",
            "SellingPlan",
            "SellingPlanPrice",
            "ShopifyFilters",
            "ShopifySearchFilters",
            "IdentityLinkingPlatformSchema",
            "PermalinkPlatformSchema",
            "ErrorResponse",
        ],
    );
    assert_lacks_defs(
        &bundle.defs,
        &[
            "Booking",
            "StorefrontSearchRequest",
            "ShopifySearchRequest",
            "StorefrontSearchResponse",
            "ShopifySearchResponse",
            "StorefrontLookupRequest",
            "ShopifyLookupRequest",
            "StorefrontLookupResponse",
            "ShopifyLookupResponse",
            "StorefrontGetProductRequest",
            "ShopifyGetProductRequest",
            "StorefrontGetProductResponse",
            "ShopifyGetProductResponse",
        ],
    );

    assert_eq!(
        bundle.defs["CatalogSearchRequest"]["properties"]["filters"]["$ref"],
        "#/$defs/ShopifySearchFilters"
    );
    assert!(bundle.defs["CatalogSearchRequest"]["required"]
        .as_array()
        .unwrap()
        .contains(&Value::String("query".to_string())));
    assert_eq!(
        bundle.defs["CatalogSearchResponse"]["properties"]["products"]["items"]["$ref"],
        "#/$defs/StorefrontProduct"
    );
    assert_eq!(
        bundle.defs["CatalogLookupResponse"]["properties"]["products"]["items"]["$ref"],
        "#/$defs/StorefrontProduct"
    );
    assert_eq!(
        bundle.defs["CatalogGetProductResponse"]["properties"]["product"]["$ref"],
        "#/$defs/StorefrontProduct"
    );
}

#[test]
fn cli_generate_types_profile_mode_orphaned_warning_and_flag_conflicts() {
    let bin = env!("CARGO_BIN_EXE_ucp-schema");
    let schema_dir = fixture_schemas_dir();
    let tmp = tempfile::tempdir().unwrap();
    let profile_path = tmp.path().join("profile.json");
    let s = |rel: &str| schema_dir.join(rel).to_string_lossy().into_owned();

    let profile_json = serde_json::json!({
        "ucp": {
            "capabilities": {
                "dev.ucp.shopping.checkout": [{ "version": "2026-01-11", "schema": s("shopping/checkout.json") }],
                "dev.ucp.shopping.discount": [{
                    "version": "2026-01-11",
                    "schema": s("shopping/discount.json"),
                    "extends": "dev.ucp.shopping.checkout"
                }],
                "dev.ucp.shopping.fulfillment": [{
                    "version": "2026-01-11",
                    "schema": s("shopping/fulfillment.json"),
                    "extends": "dev.ucp.shopping.order"
                }]
            }
        }
    });
    std::fs::write(&profile_path, profile_json.to_string()).unwrap();

    // 1. Valid --profile execution emits orphaned extension warning to stderr and valid bundle to stdout
    let out = std::process::Command::new(bin)
        .args([
            "generate-types",
            "--profile",
            profile_path.to_str().unwrap(),
        ])
        .output()
        .expect("run ucp-schema generate-types --profile");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("warning: pruning orphaned extension 'dev.ucp.shopping.fulfillment'"),
        "expected orphaned extension warning on stderr, got: {stderr}"
    );
    let doc: ucp_schema::TypesBundleDoc = serde_json::from_slice(&out.stdout).unwrap();
    assert_bundle_invariants(&doc.defs);
    assert_has_defs(
        &doc.defs,
        &["Checkout", "CheckoutCreateRequest", "CheckoutUpdateRequest"],
    );
    assert!(doc.defs["Checkout"]["properties"]
        .get("discounts")
        .is_some());
    assert!(doc.defs["Checkout"]["properties"]
        .get("fulfillment")
        .is_none());

    // 2. --profile conflicts with --schema-dir, --capability, and --extension (exit code 2)
    for extra_args in [
        ["--schema-dir", schema_dir.to_str().unwrap()],
        ["--capability", "dev.ucp.shopping.checkout"],
        ["--extension", "dev.ucp.shopping.discount"],
    ] {
        let conflict = std::process::Command::new(bin)
            .args([
                "generate-types",
                "--profile",
                profile_path.to_str().unwrap(),
            ])
            .args(extra_args)
            .output()
            .unwrap();
        assert_eq!(
            conflict.status.code(),
            Some(2),
            "expected clap conflict exit code 2 for {extra_args:?}"
        );
    }
}

#[test]
fn hoist_inline_conditional_variants_and_ast_normalizers_on_ucp_corpus() {
    let Some(schema_dir) = ucp_schemas_dir() else {
        return;
    };

    let bundle = generate_types(&GenerateTypesOptions::new().schema_dir(schema_dir)).unwrap();
    assert_bundle_invariants(&bundle.defs);

    // 1. Pre-slicing inline variant hoisting on FulfillmentMethod -> ShippingMethod, PickupMethod
    assert_has_defs(
        &bundle.defs,
        &[
            "ShippingMethod",
            "ShippingMethodCreateRequest",
            "ShippingMethodUpdateRequest",
            "PickupMethod",
            "PickupMethodCreateRequest",
            "PickupMethodUpdateRequest",
            "Oauth2Provider",
        ],
    );
    assert_eq!(
        bundle.defs["ShippingMethodCreateRequest"]["properties"]["destinations"]["items"]["$ref"],
        "#/$defs/ShippingDestinationCreateRequest"
    );
    assert!(
        bundle.defs["PickupMethodCreateRequest"]["properties"]
            .get("destinations")
            .is_none(),
        "PickupMethodCreateRequest must omit response-only destinations"
    );
    assert_eq!(
        bundle.defs["Oauth2Provider"]["properties"]["type"]["const"],
        "oauth2"
    );
    assert!(bundle.defs["Oauth2Provider"]["properties"]
        .get("auth_url")
        .is_some());

    // 2. Scalar value constraints in Total are NOT hoisted into variant classes
    assert_lacks_defs(
        &bundle.defs,
        &[
            "DiscountTotal",
            "SubtotalTotal",
            "ItemsDiscountTotal",
            "FulfillmentTotal",
        ],
    );

    // 3. Totals strips top-level contains allOf and flattens items.allOf
    assert!(bundle.defs["Totals"].get("allOf").is_none());
    assert_eq!(bundle.defs["Totals"]["items"]["type"], "object");
    assert!(bundle.defs["Totals"]["items"]["properties"]
        .get("amount")
        .is_some());
    assert!(bundle.defs["Totals"]["items"]["properties"]
        .get("lines")
        .is_some());

    // 4. Bare anyOf property distribution on ValueConstraint and StayCreateRequest
    assert!(bundle.defs["ValueConstraint"].get("properties").is_none());
    assert_eq!(bundle.defs["ValueConstraint"]["anyOf"][0]["type"], "object");
    assert!(bundle.defs["ValueConstraint"]["anyOf"][0]["properties"]
        .get("enum")
        .is_some());
    assert!(bundle.defs["StayCreateRequest"].get("properties").is_none());
    assert_eq!(
        bundle.defs["StayCreateRequest"]["anyOf"][1]["properties"]["accommodation_type"]["$ref"],
        "#/$defs/AccommodationTypeCreateRequest"
    );
    assert!(
        bundle.defs["StayCreateRequest"]["anyOf"][1]["properties"]["accommodation_type"]
            .get("required")
            .is_none(),
        "base $ref property must stay isolated without sibling required keys"
    );

    // 5. Scalar-or-array union normalization on CapabilityBase.properties.extends
    assert!(bundle.defs["CapabilityBase"]["properties"]["extends"]
        .get("oneOf")
        .is_none());
    assert_eq!(
        bundle.defs["CapabilityBase"]["properties"]["extends"]["type"],
        "array"
    );
    assert_eq!(
        bundle.defs["CapabilityBase"]["properties"]["extends"]["items"]["$ref"],
        "#/$defs/ReverseDomainName"
    );

    // 6. Single-object allOf flattening on ShippingDestination, LocationDestination, CardPaymentInstrument
    for def_name in [
        "ShippingDestination",
        "ShippingDestinationCreateRequest",
        "LocationDestination",
        "CardPaymentInstrument",
        "UcpPlatformSchema",
    ] {
        assert!(
            bundle.defs[def_name].get("allOf").is_none(),
            "expected {def_name}.allOf to be flattened"
        );
        assert_eq!(bundle.defs[def_name]["type"], "object");
    }
    assert!(bundle.defs["ShippingDestination"]["properties"]
        .get("street_address")
        .is_some());
    assert_eq!(
        bundle.defs["UcpPlatformSchema"]["properties"]["services"]["type"],
        "object"
    );
    assert_eq!(
        bundle.defs["UcpPlatformSchema"]["properties"]["services"]["additionalProperties"]["items"]
            ["$ref"],
        "#/$defs/ServicePlatformSchema"
    );
}
