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
