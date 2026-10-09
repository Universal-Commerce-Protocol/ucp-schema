use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use ucp_schema::{generate_openapi, CodegenError, GenerateOpenApiOptions};

fn ucp_source_dir() -> Option<PathBuf> {
    let candidate = Path::new(env!("CARGO_MANIFEST_DIR")).join("../ucp/source");
    candidate.exists().then_some(candidate)
}

fn visit_json_objects(val: &Value, f: &mut impl FnMut(&serde_json::Map<String, Value>)) {
    match val {
        Value::Object(obj) => {
            f(obj);
            for (k, v) in obj {
                if !["const", "enum", "default", "examples"].contains(&k.as_str()) {
                    visit_json_objects(v, f);
                }
            }
        }
        Value::Array(arr) => arr.iter().for_each(|v| visit_json_objects(v, f)),
        _ => {}
    }
}

fn assert_openapi_invariants(spec: &Value) {
    assert_eq!(spec["openapi"], "3.1.0");
    assert_eq!(
        spec["jsonSchemaDialect"],
        "https://spec.openapis.org/oas/3.1/dialect/base"
    );
    assert!(spec["paths"].is_object(), "paths must be an object");

    let components = spec["components"]
        .as_object()
        .expect("components must be an object");
    let schemas = components["schemas"]
        .as_object()
        .expect("components.schemas must be an object");
    let known_schemas: BTreeSet<&str> = schemas.keys().map(String::as_str).collect();
    let known_params: BTreeSet<&str> = components
        .get("parameters")
        .and_then(Value::as_object)
        .map(|m| m.keys().map(String::as_str).collect())
        .unwrap_or_default();
    let known_headers: BTreeSet<&str> = components
        .get("headers")
        .and_then(Value::as_object)
        .map(|m| m.keys().map(String::as_str).collect())
        .unwrap_or_default();

    for schema_name in &known_schemas {
        assert!(
            schema_name.starts_with(|c: char| c.is_ascii_uppercase()) && !schema_name.contains('_'),
            "components.schemas key '{schema_name}' must be canonical PascalCase (no snake_case wrappers)"
        );
    }

    visit_json_objects(spec, &mut |obj| {
        assert!(
            !obj.contains_key("discriminator"),
            "OpenAPI spec must not contain 'discriminator' keyword: {obj:?}"
        );
        assert!(
            !obj.keys().any(|k| {
                k == "ucp_request"
                    || k == "ucp_response"
                    || k == "ucp_shared_request"
                    || k.starts_with("x-ucp-")
            }),
            "OpenAPI spec still contains UCP authoring annotation: {obj:?}"
        );

        let Some(r) = obj.get("$ref").and_then(Value::as_str) else {
            return;
        };
        assert!(
            !r.starts_with("#/$defs/"),
            "OpenAPI spec contains unrewritten #/$defs/ reference: '{r}'"
        );
        if let Some(target) = r.strip_prefix("#/components/schemas/") {
            assert!(
                known_schemas.contains(target),
                "dangling schema $ref '#/components/schemas/{target}'"
            );
            return;
        }
        if let Some(target) = r.strip_prefix("#/components/parameters/") {
            assert!(
                known_params.contains(target),
                "dangling parameter $ref '#/components/parameters/{target}'"
            );
            return;
        }
        if let Some(target) = r.strip_prefix("#/components/headers/") {
            assert!(
                known_headers.contains(target),
                "dangling header $ref '#/components/headers/{target}'"
            );
            return;
        }
        panic!("unexpected non-component $ref in OpenAPI spec: '{r}'");
    });
}

