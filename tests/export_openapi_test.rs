//! Integration tests for OpenAPI 3.1 exporter (`export-openapi`).

use assert_cmd::Command;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;
use ucp_schema::{export_openapi, ExportOpenApiOptions};

fn setup_test_schemas() -> (TempDir, PathBuf) {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("schemas");

    let shopping_dir = root.join("shopping");
    let types_dir = shopping_dir.join("types");
    let common_types_dir = root.join("common").join("types");

    fs::create_dir_all(&types_dir).unwrap();
    fs::create_dir_all(&common_types_dir).unwrap();

    // 1. Common Types
    fs::write(
        common_types_dir.join("amount.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://ucp.dev/draft/schemas/common/types/amount.json",
            "title": "Amount",
            "type": "integer",
            "description": "Monetary amount in minor units (e.g. cents)."
        })
        .to_string(),
    )
    .unwrap();

    fs::write(
        common_types_dir.join("postal_address.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://ucp.dev/draft/schemas/common/types/postal_address.json",
            "title": "Postal Address",
            "type": "object",
            "required": ["street_address", "locality", "postal_code", "country"],
            "properties": {
                "street_address": { "type": "string" },
                "locality": { "type": "string" },
                "postal_code": { "type": "string" },
                "country": { "type": "string" }
            }
        })
        .to_string(),
    )
    .unwrap();

    fs::write(
        common_types_dir.join("location_summary.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://ucp.dev/draft/schemas/common/types/location_summary.json",
            "title": "Location Summary",
            "type": "object",
            "required": ["location_id", "name"],
            "properties": {
                "location_id": { "type": "string" },
                "name": { "type": "string" }
            }
        })
        .to_string(),
    )
    .unwrap();

    fs::write(
        common_types_dir.join("totals.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://ucp.dev/draft/schemas/common/types/totals.json",
            "title": "Totals",
            "description": "Object-wrapped totals breakdown per PR #684.",
            "type": "object",
            "required": ["subtotal", "total"],
            "properties": {
                "subtotal": { "$ref": "https://ucp.dev/draft/schemas/common/types/amount.json" },
                "tax": { "$ref": "https://ucp.dev/draft/schemas/common/types/amount.json" },
                "total": { "$ref": "https://ucp.dev/draft/schemas/common/types/amount.json" }
            }
        })
        .to_string(),
    )
    .unwrap();

    // 2. Shopping Types: LineItem
    fs::write(
        types_dir.join("line_item.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://ucp.dev/draft/schemas/shopping/types/line_item.json",
            "title": "Line Item",
            "type": "object",
            "required": ["item_id", "quantity"],
            "properties": {
                "item_id": { "type": "string", "ucp_request": "required" },
                "quantity": { "type": "integer", "minimum": 1, "ucp_request": "required" },
                "title": { "type": "string", "ucp_request": "optional" },
                "price": { "$ref": "https://ucp.dev/draft/schemas/common/types/amount.json", "ucp_request": "omit" }
            }
        })
        .to_string(),
    )
    .unwrap();

    // 3. Polymorphic Destinations (PR #688)
    fs::write(
        types_dir.join("shipping_destination.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://ucp.dev/draft/schemas/shopping/types/shipping_destination.json",
            "title": "Shipping Destination",
            "type": "object",
            "ucp_shared_request": true,
            "allOf": [
                { "$ref": "https://ucp.dev/draft/schemas/common/types/postal_address.json" },
                {
                    "type": "object",
                    "required": ["id", "type"],
                    "properties": {
                        "id": { "type": "string", "ucp_request": "optional" },
                        "type": {
                            "type": "string",
                            "const": "shipping_address",
                            "description": "Discriminator value.",
                            "ucp_request": "optional"
                        }
                    }
                }
            ]
        })
        .to_string(),
    )
    .unwrap();

    fs::write(
        types_dir.join("location_destination.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://ucp.dev/draft/schemas/shopping/types/location_destination.json",
            "title": "Location Destination",
            "type": "object",
            "ucp_shared_request": true,
            "allOf": [
                { "$ref": "https://ucp.dev/draft/schemas/common/types/location_summary.json" },
                {
                    "type": "object",
                    "required": ["type"],
                    "properties": {
                        "type": {
                            "type": "string",
                            "const": "business_location",
                            "description": "Discriminator value.",
                            "ucp_request": "omit"
                        }
                    }
                }
            ]
        })
        .to_string(),
    )
    .unwrap();

    fs::write(
        types_dir.join("fulfillment_destination.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://ucp.dev/draft/schemas/shopping/types/fulfillment_destination.json",
            "title": "Fulfillment Destination",
            "description": "A destination for fulfillment.",
            "type": "object",
            "ucp_shared_request": true,
            "required": ["type", "id"],
            "properties": {
                "type": {
                    "type": "string",
                    "description": "Discriminator.",
                    "ucp_request": "optional"
                },
                "id": {
                    "type": "string",
                    "ucp_request": "optional"
                }
            },
            "allOf": [
                {
                    "if": {
                        "properties": {
                            "type": { "const": "shipping_address" }
                        },
                        "required": ["type"]
                    },
                    "then": {
                        "$ref": "https://ucp.dev/draft/schemas/shopping/types/shipping_destination.json"
                    }
                },
                {
                    "if": {
                        "properties": {
                            "type": { "const": "business_location" }
                        },
                        "required": ["type"]
                    },
                    "then": {
                        "$ref": "https://ucp.dev/draft/schemas/shopping/types/location_destination.json"
                    }
                }
            ]
        })
        .to_string(),
    )
    .unwrap();

    // 4. Shopping Root Resource: Checkout
    fs::write(
        shopping_dir.join("checkout.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://ucp.dev/draft/schemas/shopping/checkout.json",
            "name": "dev.ucp.shopping.checkout",
            "title": "Checkout",
            "description": "Base checkout session resource.",
            "type": "object",
            "x-ucp-path": "/checkout-sessions",
            "x-ucp-lifecycle": ["complete", "cancel"],
            "required": ["id", "line_items", "status", "totals"],
            "properties": {
                "id": {
                    "type": "string",
                    "ucp_request": {
                        "create": "omit",
                        "update": "required",
                        "read": "required"
                    }
                },
                "line_items": {
                    "type": "array",
                    "items": { "$ref": "https://ucp.dev/draft/schemas/shopping/types/line_item.json" },
                    "ucp_request": {
                        "create": "required",
                        "update": "required",
                        "read": "required"
                    }
                },
                "destination": {
                    "$ref": "https://ucp.dev/draft/schemas/shopping/types/fulfillment_destination.json",
                    "ucp_request": {
                        "create": "optional",
                        "update": "optional"
                    }
                },
                "status": {
                    "type": "string",
                    "enum": ["incomplete", "ready_for_complete", "completed"],
                    "ucp_request": "omit"
                },
                "totals": {
                    "$ref": "https://ucp.dev/draft/schemas/common/types/totals.json",
                    "ucp_request": "omit"
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    (temp_dir, root)
}

#[test]
fn test_export_openapi_programmatic() {
    let (_temp_dir, schema_dir) = setup_test_schemas();

    let options = ExportOpenApiOptions::new(schema_dir)
        .title("UCP Shopping API")
        .api_version("2026-09-01")
        .description(Some("Universal Commerce Protocol API Specification".into()));

    let doc = export_openapi(&options).expect("OpenAPI export should succeed");

    // 1. Basic Metadata
    assert_eq!(doc.openapi, "3.1.0");
    assert_eq!(doc.info.title, "UCP Shopping API");
    assert_eq!(doc.info.version, "2026-09-01");
    assert_eq!(
        doc.info.description.as_deref(),
        Some("Universal Commerce Protocol API Specification")
    );

    let components = doc.components.expect("Components must be present");
    let schemas = &components.schemas;

    // 2. Discriminator Polymorphism (PR #688)
    assert!(schemas.contains_key("FulfillmentDestination"));
    let fulfillment_dest = &schemas["FulfillmentDestination"];

    // allOf conditional branches must be replaced
    assert!(fulfillment_dest.get("allOf").is_none());

    // oneOf union with refs
    let one_of = fulfillment_dest
        .get("oneOf")
        .expect("oneOf must be present")
        .as_array()
        .unwrap();
    assert_eq!(one_of.len(), 2);
    assert_eq!(
        one_of[0]["$ref"],
        "#/components/schemas/LocationDestination"
    );
    assert_eq!(
        one_of[1]["$ref"],
        "#/components/schemas/ShippingDestination"
    );

    // discriminator mapping
    let discriminator = &fulfillment_dest["discriminator"];
    assert_eq!(discriminator["propertyName"], "type");
    assert_eq!(
        discriminator["mapping"]["shipping_address"],
        "#/components/schemas/ShippingDestination"
    );
    assert_eq!(
        discriminator["mapping"]["business_location"],
        "#/components/schemas/LocationDestination"
    );

    // 3. Directional Request & Response Models (FR-3)
    assert!(schemas.contains_key("CheckoutCreateRequest"));
    assert!(schemas.contains_key("CheckoutUpdateRequest"));
    assert!(schemas.contains_key("Checkout"));

    let create_req = &schemas["CheckoutCreateRequest"];
    let update_req = &schemas["CheckoutUpdateRequest"];
    let response_model = &schemas["Checkout"];

    // CreateRequest: id and status omitted, line_items required
    assert!(create_req["properties"].get("id").is_none());
    assert!(create_req["properties"].get("status").is_none());
    assert!(create_req["properties"].get("totals").is_none());
    assert!(create_req["properties"].get("line_items").is_some());
    assert_eq!(
        create_req["properties"]["line_items"]["items"]["$ref"],
        "#/components/schemas/LineItemCreateRequest"
    );
    let create_req_required = create_req["required"].as_array().unwrap();
    assert!(create_req_required.contains(&json!("line_items")));
    assert!(!create_req_required.contains(&json!("id")));

    // UpdateRequest: id required
    assert!(update_req["properties"].get("id").is_some());
    assert!(update_req["properties"].get("status").is_none());

    // Response model: all fields present
    assert!(response_model["properties"].get("id").is_some());
    assert!(response_model["properties"].get("status").is_some());
    assert!(response_model["properties"].get("totals").is_some());

    // 4. Object Totals (PR #684)
    assert!(schemas.contains_key("Totals"));
    let totals_schema = &schemas["Totals"];
    assert_eq!(totals_schema["type"], "object");
    assert_eq!(
        totals_schema["properties"]["subtotal"]["$ref"],
        "#/components/schemas/Amount"
    );

    // 5. Standard Parameters & Headers (FR-6)
    let params = components.parameters.expect("parameters must be present");
    assert!(params.contains_key("UcpAgent"));
    assert_eq!(params["UcpAgent"].name, "UCP-Agent");
    assert!(params.contains_key("IdempotencyKey"));
    assert_eq!(params["IdempotencyKey"].name, "Idempotency-Key");
    assert!(params.contains_key("Signature"));
    assert!(params.contains_key("SignatureInput"));
    assert!(params.contains_key("SignatureAgent"));

    // 6. Security Schemes (FR-6)
    let security_schemes = components
        .security_schemes
        .expect("securitySchemes must be present");
    assert!(security_schemes.contains_key("HttpSignatureAuth"));
    assert_eq!(security_schemes["HttpSignatureAuth"].type_, "http");
    assert_eq!(
        security_schemes["HttpSignatureAuth"].scheme.as_deref(),
        Some("signature")
    );

    assert!(security_schemes.contains_key("BearerAuth"));
    assert_eq!(security_schemes["BearerAuth"].type_, "http");
    assert_eq!(
        security_schemes["BearerAuth"].scheme.as_deref(),
        Some("bearer")
    );

    // 7. Route & Operation Projections (FR-5)
    assert!(doc.paths.contains_key("/checkout-sessions"));
    assert!(doc.paths.contains_key("/checkout-sessions/{id}"));
    assert!(doc.paths.contains_key("/checkout-sessions/{id}/complete"));
    assert!(doc.paths.contains_key("/checkout-sessions/{id}/cancel"));

    // Ensure no phantom routes exist for auxiliary/helper types
    assert!(!doc.paths.contains_key("/checkouts"));
    assert!(!doc.paths.contains_key("/checkouts/{id}"));
    assert!(!doc.paths.contains_key("/lineitems"));
    assert!(!doc.paths.contains_key("/lineitems/{id}"));
    assert!(!doc.paths.contains_key("/locationsummaries"));
    assert!(!doc.paths.contains_key("/locationsummaries/{id}"));
    assert!(!doc.paths.contains_key("/totals"));
    assert!(!doc.paths.contains_key("/totals/{id}"));
    assert!(!doc.paths.contains_key("/shippingdestinations"));
    assert!(!doc.paths.contains_key("/fulfillmentdestinations"));

    let collection_path = &doc.paths["/checkout-sessions"];
    let post_op = collection_path
        .post
        .as_ref()
        .expect("POST /checkout-sessions must exist");
    assert_eq!(post_op.operation_id.as_deref(), Some("createCheckout"));

    // Parameters include UCP-Agent, Idempotency-Key, Signature
    let post_params = post_op.parameters.as_ref().unwrap();
    let post_param_refs: Vec<String> = post_params
        .iter()
        .filter_map(|p| match p {
            ucp_schema::openapi::ParameterOrRef::Ref { reference } => Some(reference.clone()),
            _ => None,
        })
        .collect();
    assert!(post_param_refs.contains(&"#/components/parameters/UcpAgent".to_string()));
    assert!(post_param_refs.contains(&"#/components/parameters/IdempotencyKey".to_string()));
    assert!(post_param_refs.contains(&"#/components/parameters/Signature".to_string()));

    // Request body bound to CheckoutCreateRequest
    let req_body = post_op.request_body.as_ref().unwrap();
    match req_body {
        ucp_schema::openapi::RequestBodyOrRef::Item(rb) => {
            let json_content = &rb.content["application/json"];
            assert_eq!(
                json_content.schema,
                Some(json!({ "$ref": "#/components/schemas/CheckoutCreateRequest" }))
            );
        }
        _ => panic!("Expected inline RequestBody"),
    }

    // Success response 201 bound to Checkout
    let resp_201 = &post_op.responses["201"];
    match resp_201 {
        ucp_schema::openapi::ResponseItemOrRef::Item(r) => {
            let content = r.content.as_ref().unwrap();
            assert_eq!(
                content["application/json"].schema,
                Some(json!({ "$ref": "#/components/schemas/Checkout" }))
            );
        }
        _ => panic!("Expected inline ResponseItem"),
    }

    let item_path = &doc.paths["/checkout-sessions/{id}"];
    let get_op = item_path
        .get
        .as_ref()
        .expect("GET /checkout-sessions/{id} must exist");
    assert_eq!(get_op.operation_id.as_deref(), Some("getCheckout"));

    let put_op = item_path
        .put
        .as_ref()
        .expect("PUT /checkout-sessions/{id} must exist");
    assert_eq!(put_op.operation_id.as_deref(), Some("updateCheckout"));

    // Complete operation: POST /checkout-sessions/{id}/complete
    let complete_path = &doc.paths["/checkout-sessions/{id}/complete"];
    let complete_op = complete_path
        .post
        .as_ref()
        .expect("POST /checkout-sessions/{id}/complete must exist");
    assert_eq!(
        complete_op.operation_id.as_deref(),
        Some("completeCheckout")
    );

    // Cancel operation: POST /checkout-sessions/{id}/cancel
    let cancel_path = &doc.paths["/checkout-sessions/{id}/cancel"];
    let cancel_op = cancel_path
        .post
        .as_ref()
        .expect("POST /checkout-sessions/{id}/cancel must exist");
    assert_eq!(cancel_op.operation_id.as_deref(), Some("cancelCheckout"));
}

#[test]
fn test_export_openapi_cli_stdout_and_file() {
    let (temp_dir, schema_path) = setup_test_schemas();
    let schema_dir = schema_path.to_str().unwrap();
    let output_file = temp_dir.path().join("dist").join("openapi.json");

    // 1. Test CLI file export
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("ucp-schema"));
    cmd.args([
        "export-openapi",
        "--schema-dir",
        schema_dir,
        "--output",
        output_file.to_str().unwrap(),
        "--title",
        "Test CLI API",
        "--api-version",
        "2026-09-01",
    ]);
    cmd.assert().success();

    assert!(output_file.exists());
    let file_content = fs::read_to_string(&output_file).unwrap();
    let doc_json: Value = serde_json::from_str(&file_content).unwrap();
    assert_eq!(doc_json["openapi"], "3.1.0");
    assert_eq!(doc_json["info"]["title"], "Test CLI API");
    assert!(
        doc_json["components"]["schemas"]["FulfillmentDestination"]["discriminator"].is_object()
    );

    // 2. Test CLI stdout export
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("ucp-schema"));
    let assert = cmd
        .args([
            "export-openapi",
            "--schema-dir",
            schema_dir,
            "--title",
            "Stdout API",
        ])
        .assert()
        .success();

    let stdout_str = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let stdout_json: Value = serde_json::from_str(&stdout_str).unwrap();
    assert_eq!(stdout_json["info"]["title"], "Stdout API");

    // 3. Test CLI --check mode (up-to-date passes)
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("ucp-schema"));
    cmd.args([
        "export-openapi",
        "--schema-dir",
        schema_dir,
        "--output",
        output_file.to_str().unwrap(),
        "--title",
        "Test CLI API",
        "--api-version",
        "2026-09-01",
        "--check",
    ]);
    cmd.assert().success();

    // 4. Test CLI --check mode (drift fails with exit code 1)
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("ucp-schema"));
    cmd.args([
        "export-openapi",
        "--schema-dir",
        schema_dir,
        "--output",
        output_file.to_str().unwrap(),
        "--title",
        "Drifted API Title",
        "--api-version",
        "2026-09-01",
        "--check",
    ]);
    cmd.assert().failure().code(1);
}

