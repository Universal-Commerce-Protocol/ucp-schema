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

const INSTANCE_DATA_KEYS: &[&str] = &["const", "enum", "default", "examples"];

fn collect_schema_refs_and_annotations(
    val: &Value,
    refs: &mut Vec<String>,
    annotations: &mut Vec<String>,
) {
    let Some(obj) = val.as_object() else {
        if let Some(arr) = val.as_array() {
            for item in arr {
                collect_schema_refs_and_annotations(item, refs, annotations);
            }
        }
        return;
    };

    if let Some(Value::String(r)) = obj.get("$ref") {
        refs.push(r.clone());
    }
    for key in obj.keys() {
        if key == "ucp_request"
            || key == "ucp_response"
            || key == "ucp_shared_request"
            || key.starts_with("x-ucp-")
        {
            annotations.push(key.clone());
        }
    }

    for (k, v) in obj {
        if INSTANCE_DATA_KEYS.contains(&k.as_str()) {
            continue;
        }
        if k == "properties" || k == "$defs" || k == "definitions" || k == "patternProperties" {
            if let Some(map) = v.as_object() {
                for child in map.values() {
                    collect_schema_refs_and_annotations(child, refs, annotations);
                }
            }
            continue;
        }
        collect_schema_refs_and_annotations(v, refs, annotations);
    }
}

fn assert_bundle_invariants(defs: &std::collections::BTreeMap<String, Value>) {
    let known: BTreeSet<&str> = defs.keys().map(String::as_str).collect();
    for (def_name, schema_val) in defs {
        let mut refs = Vec::new();
        let mut annotations = Vec::new();
        collect_schema_refs_and_annotations(schema_val, &mut refs, &mut annotations);

        assert!(
            annotations.is_empty(),
            "def '{def_name}' still contains UCP annotations: {annotations:?}"
        );

        for r in refs {
            let Some(target) = r.strip_prefix("#/$defs/") else {
                panic!("def '{def_name}' contains non-local $ref: '{r}'");
            };
            assert!(
                known.contains(target),
                "def '{def_name}' contains dangling $ref '#/$defs/{target}'"
            );
        }
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

    assert!(bundle.defs.contains_key("Checkout"));
    assert!(bundle.defs.contains_key("CheckoutCreateRequest"));
    assert!(bundle.defs.contains_key("CheckoutUpdateRequest"));
    assert!(!bundle.defs.contains_key("Cart"));

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

    for expected in [
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
    ] {
        assert!(
            bundle.defs.contains_key(expected),
            "expected def '{expected}' in bundle.defs"
        );
    }

    for excluded in [
        "Cart",
        "CartCreateRequest",
        "Order",
        "Booking",
        "CatalogSearchRequest",
        "CatalogLookupRequest",
    ] {
        assert!(
            !bundle.defs.contains_key(excluded),
            "expected inactive capability def '{excluded}' to be excluded"
        );
    }

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
    assert!(
        bundle.defs["CheckoutCompleteRequest"]["properties"]
            .get("discounts")
            .is_none(),
        "discounts has complete: omit in discount.json"
    );
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

    // 1. BuyerConsent composed in-place into Buyer and overrides CheckoutCompleteRequest.buyer
    assert_eq!(
        bundle.defs["Buyer"]["properties"]["consent"]["$ref"],
        "#/$defs/Consent"
    );
    assert!(bundle.defs.contains_key("ConsentPurpose"));
    assert!(bundle.defs.contains_key("ConsentPurposeCreateRequest"));
    assert!(bundle.defs["ConsentPurposeCreateRequest"]["properties"]
        .get("description")
        .is_none());
    assert!(
        bundle.defs["CheckoutCompleteRequest"]["properties"]
            .get("buyer")
            .is_some(),
        "buyer_consent sets buyer.ucp_request.complete = optional"
    );

    // 2. PaymentTerms composed in-place into Payment and sliced directionally
    assert!(bundle.defs["Payment"]["properties"]
        .get("instruments")
        .is_some());
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

    // 3. PaymentSplitPayments composed in-place into PaymentInstrument and role schema hoisted
    assert_eq!(
        bundle.defs["PaymentInstrument"]["properties"]["amount"]["$ref"],
        "#/$defs/Amount"
    );
    assert!(bundle.defs.contains_key("BusinessSplitPaymentsConfig"));
    assert!(bundle
        .defs
        .contains_key("PaymentSplitPaymentsBusinessSchema"));

    // 4. Fulfillment + Loyalty composed into CatalogSearch and CatalogLookup container ops
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

    let opts = GenerateTypesOptions::new().schema_dir(schema_dir);

    let bundle = generate_types(&opts).unwrap();
    assert_bundle_invariants(&bundle.defs);

    // Verify self-named capability role schemas and helper defs are present
    for expected in [
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
        // Collision-qualified $defs alongside standalone types
        "ErrorCode",
        "PaymentAp2MandateErrorCode",
        "ErrorResponse",
        "JsonrpcErrorResponse",
        "Message",
        "A2aMessageMessage",
        // Standalone map-valued schema with $defs
        "Actions",
        "Instance",
        // Container capabilities
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
    ] {
        assert!(
            bundle.defs.contains_key(expected),
            "expected '{expected}' in full corpus bundle.defs"
        );
    }

    // Verify payment_authentication.json merges inline properties onto Checkout.properties.actions
    assert!(
        bundle.defs["Checkout"]["properties"]["actions"]["properties"]
            .get("dev.ucp.common.payment.device_data_collection")
            .is_some()
    );
    assert!(
        bundle.defs["Checkout"]["properties"]["actions"]["properties"]
            .get("dev.ucp.common.payment.three_ds_challenge")
            .is_some()
    );

    // Verify payment_ap2_mandate.json response-only Ap2WithMerchantAuthorization is omitted from CheckoutCompleteRequest.properties.ap2.allOf
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

    // 1. Passing --capability dev.ucp.shopping.checkout with extensions: None excludes unrequested extensions
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

    // 2. Passing an extension ("dev.ucp.shopping.fulfillment") via capabilities reclassifies it into active extensions
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

    // 3. Passing --capability dev.ucp.shopping.cart alone does not pull in Checkout; passing both cart and checkout
    // composes cart.json#/$defs/checkout (adding cart_id) into Checkout.
    let cart_only = generate_types(
        &GenerateTypesOptions::new()
            .schema_dir(&schema_dir)
            .capabilities(["dev.ucp.shopping.cart"]),
    )
    .unwrap();
    assert_bundle_invariants(&cart_only.defs);
    assert!(cart_only.defs.contains_key("Cart"));
    assert!(!cart_only.defs.contains_key("Checkout"));

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
            .is_some(),
        "expected cart.json#/$defs/checkout overlay to add cart_id to CheckoutCreateRequest"
    );

    // 4. Short names ("checkout") are rejected with InvalidCapability
    let err = generate_types(
        &GenerateTypesOptions::new()
            .schema_dir(&schema_dir)
            .capabilities(["checkout"]),
    )
    .expect_err("short capability names must be rejected");
    assert_eq!(err.exit_code(), 2);
}