#[test]
fn shopping_checkout_discount_fulfillment_prunes_routes_and_binds_directional_schemas() {
    let Some(source_dir) = ucp_source_dir() else {
        return;
    };
    let schema_dir = source_dir.join("schemas");
    let s = |rel: &str| schema_dir.join(rel).to_string_lossy().into_owned();

    let tmp = tempfile::tempdir().unwrap();
    let profile_path = tmp.path().join("shopping_checkout_profile.json");
    let profile = json!({
        "ucp": {
            "version": "2026-08-25",
            "services": {
                "dev.ucp.shopping": [
                    {
                        "version": "2026-08-25",
                        "transport": "rest",
                        "endpoint": "https://merchant.example.com/ucp",
                        "schema": "https://ucp.dev/2026-08-25/services/shopping/rest.openapi.json"
                    }
                ]
            },
            "capabilities": {
                "dev.ucp.shopping.checkout": [
                    { "version": "2026-08-25", "schema": s("shopping/checkout.json") }
                ],
                "dev.ucp.shopping.discount": [
                    {
                        "version": "2026-08-25",
                        "schema": s("shopping/discount.json"),
                        "extends": "dev.ucp.shopping.checkout"
                    }
                ],
                "dev.ucp.shopping.fulfillment": [
                    {
                        "version": "2026-08-25",
                        "schema": s("shopping/fulfillment.json"),
                        "extends": "dev.ucp.shopping.checkout"
                    }
                ]
            }
        }
    });
    std::fs::write(&profile_path, profile.to_string()).unwrap();

    let opts = GenerateOpenApiOptions::new(profile_path.to_str().unwrap());
    let spec = generate_openapi(&opts).unwrap();
    assert_openapi_invariants(&spec);

    assert_eq!(
        spec["servers"],
        json!([{ "url": "https://merchant.example.com/ucp" }])
    );

    let paths = spec["paths"].as_object().unwrap();
    let retained_paths: Vec<&str> = paths.keys().map(String::as_str).collect();
    assert_eq!(
        retained_paths,
        vec![
            "/checkout-sessions",
            "/checkout-sessions/{id}",
            "/checkout-sessions/{id}/complete",
            "/checkout-sessions/{id}/cancel",
        ]
    );
    assert!(
        spec.get("webhooks").is_none(),
        "webhooks must be pruned when order capability is inactive"
    );

    // Verify directional request bindings
    assert_eq!(
        paths["/checkout-sessions"]["post"]["requestBody"]["content"]["application/json"]["schema"]
            ["$ref"],
        "#/components/schemas/CheckoutCreateRequest"
    );
    assert_eq!(
        paths["/checkout-sessions/{id}"]["put"]["requestBody"]["content"]["application/json"]
            ["schema"]["$ref"],
        "#/components/schemas/CheckoutUpdateRequest"
    );
    assert_eq!(
        paths["/checkout-sessions/{id}/complete"]["post"]["requestBody"]["content"]
            ["application/json"]["schema"]["$ref"],
        "#/components/schemas/CheckoutCompleteRequest"
    );

    // Verify inlined oneOf response bindings
    let expected_checkout_oneof = json!({
        "oneOf": [
            { "$ref": "#/components/schemas/Checkout" },
            { "$ref": "#/components/schemas/ErrorResponse" }
        ]
    });
    assert_eq!(
        paths["/checkout-sessions"]["post"]["responses"]["201"]["content"]["application/json"]
            ["schema"],
        expected_checkout_oneof
    );
    assert_eq!(
        paths["/checkout-sessions/{id}"]["get"]["responses"]["200"]["content"]["application/json"]
            ["schema"],
        expected_checkout_oneof
    );
    assert_eq!(
        paths["/checkout-sessions/{id}/cancel"]["post"]["responses"]["200"]["content"]
            ["application/json"]["schema"],
        expected_checkout_oneof
    );

    let schemas = spec["components"]["schemas"].as_object().unwrap();
    for expected in [
        "Checkout",
        "CheckoutCreateRequest",
        "CheckoutUpdateRequest",
        "CheckoutCompleteRequest",
        "DiscountsObject",
        "DiscountsObjectCreateRequest",
        "DiscountsObjectUpdateRequest",
        "Fulfillment",
        "FulfillmentCreateRequest",
        "FulfillmentUpdateRequest",
        "FulfillmentMethod",
        "FulfillmentMethodBase",
        "ShippingMethod",
        "PickupMethod",
        "FulfillmentDestination",
        "FulfillmentDestinationBase",
        "ShippingDestination",
        "LocationDestination",
        "ErrorResponse",
        "ResponseCheckoutSchema",
    ] {
        assert!(
            schemas.contains_key(expected),
            "expected '{expected}' in components.schemas"
        );
    }

    for pruned in [
        "checkout",
        "checkout_response",
        "cart",
        "cart_response",
        "order",
        "order_response",
        "Cart",
        "Order",
        "CatalogSearchRequest",
        "DiscoveryProfile",
        "CheckoutPlatformSchema",
        "CheckoutBusinessSchema",
        "UcpPlatformSchema",
        "UcpBusinessSchema",
    ] {
        assert!(
            !schemas.contains_key(pruned),
            "expected '{pruned}' to be pruned from components.schemas"
        );
    }

    let params = spec["components"]["parameters"].as_object().unwrap();
    assert!(params.contains_key("checkout_session_id_path"));
    for pruned_param in [
        "cart_id_path",
        "order_id_path",
        "webhook_timestamp",
        "webhook_id",
        "QueryParam",
        "LimitParam",
    ] {
        assert!(
            !params.contains_key(pruned_param),
            "expected unused parameter '{pruned_param}' to be pruned"
        );
    }
}