#[test]
fn test_export_openapi_real_schemas_if_present() {
    let repo_schemas = std::env::var("UCP_SCHEMAS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            let draft_path = manifest_dir.join("../ucp/local_preview/draft/schemas");
            if draft_path.exists() {
                draft_path
            } else {
                manifest_dir.join("../ucp/source/schemas")
            }
        });
    if !repo_schemas.exists() {
        return;
    }

    let options = ExportOpenApiOptions::new(&repo_schemas)
        .title("UCP Canonical Shopping API")
        .api_version("2026-09-01")
        .description(Some("Exported from UCP Draft 2020-12 Schemas".into()));

    let doc = export_openapi(&options).expect("Export on repo schemas must succeed");
    assert_eq!(doc.openapi, "3.1.0");

    let components = doc.components.unwrap();
    assert!(components.schemas.len() > 10);
    assert!(components.schemas.contains_key("Checkout"));
    assert!(components.schemas.contains_key("CheckoutCreateRequest"));
    assert!(components.schemas.contains_key("FulfillmentDestination"));
    assert!(components.schemas.contains_key("Totals"));

    let checkout = &components.schemas["Checkout"];
    assert!(
        checkout["properties"].get("discounts").is_some(),
        "Checkout in repo schemas must have composed discounts"
    );
    assert!(
        checkout["properties"].get("fulfillment").is_some(),
        "Checkout in repo schemas must have composed fulfillment"
    );

    let fulfillment_dest = &components.schemas["FulfillmentDestination"];
    assert!(fulfillment_dest.get("discriminator").is_some());
    assert_eq!(fulfillment_dest["discriminator"]["propertyName"], "type");
}