#[test]
fn shopping_order_retains_orders_route_and_order_event_webhook() {
    let Some(source_dir) = ucp_source_dir() else {
        return;
    };
    let schema_dir = source_dir.join("schemas");
    let s = |rel: &str| schema_dir.join(rel).to_string_lossy().into_owned();

    let tmp = tempfile::tempdir().unwrap();
    let profile_path = tmp.path().join("shopping_order_profile.json");
    let profile = json!({
        "ucp": {
            "version": "2026-08-25",
            "services": {
                "dev.ucp.shopping": [{
                    "version": "2026-08-25",
                    "transport": "rest",
                    "endpoint": "https://orders.example.com/ucp",
                    "schema": source_dir.join("services/shopping/rest.openapi.json").to_str().unwrap()
                }]
            },
            "capabilities": {
                "dev.ucp.shopping.order": [
                    { "version": "2026-08-25", "schema": s("shopping/order.json") }
                ]
            }
        }
    });
    std::fs::write(&profile_path, profile.to_string()).unwrap();

    let spec =
        generate_openapi(&GenerateOpenApiOptions::new(profile_path.to_str().unwrap())).unwrap();
    assert_openapi_invariants(&spec);

    let paths = spec["paths"].as_object().unwrap();
    let retained_paths: Vec<&str> = paths.keys().map(String::as_str).collect();
    assert_eq!(retained_paths, vec!["/orders/{id}"]);
    assert_eq!(
        paths["/orders/{id}"]["get"]["responses"]["200"]["content"]["application/json"]["schema"],
        json!({
            "oneOf": [
                { "$ref": "#/components/schemas/Order" },
                { "$ref": "#/components/schemas/ErrorResponse" }
            ]
        })
    );

    let webhooks = spec["webhooks"]
        .as_object()
        .expect("webhooks must be retained when order capability is active");
    assert!(webhooks.contains_key("orderEvent"));
    let webhook_op = &webhooks["orderEvent"]["post"];
    assert_eq!(
        webhook_op["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/Order"
    );
    assert_eq!(
        webhook_op["responses"]["200"]["content"]["application/json"]["schema"]["properties"]
            ["ucp"]["$ref"],
        "#/components/schemas/UcpBase"
    );

    let schemas = spec["components"]["schemas"].as_object().unwrap();
    assert!(schemas.contains_key("Order"));
    assert!(schemas.contains_key("UcpBase"));
    assert!(schemas.contains_key("ErrorResponse"));
    assert!(!schemas.contains_key("Checkout"));
    assert!(!schemas.contains_key("Cart"));

    let params = spec["components"]["parameters"].as_object().unwrap();
    assert!(params.contains_key("order_id_path"));
    assert!(params.contains_key("webhook_timestamp"));
    assert!(params.contains_key("webhook_id"));
    assert!(!params.contains_key("checkout_session_id_path"));
    assert!(!params.contains_key("cart_id_path"));
}

#[test]
fn container_capabilities_and_lodging_booking_bind_canonical_schemas() {
    let Some(source_dir) = ucp_source_dir() else {
        return;
    };
    let schema_dir = source_dir.join("schemas");
    let s = |rel: &str| schema_dir.join(rel).to_string_lossy().into_owned();

    let tmp = tempfile::tempdir().unwrap();
    let profile_path = tmp.path().join("containers_and_lodging_profile.json");
    let profile = json!({
        "ucp": {
            "version": "2026-08-25",
            "services": {
                "dev.ucp.shopping": [{
                    "version": "2026-08-25",
                    "transport": "rest",
                    "endpoint": "https://commerce.example.com/shopping",
                    "schema": "https://ucp.dev/2026-08-25/services/shopping/rest.openapi.json"
                }],
                "dev.ucp.common": [{
                    "version": "2026-08-25",
                    "transport": "rest",
                    "endpoint": "https://commerce.example.com/common",
                    "schema": "https://ucp.dev/2026-08-25/services/common/rest.openapi.json"
                }],
                "dev.ucp.lodging": [{
                    "version": "2026-08-25",
                    "transport": "rest",
                    "endpoint": "https://commerce.example.com/lodging",
                    "schema": "https://ucp.dev/2026-08-25/services/lodging/rest.openapi.json"
                }]
            },
            "capabilities": {
                "dev.ucp.shopping.catalog.search": [
                    { "version": "2026-08-25", "schema": s("shopping/catalog_search.json") }
                ],
                "dev.ucp.shopping.catalog.lookup": [
                    { "version": "2026-08-25", "schema": s("shopping/catalog_lookup.json") }
                ],
                "dev.ucp.common.location.search": [
                    { "version": "2026-08-25", "schema": s("common/location_search.json") }
                ],
                "dev.ucp.common.location.lookup": [
                    { "version": "2026-08-25", "schema": s("common/location_lookup.json") }
                ],
                "dev.ucp.common.ask": [
                    { "version": "2026-08-25", "schema": s("common/ask.json") }
                ],
                "dev.ucp.lodging.booking": [
                    { "version": "2026-08-25", "schema": s("lodging/booking.json") }
                ]
            }
        }
    });
    std::fs::write(&profile_path, profile.to_string()).unwrap();

    // 1. Default multi-service merge includes shopping catalog, common location/ask, and lodging booking
    let merged_spec =
        generate_openapi(&GenerateOpenApiOptions::new(profile_path.to_str().unwrap())).unwrap();
    assert_openapi_invariants(&merged_spec);

    assert_eq!(
        merged_spec["servers"],
        json!([
            { "url": "https://commerce.example.com/shopping" },
            { "url": "https://commerce.example.com/common" },
            { "url": "https://commerce.example.com/lodging" }
        ])
    );

    let paths = merged_spec["paths"].as_object().unwrap();
    assert_eq!(
        paths["/catalog/search"]["post"]["requestBody"]["content"]["application/json"]["schema"]
            ["$ref"],
        "#/components/schemas/CatalogSearchRequest"
    );
    assert_eq!(
        paths["/catalog/search"]["post"]["responses"]["200"]["content"]["application/json"]
            ["schema"]["$ref"],
        "#/components/schemas/CatalogSearchResponse"
    );
    assert_eq!(
        paths["/catalog/lookup"]["post"]["requestBody"]["content"]["application/json"]["schema"]
            ["$ref"],
        "#/components/schemas/CatalogLookupRequest"
    );
    assert_eq!(
        paths["/catalog/lookup"]["post"]["responses"]["200"]["content"]["application/json"]
            ["schema"]["$ref"],
        "#/components/schemas/CatalogLookupResponse"
    );
    assert_eq!(
        paths["/catalog/product"]["post"]["requestBody"]["content"]["application/json"]["schema"]
            ["$ref"],
        "#/components/schemas/CatalogGetProductRequest"
    );
    assert_eq!(
        paths["/catalog/product"]["post"]["responses"]["200"]["content"]["application/json"]
            ["schema"],
        json!({
            "oneOf": [
                { "$ref": "#/components/schemas/CatalogGetProductResponse" },
                { "$ref": "#/components/schemas/ErrorResponse" }
            ]
        })
    );

    assert_eq!(
        paths["/locations/search"]["post"]["requestBody"]["content"]["application/json"]["schema"]
            ["$ref"],
        "#/components/schemas/LocationSearchRequest"
    );
    assert_eq!(
        paths["/locations/search"]["post"]["responses"]["200"]["content"]["application/json"]
            ["schema"]["$ref"],
        "#/components/schemas/LocationSearchResponse"
    );
    assert_eq!(
        paths["/locations/lookup"]["post"]["requestBody"]["content"]["application/json"]["schema"]
            ["$ref"],
        "#/components/schemas/LocationLookupRequest"
    );
    assert_eq!(
        paths["/locations/lookup"]["post"]["responses"]["200"]["content"]["application/json"]
            ["schema"]["$ref"],
        "#/components/schemas/LocationLookupResponse"
    );
    assert_eq!(
        paths["/ask"]["post"]["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/AskRequest"
    );
    assert_eq!(
        paths["/ask"]["post"]["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/AskResponse"
    );

    // Lodging booking directional request and response bindings
    assert_eq!(
        paths["/booking-sessions"]["post"]["requestBody"]["content"]["application/json"]["schema"]
            ["$ref"],
        "#/components/schemas/BookingCreateRequest"
    );
    assert_eq!(
        paths["/booking-sessions/{id}"]["put"]["requestBody"]["content"]["application/json"]
            ["schema"]["$ref"],
        "#/components/schemas/BookingUpdateRequest"
    );
    assert_eq!(
        paths["/booking-sessions/{id}/complete"]["post"]["requestBody"]["content"]
            ["application/json"]["schema"]["$ref"],
        "#/components/schemas/BookingCompleteRequest"
    );
    assert_eq!(
        paths["/booking-sessions"]["post"]["responses"]["201"]["content"]["application/json"]
            ["schema"],
        json!({
            "oneOf": [
                { "$ref": "#/components/schemas/Booking" },
                { "$ref": "#/components/schemas/ErrorResponse" }
            ]
        })
    );

    // 2. Filtering via --service dev.ucp.common scopes strictly to common service routes & schemas
    let common_spec = generate_openapi(
        &GenerateOpenApiOptions::new(profile_path.to_str().unwrap())
            .service("dev.ucp.common")
            .server_url("https://override.example.com/common"),
    )
    .unwrap();
    assert_openapi_invariants(&common_spec);
    assert_eq!(
        common_spec["servers"],
        json!([{ "url": "https://override.example.com/common" }])
    );
    let common_paths: Vec<&str> = common_spec["paths"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        common_paths,
        vec!["/locations/search", "/locations/lookup", "/ask"]
    );
    let common_schemas = common_spec["components"]["schemas"].as_object().unwrap();
    assert!(common_schemas.contains_key("AskRequest"));
    assert!(common_schemas.contains_key("LocationSearchRequest"));
    assert!(!common_schemas.contains_key("CatalogSearchRequest"));
    assert!(!common_schemas.contains_key("Booking"));
}

#[test]
fn error_handling_and_cli_exit_codes_for_missing_rest_binding_and_schema() {
    let Some(source_dir) = ucp_source_dir() else {
        return;
    };
    let schema_dir = source_dir.join("schemas");
    let s = |rel: &str| schema_dir.join(rel).to_string_lossy().into_owned();

    let tmp = tempfile::tempdir().unwrap();

    // 1. MCP-only profile (like Allbirds) -> NoRestServiceBinding (exit code 2)
    let mcp_profile_path = tmp.path().join("mcp_only_profile.json");
    let mcp_profile = json!({
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
                "dev.ucp.shopping.checkout": [
                    { "version": "2026-08-25", "schema": s("shopping/checkout.json") }
                ]
            }
        }
    });
    std::fs::write(&mcp_profile_path, mcp_profile.to_string()).unwrap();

    let err = generate_openapi(&GenerateOpenApiOptions::new(
        mcp_profile_path.to_str().unwrap(),
    ))
    .expect_err("MCP-only profile must return NoRestServiceBinding");
    assert!(matches!(err, CodegenError::NoRestServiceBinding { .. }));
    assert_eq!(err.exit_code(), 2);

    // 2. REST binding missing "schema" URL -> MissingServiceSchema (exit code 2)
    let missing_schema_path = tmp.path().join("missing_service_schema_profile.json");
    let missing_schema_profile = json!({
        "ucp": {
            "version": "2026-08-25",
            "services": {
                "dev.ucp.shopping": [{
                    "version": "2026-08-25",
                    "transport": "rest",
                    "endpoint": "https://merchant.example.com/ucp"
                }]
            },
            "capabilities": {
                "dev.ucp.shopping.checkout": [
                    { "version": "2026-08-25", "schema": s("shopping/checkout.json") }
                ]
            }
        }
    });
    std::fs::write(&missing_schema_path, missing_schema_profile.to_string()).unwrap();

    let err = generate_openapi(&GenerateOpenApiOptions::new(
        missing_schema_path.to_str().unwrap(),
    ))
    .expect_err("REST service missing schema must return MissingServiceSchema");
    assert!(matches!(err, CodegenError::MissingServiceSchema { .. }));
    assert_eq!(err.exit_code(), 2);

    // 3. --service filter matching no REST service -> NoRestServiceBinding
    let valid_profile_path = tmp.path().join("valid_rest_profile.json");
    let valid_profile = json!({
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
                "dev.ucp.shopping.checkout": [
                    { "version": "2026-08-25", "schema": s("shopping/checkout.json") }
                ]
            }
        }
    });
    std::fs::write(&valid_profile_path, valid_profile.to_string()).unwrap();

    let err = generate_openapi(
        &GenerateOpenApiOptions::new(valid_profile_path.to_str().unwrap())
            .service("dev.ucp.lodging"),
    )
    .expect_err("non-matching --service must return NoRestServiceBinding");
    assert!(matches!(err, CodegenError::NoRestServiceBinding { .. }));
    assert_eq!(err.exit_code(), 2);

    // 4. Verify CLI execution (both success with --server-url / --output and failure exit code 2)
    let bin = env!("CARGO_BIN_EXE_ucp-schema");
    let out_file = tmp.path().join("emitted.openapi.json");
    let ok_out = std::process::Command::new(bin)
        .args([
            "generate-openapi",
            "--profile",
            valid_profile_path.to_str().unwrap(),
            "--server-url",
            "https://cli-override.example.com/ucp",
            "--output",
            out_file.to_str().unwrap(),
        ])
        .output()
        .expect("run ucp-schema generate-openapi");
    assert!(
        ok_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&ok_out.stderr)
    );
    let emitted: Value =
        serde_json::from_str(&std::fs::read_to_string(&out_file).unwrap()).unwrap();
    assert_openapi_invariants(&emitted);
    assert_eq!(
        emitted["servers"],
        json!([{ "url": "https://cli-override.example.com/ucp" }])
    );

    let fail_out = std::process::Command::new(bin)
        .args([
            "generate-openapi",
            "--profile",
            mcp_profile_path.to_str().unwrap(),
        ])
        .output()
        .expect("run ucp-schema generate-openapi on MCP-only profile");
    assert_eq!(fail_out.status.code(), Some(2));

    // 5. Verify --pretty=false compact output on stdout
    let compact_out = std::process::Command::new(bin)
        .args([
            "generate-openapi",
            "-p",
            valid_profile_path.to_str().unwrap(),
            "--pretty=false",
        ])
        .output()
        .expect("run ucp-schema generate-openapi --pretty=false");
    assert!(compact_out.status.success());
    let compact_str = String::from_utf8(compact_out.stdout).unwrap();
    assert_eq!(compact_str.lines().count(), 1);
    let compact_val: Value = serde_json::from_str(&compact_str).unwrap();
    assert_openapi_invariants(&compact_val);
}