#[test]
fn test_export_openapi_strict_mode() {
    let (_temp_dir, schema_dir) = setup_test_schemas();

    let options = ExportOpenApiOptions::new(schema_dir)
        .title("Strict UCP API")
        .strict(true);

    let doc = export_openapi(&options).expect("Strict export should succeed");
    let components = doc.components.unwrap();

    let create_req = &components.schemas["CheckoutCreateRequest"];
    assert_eq!(create_req["additionalProperties"], false);

    let update_req = &components.schemas["CheckoutUpdateRequest"];
    assert_eq!(update_req["additionalProperties"], false);
}

#[test]
fn test_export_openapi_multi_capability() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("schemas");
    fs::create_dir_all(&root).unwrap();

    // Create Cart and Order schemas
    fs::write(
        root.join("cart.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://ucp.dev/draft/schemas/shopping/cart.json",
            "name": "dev.ucp.shopping.cart",
            "title": "Cart",
            "type": "object",
            "required": ["id", "items"],
            "properties": {
                "id": {
                    "type": "string",
                    "ucp_request": { "create": "omit", "update": "required" }
                },
                "items": {
                    "type": "array",
                    "items": { "type": "string" },
                    "ucp_request": "required"
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    fs::write(
        root.join("order.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://ucp.dev/draft/schemas/shopping/order.json",
            "name": "dev.ucp.shopping.order",
            "title": "Order",
            "type": "object",
            "required": ["id", "order_number", "status"],
            "properties": {
                "id": { "type": "string" },
                "order_number": { "type": "string" },
                "status": { "type": "string" }
            }
        })
        .to_string(),
    )
    .unwrap();

    let options = ExportOpenApiOptions::new(root);
    let doc = export_openapi(&options).unwrap();

    // Both Cart and Order paths projected
    assert!(doc.paths.contains_key("/carts"));
    assert!(doc.paths.contains_key("/carts/{id}"));
    assert!(doc.paths.contains_key("/orders/{id}"));

    let components = doc.components.unwrap();
    assert!(components.schemas.contains_key("CartCreateRequest"));
    assert!(components.schemas.contains_key("CartUpdateRequest"));
    assert!(components.schemas.contains_key("Cart"));
    assert!(components.schemas.contains_key("Order"));
}

#[test]
fn test_export_openapi_hoisted_defs_self_ref_rewriting() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("schemas");
    fs::create_dir_all(&root).unwrap();

    fs::write(
        root.join("payment_instrument.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://ucp.dev/draft/schemas/shopping/types/payment_instrument.json",
            "title": "Payment Instrument",
            "type": "object",
            "required": ["id", "type"],
            "properties": {
                "id": { "type": "string" },
                "type": { "type": "string" }
            },
            "$defs": {
                "selected_payment_instrument": {
                    "title": "Selected Payment Instrument",
                    "allOf": [
                        { "$ref": "#" },
                        {
                            "type": "object",
                            "properties": {
                                "selected": { "type": "boolean" }
                            }
                        }
                    ]
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let options = ExportOpenApiOptions::new(root);
    let doc = export_openapi(&options).unwrap();
    let components = doc.components.unwrap();

    assert!(components.schemas.contains_key("PaymentInstrument"));
    assert!(components.schemas.contains_key("SelectedPaymentInstrument"));

    let selected = &components.schemas["SelectedPaymentInstrument"];
    let all_of = selected["allOf"].as_array().unwrap();
    assert_eq!(all_of[0]["$ref"], "#/components/schemas/PaymentInstrument");
}

#[test]
fn test_export_openapi_message_oneof_discriminator() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("schemas");
    fs::create_dir_all(&root).unwrap();

    fs::write(
        root.join("message.json"),
        json!({
            "title": "Message",
            "type": "object",
            "oneOf": [
                { "$ref": "message_error.json" },
                { "$ref": "message_warning.json" },
                { "$ref": "message_info.json" }
            ]
        })
        .to_string(),
    )
    .unwrap();

    fs::write(
        root.join("message_error.json"),
        json!({
            "title": "Message Error",
            "type": "object",
            "required": ["type", "code", "content"],
            "properties": {
                "type": { "type": "string", "const": "error" },
                "code": { "type": "string" },
                "content": { "type": "string" }
            }
        })
        .to_string(),
    )
    .unwrap();

    fs::write(
        root.join("message_warning.json"),
        json!({
            "title": "Message Warning",
            "type": "object",
            "required": ["type", "code", "content"],
            "properties": {
                "type": { "type": "string", "const": "warning" },
                "code": { "type": "string" },
                "content": { "type": "string" }
            }
        })
        .to_string(),
    )
    .unwrap();

    fs::write(
        root.join("message_info.json"),
        json!({
            "title": "Message Info",
            "type": "object",
            "required": ["type", "content"],
            "properties": {
                "type": { "type": "string", "const": "info" },
                "content": { "type": "string" }
            }
        })
        .to_string(),
    )
    .unwrap();

    let options = ExportOpenApiOptions::new(root);
    let doc = export_openapi(&options).unwrap();
    let components = doc.components.unwrap();

    let message = &components.schemas["Message"];
    assert!(message.get("discriminator").is_some());
    let disc = &message["discriminator"];
    assert_eq!(disc["propertyName"], "type");
    assert_eq!(
        disc["mapping"]["error"],
        "#/components/schemas/MessageError"
    );
    assert_eq!(
        disc["mapping"]["warning"],
        "#/components/schemas/MessageWarning"
    );
    assert_eq!(disc["mapping"]["info"], "#/components/schemas/MessageInfo");
}

#[test]
fn test_export_openapi_value_constraint_properties_distribution() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("schemas");
    fs::create_dir_all(&root).unwrap();

    fs::write(
        root.join("constraint_expression.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://ucp.dev/draft/schemas/common/types/constraint_expression.json",
            "title": "Constraint Expression",
            "type": "object",
            "properties": {
                "required": {
                    "type": "array",
                    "items": { "type": "string" }
                },
                "properties": {
                    "type": "object",
                    "additionalProperties": {
                        "oneOf": [
                            { "$ref": "#" },
                            { "$ref": "#/$defs/value_constraint" }
                        ]
                    }
                }
            },
            "$defs": {
                "value_constraint": {
                    "title": "Value Constraint",
                    "type": "object",
                    "anyOf": [
                        { "required": ["enum"] },
                        { "required": ["const"] }
                    ],
                    "properties": {
                        "enum": {
                            "type": "array",
                            "minItems": 1
                        },
                        "const": {}
                    },
                    "additionalProperties": false
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let options = ExportOpenApiOptions::new(root);
    let doc = export_openapi(&options).unwrap();
    let components = doc.components.unwrap();

    assert!(components.schemas.contains_key("ConstraintExpression"));
    assert!(components.schemas.contains_key("ValueConstraint"));

    let value_constraint = &components.schemas["ValueConstraint"];
    let any_of = value_constraint["anyOf"].as_array().unwrap();
    assert_eq!(any_of.len(), 2);

    // Each branch in anyOf must now have concrete properties distributed
    assert_eq!(any_of[0]["type"], "object");
    assert!(any_of[0]["properties"].get("enum").is_some());
    assert!(any_of[0]["properties"].get("const").is_some());
    assert_eq!(any_of[0]["additionalProperties"], false);

    assert_eq!(any_of[1]["type"], "object");
    assert!(any_of[1]["properties"].get("enum").is_some());
    assert!(any_of[1]["properties"].get("const").is_some());
    assert_eq!(any_of[1]["additionalProperties"], false);
}

#[test]
fn test_export_openapi_downscoping_merchant_discovery_profile() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("schemas");
    let shopping_dir = root.join("shopping");
    let discovery_dir = root.join("discovery");
    fs::create_dir_all(&shopping_dir).unwrap();
    fs::create_dir_all(&discovery_dir).unwrap();

    // 1. Discovery Schemas: profile.json, catalog_search.json, product.json
    fs::write(
        discovery_dir.join("profile.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://ucp.dev/draft/schemas/discovery/profile.json",
            "title": "Profile",
            "type": "object",
            "required": ["version", "capabilities"],
            "properties": {
                "version": { "type": "string" },
                "capabilities": { "type": "array" }
            }
        })
        .to_string(),
    )
    .unwrap();

    fs::write(
        discovery_dir.join("product.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://ucp.dev/draft/schemas/discovery/product.json",
            "title": "Product",
            "type": "object",
            "required": ["id", "title"],
            "properties": {
                "id": { "type": "string" },
                "title": { "type": "string" }
            }
        })
        .to_string(),
    )
    .unwrap();

    fs::write(
        discovery_dir.join("catalog_search.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://ucp.dev/draft/schemas/discovery/catalog_search.json",
            "title": "Catalog Search",
            "type": "object",
            "$defs": {
                "search_request": {
                    "title": "Search Request",
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" }
                    }
                },
                "search_response": {
                    "title": "Search Response",
                    "type": "object",
                    "properties": {
                        "results": {
                            "type": "array",
                            "items": { "$ref": "product.json" }
                        }
                    }
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    // 2. Shopping Schemas: checkout.json and cart.json
    fs::write(
        shopping_dir.join("checkout.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://ucp.dev/draft/schemas/shopping/checkout.json",
            "name": "dev.ucp.shopping.checkout",
            "title": "Checkout",
            "type": "object",
            "x-ucp-path": "/checkout-sessions",
            "x-ucp-lifecycle": ["complete", "cancel"],
            "required": ["id", "line_items"],
            "properties": {
                "id": { "type": "string", "ucp_request": { "create": "omit", "update": "required" } },
                "line_items": { "type": "array", "ucp_request": "required" }
            }
        })
        .to_string(),
    )
    .unwrap();

    fs::write(
        shopping_dir.join("cart.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://ucp.dev/draft/schemas/shopping/cart.json",
            "name": "dev.ucp.shopping.cart",
            "title": "Cart",
            "type": "object",
            "x-ucp-lifecycle": ["cancel"],
            "required": ["id", "items"],
            "properties": {
                "id": { "type": "string", "ucp_request": { "create": "omit", "update": "required" } },
                "items": { "type": "array", "items": { "type": "string" }, "ucp_request": "required" }
            }
        })
        .to_string(),
    )
    .unwrap();

    // A. Baseline: Export without profile filter (all capabilities included)
    let full_options = ExportOpenApiOptions::new(&root);
    let full_doc = export_openapi(&full_options).expect("Full export must succeed");
    let full_schemas = full_doc
        .components
        .as_ref()
        .unwrap()
        .schemas
        .keys()
        .cloned()
        .collect::<Vec<_>>();

    assert!(full_doc.paths.contains_key("/checkout-sessions"));
    assert!(full_doc.paths.contains_key("/carts"));
    assert!(full_doc.paths.contains_key("/catalog/search"));
    assert!(full_doc.paths.contains_key("/.well-known/ucp"));
    assert!(full_schemas.contains(&"Checkout".to_string()));
    assert!(full_schemas.contains(&"CheckoutCreateRequest".to_string()));
    assert!(full_schemas.contains(&"Cart".to_string()));
    assert!(full_schemas.contains(&"Profile".to_string()));
    assert!(full_schemas.contains(&"Product".to_string()));

    // B. Down-scoping: Merchant chooses to ONLY implement Discovery (no Cart or Checkout)
    let discovery_options = ExportOpenApiOptions::new(&root)
        .profile(Some("discovery".to_string()))
        .title("Merchant Discovery API")
        .api_version("2026-09-01");
    let discovery_doc = export_openapi(&discovery_options).expect("Discovery export must succeed");
    let discovery_schemas = discovery_doc
        .components
        .as_ref()
        .unwrap()
        .schemas
        .keys()
        .cloned()
        .collect::<Vec<_>>();

    // 1. Discovery operations and schemas MUST be present
    assert!(discovery_doc.paths.contains_key("/.well-known/ucp"));
    assert!(discovery_doc.paths.contains_key("/catalog/search"));
    assert!(discovery_schemas.contains(&"Profile".to_string()));
    assert!(discovery_schemas.contains(&"Product".to_string()));
    assert!(discovery_schemas.contains(&"SearchRequest".to_string()));
    assert!(discovery_schemas.contains(&"SearchResponse".to_string()));

    // 2. Shopping/Checkout/Cart operations and schemas MUST be strictly omitted
    assert!(!discovery_doc.paths.contains_key("/checkout-sessions"));
    assert!(!discovery_doc.paths.contains_key("/checkout-sessions/{id}"));
    assert!(!discovery_doc
        .paths
        .contains_key("/checkout-sessions/{id}/complete"));
    assert!(!discovery_doc
        .paths
        .contains_key("/checkout-sessions/{id}/cancel"));
    assert!(!discovery_doc.paths.contains_key("/carts"));
    assert!(!discovery_doc.paths.contains_key("/carts/{id}"));

    assert!(!discovery_schemas.contains(&"Checkout".to_string()));
    assert!(!discovery_schemas.contains(&"CheckoutCreateRequest".to_string()));
    assert!(!discovery_schemas.contains(&"CheckoutUpdateRequest".to_string()));
    assert!(!discovery_schemas.contains(&"Cart".to_string()));
    assert!(!discovery_schemas.contains(&"CartCreateRequest".to_string()));
    assert!(!discovery_schemas.contains(&"CartUpdateRequest".to_string()));

    // C. CLI Verification: Test downscoping via `--profile discovery`
    let output_file = temp_dir.path().join("dist").join("discovery.openapi.json");
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("ucp-schema"));
    cmd.args([
        "export-openapi",
        "--schema-dir",
        root.to_str().unwrap(),
        "--profile",
        "discovery",
        "--output",
        output_file.to_str().unwrap(),
    ]);
    cmd.assert().success();

    let file_content = fs::read_to_string(&output_file).unwrap();
    let doc_json: Value = serde_json::from_str(&file_content).unwrap();
    let paths_obj = doc_json["paths"].as_object().unwrap();
    assert!(paths_obj.contains_key("/.well-known/ucp"));
    assert!(paths_obj.contains_key("/catalog/search"));
    assert!(!paths_obj.contains_key("/checkout-sessions"));
    assert!(!paths_obj.contains_key("/carts"));
}

#[test]
fn test_export_openapi_capability_composition() {
    let temp_dir = TempDir::new().unwrap();
    let schema_dir = temp_dir.path().join("schemas");
    let shopping_dir = schema_dir.join("shopping");
    fs::create_dir_all(&shopping_dir).unwrap();

    // 1. Root capability schema: checkout.json
    fs::write(
        shopping_dir.join("checkout.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://ucp.dev/schemas/shopping/checkout.json",
            "name": "dev.ucp.shopping.checkout",
            "title": "Checkout",
            "description": "Base checkout session resource.",
            "type": "object",
            "x-ucp-path": "/checkout-sessions",
            "x-ucp-lifecycle": ["complete", "cancel"],
            "required": ["id", "currency"],
            "properties": {
                "id": {
                    "type": "string",
                    "ucp_request": {
                        "create": "omit",
                        "update": "required"
                    }
                },
                "currency": {
                    "type": "string",
                    "ucp_request": {
                        "create": "required",
                        "update": "optional"
                    }
                },
                "status": {
                    "type": "string",
                    "ucp_request": "omit"
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    // 2. Capability extension 1: discount.json
    fs::write(
        shopping_dir.join("discount.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://ucp.dev/schemas/shopping/discount.json",
            "name": "dev.ucp.shopping.discount",
            "title": "Discount Extension",
            "$defs": {
                "dev.ucp.shopping.checkout": {
                    "title": "Checkout with Discount",
                    "allOf": [
                        { "$ref": "checkout.json" },
                        {
                            "type": "object",
                            "properties": {
                                "discounts": {
                                    "$ref": "#/$defs/discounts_object",
                                    "ucp_request": {
                                        "create": "optional",
                                        "update": "optional",
                                        "complete": "omit"
                                    }
                                }
                            }
                        }
                    ]
                },
                "discounts_object": {
                    "type": "object",
                    "title": "Discounts Object",
                    "properties": {
                        "codes": {
                            "type": "array",
                            "items": { "type": "string" }
                        }
                    }
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    // 3. Capability extension 2: fulfillment.json
    fs::write(
        shopping_dir.join("fulfillment.json"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://ucp.dev/schemas/shopping/fulfillment.json",
            "name": "dev.ucp.shopping.fulfillment",
            "title": "Fulfillment Extension",
            "$defs": {
                "dev.ucp.shopping.checkout": {
                    "title": "Checkout with Fulfillment",
                    "allOf": [
                        { "$ref": "checkout.json" },
                        {
                            "type": "object",
                            "properties": {
                                "fulfillment": {
                                    "$ref": "#/$defs/fulfillment_details",
                                    "ucp_request": {
                                        "create": "optional",
                                        "update": "optional",
                                        "complete": "omit"
                                    }
                                }
                            }
                        }
                    ]
                },
                "fulfillment_details": {
                    "type": "object",
                    "title": "Fulfillment Details",
                    "properties": {
                        "method": { "type": "string" }
                    }
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    // Export OpenAPI with full capability composition
    let options = ExportOpenApiOptions::new(schema_dir)
        .title("UCP Composed Shopping API")
        .api_version("2026-09-01");
    let doc = export_openapi(&options).expect("Export with composition must succeed");
    let schemas = &doc.components.as_ref().unwrap().schemas;

    // A. Verify hoisted defs exist under components.schemas
    assert!(
        schemas.contains_key("DiscountsObject"),
        "DiscountsObject must be hoisted to components.schemas"
    );
    assert!(
        schemas.contains_key("FulfillmentDetails"),
        "FulfillmentDetails must be hoisted to components.schemas"
    );

    // B. Verify Checkout response model contains composed extensions
    let checkout = &schemas["Checkout"];
    let checkout_props = checkout["properties"]
        .as_object()
        .expect("Checkout must have properties");
    assert!(checkout_props.contains_key("id"), "Checkout must have id");
    assert!(
        checkout_props.contains_key("currency"),
        "Checkout must have currency"
    );
    assert!(
        checkout_props.contains_key("discounts"),
        "Checkout must have composed discounts property"
    );
    assert!(
        checkout_props.contains_key("fulfillment"),
        "Checkout must have composed fulfillment property"
    );
    assert_eq!(
        checkout_props["discounts"]["$ref"],
        "#/components/schemas/DiscountsObject"
    );
    assert_eq!(
        checkout_props["fulfillment"]["$ref"],
        "#/components/schemas/FulfillmentDetails"
    );

    // C. Verify CheckoutCreateRequest contains composed extensions
    let checkout_create = &schemas["CheckoutCreateRequest"];
    let create_props = checkout_create["properties"]
        .as_object()
        .expect("CheckoutCreateRequest must have properties");
    assert!(create_props.contains_key("currency"));
    assert!(
        create_props.contains_key("discounts"),
        "CheckoutCreateRequest must have composed discounts"
    );
    assert!(
        create_props.contains_key("fulfillment"),
        "CheckoutCreateRequest must have composed fulfillment"
    );
    assert_eq!(
        create_props["discounts"]["$ref"],
        "#/components/schemas/DiscountsObject"
    );
    assert_eq!(
        create_props["fulfillment"]["$ref"],
        "#/components/schemas/FulfillmentDetails"
    );

    // D. Verify CheckoutUpdateRequest contains composed extensions
    let checkout_update = &schemas["CheckoutUpdateRequest"];
    let update_props = checkout_update["properties"]
        .as_object()
        .expect("CheckoutUpdateRequest must have properties");
    assert!(update_props.contains_key("id"));
    assert!(
        update_props.contains_key("discounts"),
        "CheckoutUpdateRequest must have composed discounts"
    );
    assert!(
        update_props.contains_key("fulfillment"),
        "CheckoutUpdateRequest must have composed fulfillment"
    );

    // E. Verify CheckoutCompleteRequest does NOT contain discounts or fulfillment (annotated complete: omit)
    let checkout_complete = &schemas["CheckoutCompleteRequest"];
    let complete_props = checkout_complete["properties"]
        .as_object()
        .expect("CheckoutCompleteRequest must have properties");
    assert!(
        complete_props.get("discounts").is_none(),
        "complete: omit must omit discounts from CompleteRequest"
    );
    assert!(
        complete_props.get("fulfillment").is_none(),
        "complete: omit must omit fulfillment from CompleteRequest"
    );

    // F. Verify routes use x-ucp-path and lifecycle actions
    assert!(doc.paths.contains_key("/checkout-sessions"));
    assert!(doc.paths.contains_key("/checkout-sessions/{id}"));
    assert!(doc.paths.contains_key("/checkout-sessions/{id}/complete"));
    assert!(doc.paths.contains_key("/checkout-sessions/{id}/cancel"));
}

#[test]
fn test_export_openapi_directional_union_and_metadata_invariants() {
    let schema_dir = std::path::Path::new("../ucp/source/schemas");
    if !schema_dir.exists() {
        return;
    }

    let options = ExportOpenApiOptions::new(schema_dir)
        .profile(Some("shopping".to_string()))
        .title("UCP Shopping API")
        .api_version("2026-09-01");
    let doc = export_openapi(&options).expect("Export shopping API must succeed");
    let schemas = &doc.components.as_ref().unwrap().schemas;

    // 1. Sliced request metadata
    let checkout_create = &schemas["CheckoutCreateRequest"];
    assert_eq!(
        checkout_create["title"].as_str().unwrap(),
        "CheckoutCreateRequest"
    );
    assert!(
        checkout_create["description"]
            .as_str()
            .unwrap()
            .starts_with("Request payload to create a new Checkout"),
        "Title and description must be specialized for CreateRequest"
    );

    let checkout_update = &schemas["CheckoutUpdateRequest"];
    assert_eq!(
        checkout_update["title"].as_str().unwrap(),
        "CheckoutUpdateRequest"
    );

    // 2. Sliced polymorphic unions & discriminator mapping alignment
    let fdc = &schemas["FulfillmentDestinationCreateRequest"];
    assert_eq!(
        fdc["title"].as_str().unwrap(),
        "FulfillmentDestinationCreateRequest"
    );
    let one_of = fdc["oneOf"].as_array().expect("oneOf must be an array");
    let one_of_refs: Vec<&str> = one_of
        .iter()
        .map(|item| item["$ref"].as_str().unwrap())
        .collect();
    assert_eq!(
        one_of_refs,
        vec![
            "#/components/schemas/LocationDestinationCreateRequest",
            "#/components/schemas/ShippingDestinationCreateRequest"
        ]
    );

    let disc_mapping = fdc["discriminator"]["mapping"]
        .as_object()
        .expect("discriminator.mapping must exist");
    assert_eq!(
        disc_mapping["business_location"].as_str().unwrap(),
        "#/components/schemas/LocationDestinationCreateRequest"
    );
    assert_eq!(
        disc_mapping["shipping_address"].as_str().unwrap(),
        "#/components/schemas/ShippingDestinationCreateRequest"
    );

    // 3. Invariant: empty container schemas pruned
    assert!(!schemas.contains_key("CatalogSearch"));
    assert!(!schemas.contains_key("CatalogLookup"));
    assert!(!schemas.contains_key("Pagination"));

    // 4. Invariant: no empty object schemas emitted
    for (name, schema) in schemas {
        if schema.get("type").and_then(|t| t.as_str()) == Some("object") {
            let has_props = schema
                .get("properties")
                .and_then(|p| p.as_object())
                .map(|p| !p.is_empty())
                .unwrap_or(false);
            let has_composition = schema.get("allOf").is_some()
                || schema.get("oneOf").is_some()
                || schema.get("anyOf").is_some()
                || schema.get("$ref").is_some();
            let has_additional = schema
                .get("additionalProperties")
                .map(|v| v.is_object())
                .unwrap_or(false);
            assert!(
                has_props || has_composition || has_additional,
                "Schema '{}' is an empty object without properties or composition",
                name
            );
        }
    }

    // 5. Invariant: zero broken $refs in entire document
    let doc_json = serde_json::to_value(&doc).unwrap();
    let mut refs = Vec::new();
    fn collect_refs(val: &serde_json::Value, refs: &mut Vec<String>) {
        match val {
            serde_json::Value::Object(map) => {
                if let Some(r) = map.get("$ref").and_then(|v| v.as_str()) {
                    refs.push(r.to_string());
                }
                for v in map.values() {
                    collect_refs(v, refs);
                }
            }
            serde_json::Value::Array(arr) => {
                for v in arr {
                    collect_refs(v, refs);
                }
            }
            _ => {}
        }
    }
    collect_refs(&doc_json, &mut refs);

    for r in refs {
        if let Some(target) = r.strip_prefix("#/components/schemas/") {
            assert!(
                schemas.contains_key(target),
                "Dangling $ref target: {}",
                target
            );
        }
    }
}