#[test]
fn shopping_cart_retains_cart_routes_and_binds_directional_cart_schemas() {
    let Some(source_dir) = ucp_source_dir() else {
        return;
    };
    let schema_dir = source_dir.join("schemas");
    let s = |rel: &str| schema_dir.join(rel).to_string_lossy().into_owned();

    let tmp = tempfile::tempdir().unwrap();
    let profile_path = tmp.path().join("shopping_cart_profile.json");
    let profile = json!({
        "ucp": {
            "version": "2026-08-25",
            "services": {
                "dev.ucp.shopping": [{
                    "version": "2026-08-25",
                    "transport": "rest",
                    "endpoint": "https://carts.example.com/ucp",
                    "schema": "https://ucp.dev/2026-08-25/services/shopping/rest.openapi.json"
                }]
            },
            "capabilities": {
                "dev.ucp.shopping.cart": [
                    { "version": "2026-08-25", "schema": s("shopping/cart.json") }
                ]
            }
        }
    });
    std::fs::write(&profile_path, profile.to_string()).unwrap();

    let spec = generate_openapi(&GenerateOpenApiOptions::from_profile(
        profile_path.to_str().unwrap(),
    ))
    .unwrap();
    assert_openapi_invariants(&spec);

    let paths = spec["paths"].as_object().unwrap();
    let retained_paths: Vec<&str> = paths.keys().map(String::as_str).collect();
    assert_eq!(
        retained_paths,
        vec!["/carts", "/carts/{id}", "/carts/{id}/cancel"]
    );
    assert!(spec.get("webhooks").is_none());

    assert_eq!(
        paths["/carts"]["post"]["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/CartCreateRequest"
    );
    assert_eq!(
        paths["/carts/{id}"]["put"]["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/CartUpdateRequest"
    );
    let expected_cart_oneof = json!({
        "oneOf": [
            { "$ref": "#/components/schemas/Cart" },
            { "$ref": "#/components/schemas/ErrorResponse" }
        ]
    });
    assert_eq!(
        paths["/carts"]["post"]["responses"]["201"]["content"]["application/json"]["schema"],
        expected_cart_oneof
    );
    assert_eq!(
        paths["/carts/{id}"]["get"]["responses"]["200"]["content"]["application/json"]["schema"],
        expected_cart_oneof
    );
    assert_eq!(
        paths["/carts/{id}/cancel"]["post"]["responses"]["200"]["content"]["application/json"]
            ["schema"],
        expected_cart_oneof
    );

    let params = spec["components"]["parameters"].as_object().unwrap();
    assert!(params.contains_key("cart_id_path"));
    assert!(!params.contains_key("checkout_session_id_path"));
    assert!(!params.contains_key("order_id_path"));
}
