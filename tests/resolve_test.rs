//! Integration tests for schema resolution.

use serde_json::{json, Value};
use ucp_schema::{resolve, Direction, ResolveError, ResolveOptions};

// === Visibility Parsing Tests ===

mod visibility_parsing {
    use super::*;

    #[test]
    fn shorthand_string() {
        let schema = json!({
            "type": "object",
            "properties": {
                "status": { "type": "string", "ucp_request": "omit" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        assert!(result["properties"].get("status").is_none());
    }

    #[test]
    fn object_form() {
        let schema = json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "ucp_request": {
                        "create": "omit",
                        "update": "required"
                    }
                }
            }
        });

        // For create: id should be omitted
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();
        assert!(result["properties"].get("id").is_none());

        // For update: id should be required
        let options = ResolveOptions::new(Direction::Request, "update");
        let result = resolve(&schema, &options).unwrap();
        assert!(result["properties"].get("id").is_some());
        assert!(result["required"]
            .as_array()
            .unwrap()
            .contains(&json!("id")));
    }

    #[test]
    fn missing_annotation_defaults_to_include() {
        let schema = json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": { "type": "string" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        // Field should be included, required preserved
        assert!(result["properties"].get("name").is_some());
        assert!(result["required"]
            .as_array()
            .unwrap()
            .contains(&json!("name")));
    }

    #[test]
    fn missing_operation_defaults_to_include() {
        let schema = json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "ucp_request": { "create": "omit" }
                }
            }
        });

        // "read" not in dict, should default to include
        let options = ResolveOptions::new(Direction::Request, "read");
        let result = resolve(&schema, &options).unwrap();
        assert!(result["properties"].get("id").is_some());
    }

    #[test]
    fn both_request_and_response_annotations() {
        let schema = json!({
            "type": "object",
            "properties": {
                "context": {
                    "type": "object",
                    "ucp_request": "optional",
                    "ucp_response": "omit"
                }
            }
        });

        // Request: should be present
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();
        assert!(result["properties"].get("context").is_some());

        // Response: should be omitted
        let options = ResolveOptions::new(Direction::Response, "create");
        let result = resolve(&schema, &options).unwrap();
        assert!(result["properties"].get("context").is_none());
    }
}

// === Error Handling Tests ===

mod error_handling {
    use super::*;

    #[test]
    fn invalid_annotation_type_errors() {
        let schema = json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "ucp_request": 123 }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options);

        assert!(matches!(
            result,
            Err(ResolveError::InvalidAnnotationType { .. })
        ));
    }

    #[test]
    fn unknown_visibility_errors() {
        let schema = json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "ucp_request": "readonly" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options);

        assert!(matches!(
            result,
            Err(ResolveError::UnknownVisibility { value, .. }) if value == "readonly"
        ));
    }

    #[test]
    fn unknown_visibility_in_dict_errors() {
        let schema = json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "ucp_request": { "create": "maybe" }
                }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options);

        assert!(matches!(
            result,
            Err(ResolveError::UnknownVisibility { value, .. }) if value == "maybe"
        ));
    }
}

// === Operation Normalization Tests ===

mod operation_normalization {
    use super::*;

    #[test]
    fn operations_are_case_insensitive() {
        let schema = json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "ucp_request": { "create": "omit" }
                }
            }
        });

        // "Create" should match "create"
        let options = ResolveOptions::new(Direction::Request, "Create");
        let result = resolve(&schema, &options).unwrap();
        assert!(result["properties"].get("id").is_none());

        // "CREATE" should also match
        let options = ResolveOptions::new(Direction::Request, "CREATE");
        let result = resolve(&schema, &options).unwrap();
        assert!(result["properties"].get("id").is_none());
    }
}

// === Transformation Tests ===

mod transformation {
    use super::*;

    #[test]
    fn omit_removes_field_from_properties() {
        let schema = json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "ucp_request": "omit" },
                "name": { "type": "string" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        assert!(result["properties"].get("id").is_none());
        assert!(result["properties"].get("name").is_some());
    }

    #[test]
    fn omit_removes_field_from_required() {
        let schema = json!({
            "type": "object",
            "required": ["id", "name"],
            "properties": {
                "id": { "type": "string", "ucp_request": "omit" },
                "name": { "type": "string" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        let required = result["required"].as_array().unwrap();
        assert!(!required.contains(&json!("id")));
        assert!(required.contains(&json!("name")));
    }

    #[test]
    fn required_adds_to_required_array() {
        let schema = json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "ucp_request": "required" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        let required = result["required"].as_array().unwrap();
        assert!(required.contains(&json!("id")));
    }

    #[test]
    fn optional_removes_from_required_array() {
        let schema = json!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": { "type": "string", "ucp_request": "optional" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        let required = result["required"].as_array().unwrap();
        assert!(!required.contains(&json!("id")));
    }

    #[test]
    fn include_preserves_original_state() {
        let schema = json!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": { "type": "string" },
                "name": { "type": "string" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        // Both fields present
        assert!(result["properties"].get("id").is_some());
        assert!(result["properties"].get("name").is_some());

        // Required preserved
        let required = result["required"].as_array().unwrap();
        assert!(required.contains(&json!("id")));
        assert!(!required.contains(&json!("name")));
    }

    #[test]
    fn all_fields_omitted_yields_empty_schema() {
        let schema = json!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": { "type": "string", "ucp_request": "omit" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        assert_eq!(result["properties"], json!({}));
        assert_eq!(result["required"], json!([]));
    }

    #[test]
    fn annotations_stripped_from_output() {
        let schema = json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "ucp_request": "required",
                    "ucp_response": "omit"
                }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        // All UCP annotations should be stripped
        assert!(result["properties"]["id"].get("ucp_request").is_none());
        assert!(result["properties"]["id"].get("ucp_response").is_none());
    }
}

// === Required Array Tests ===

mod required_array {
    use super::*;

    #[test]
    fn omitted_field_removed_from_required() {
        let schema = json!({
            "type": "object",
            "required": ["id", "name", "email"],
            "properties": {
                "id": { "type": "string", "ucp_request": "omit" },
                "name": { "type": "string" },
                "email": { "type": "string" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        let required = result["required"].as_array().unwrap();
        assert!(!required.contains(&json!("id")));
        assert!(required.contains(&json!("name")));
        assert!(required.contains(&json!("email")));
    }

    #[test]
    fn required_field_added_to_required() {
        let schema = json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "id": { "type": "string", "ucp_request": "required" },
                "name": { "type": "string" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        let required = result["required"].as_array().unwrap();
        assert!(required.contains(&json!("id")));
        assert!(required.contains(&json!("name")));
    }

    #[test]
    fn unrelated_required_fields_preserved() {
        let schema = json!({
            "type": "object",
            "required": ["name", "email"],
            "properties": {
                "id": { "type": "string", "ucp_request": "omit" },
                "name": { "type": "string" },
                "email": { "type": "string" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        let required = result["required"].as_array().unwrap();
        assert!(required.contains(&json!("name")));
        assert!(required.contains(&json!("email")));
    }
}

// === Recursion Tests (Phase 2) ===

mod recursion {
    use super::*;

    #[test]
    fn nested_properties() {
        let schema = json!({
            "type": "object",
            "properties": {
                "buyer": {
                    "type": "object",
                    "properties": {
                        "email": {
                            "type": "string",
                            "ucp_request": { "create": "required" }
                        },
                        "phone": {
                            "type": "string",
                            "ucp_request": { "create": "omit" }
                        }
                    }
                }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        // Nested email should be present and required
        assert!(result["properties"]["buyer"]["properties"]
            .get("email")
            .is_some());
        let buyer_required = result["properties"]["buyer"]["required"]
            .as_array()
            .unwrap();
        assert!(buyer_required.contains(&json!("email")));

        // Nested phone should be omitted
        assert!(result["properties"]["buyer"]["properties"]
            .get("phone")
            .is_none());
    }

    #[test]
    fn array_items() {
        let schema = json!({
            "type": "object",
            "properties": {
                "line_items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "sku": { "type": "string", "ucp_request": "required" },
                            "price": { "type": "number", "ucp_request": "omit" }
                        }
                    }
                }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        // sku in items should be present and required
        assert!(result["properties"]["line_items"]["items"]["properties"]
            .get("sku")
            .is_some());

        // price in items should be omitted
        assert!(result["properties"]["line_items"]["items"]["properties"]
            .get("price")
            .is_none());
    }

    #[test]
    fn defs() {
        let schema = json!({
            "type": "object",
            "$defs": {
                "address": {
                    "type": "object",
                    "properties": {
                        "street": { "type": "string" },
                        "internal_id": { "type": "string", "ucp_request": "omit" }
                    }
                }
            },
            "properties": {
                "shipping": { "$ref": "#/$defs/address" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        // $defs should be transformed
        assert!(result["$defs"]["address"]["properties"]
            .get("street")
            .is_some());
        assert!(result["$defs"]["address"]["properties"]
            .get("internal_id")
            .is_none());
    }

    #[test]
    fn deep_nesting_five_levels() {
        let schema = json!({
            "type": "object",
            "properties": {
                "level1": {
                    "type": "object",
                    "properties": {
                        "level2": {
                            "type": "object",
                            "properties": {
                                "level3": {
                                    "type": "object",
                                    "properties": {
                                        "level4": {
                                            "type": "object",
                                            "properties": {
                                                "level5": {
                                                    "type": "string",
                                                    "ucp_request": "omit"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        // Deep nested field should be omitted
        assert!(
            result["properties"]["level1"]["properties"]["level2"]["properties"]["level3"]
                ["properties"]["level4"]["properties"]
                .get("level5")
                .is_none()
        );
    }
}

// === Composition Tests (Phase 2) ===

mod composition {
    use super::*;

    #[test]
    fn allof_transforms_each_branch() {
        let schema = json!({
            "allOf": [
                {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "ucp_request": "omit" }
                    }
                },
                {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "ucp_request": "required" }
                    }
                }
            ]
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        // First branch: id should be omitted
        assert!(result["allOf"][0]["properties"].get("id").is_none());

        // Second branch: name should be required
        assert!(result["allOf"][1]["properties"].get("name").is_some());
        assert!(result["allOf"][1]["required"]
            .as_array()
            .unwrap()
            .contains(&json!("name")));
    }

    #[test]
    fn allof_tighten_omit_to_required() {
        // Base has omit, extension adds required - should result in required (D2)
        let schema = json!({
            "allOf": [
                {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "ucp_request": { "create": "omit" } }
                    }
                },
                {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "ucp_request": { "create": "required" } }
                    }
                }
            ]
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        // First branch: id omitted
        assert!(result["allOf"][0]["properties"].get("id").is_none());

        // Second branch: id required - this "tightens" the constraint
        assert!(result["allOf"][1]["properties"].get("id").is_some());
        assert!(result["allOf"][1]["required"]
            .as_array()
            .unwrap()
            .contains(&json!("id")));
    }

    #[test]
    fn allof_loosen_required_to_omit_base_wins() {
        // Base has required, extension has omit - base wins (D2 limitation)
        let schema = json!({
            "allOf": [
                {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "ucp_request": { "create": "required" } }
                    }
                },
                {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "ucp_request": { "create": "omit" } }
                    }
                }
            ]
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        // First branch: id required (this wins due to JSON Schema allOf semantics)
        assert!(result["allOf"][0]["properties"].get("id").is_some());
        assert!(result["allOf"][0]["required"]
            .as_array()
            .unwrap()
            .contains(&json!("id")));

        // Second branch: id omitted from this branch
        assert!(result["allOf"][1]["properties"].get("id").is_none());

        // Note: JSON Schema validation will require id because allOf is conjunctive
    }

    #[test]
    fn anyof_transforms_each_branch() {
        let schema = json!({
            "anyOf": [
                {
                    "type": "object",
                    "properties": {
                        "card": { "type": "object", "ucp_request": "required" }
                    }
                },
                {
                    "type": "object",
                    "properties": {
                        "token": { "type": "string", "ucp_request": "required" }
                    }
                }
            ]
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        // Both branches should be transformed
        assert!(result["anyOf"][0]["properties"].get("card").is_some());
        assert!(result["anyOf"][1]["properties"].get("token").is_some());
    }

    #[test]
    fn oneof_transforms_each_branch() {
        let schema = json!({
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "type": { "const": "credit_card" },
                        "number": { "type": "string", "ucp_request": "required" }
                    }
                },
                {
                    "type": "object",
                    "properties": {
                        "type": { "const": "bank_account" },
                        "routing": { "type": "string", "ucp_request": "required" }
                    }
                }
            ]
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        // Both branches transformed
        assert!(result["oneOf"][0]["properties"].get("number").is_some());
        assert!(result["oneOf"][1]["properties"].get("routing").is_some());
    }
}

// === Additional Properties Tests (Phase 2) ===

mod additional_properties {
    use super::*;

    #[test]
    fn false_preserved_after_filtering() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "id": { "type": "string", "ucp_request": "omit" },
                "name": { "type": "string" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        // additionalProperties should remain false
        assert_eq!(result["additionalProperties"], json!(false));

        // id should be omitted
        assert!(result["properties"].get("id").is_none());
    }

    #[test]
    fn true_becomes_false_in_strict_mode() {
        // Strict mode changes true to false to reject unknown fields
        let schema = json!({
            "type": "object",
            "additionalProperties": true,
            "properties": {
                "id": { "type": "string", "ucp_request": "omit" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create").strict(true);
        let result = resolve(&schema, &options).unwrap();

        assert_eq!(result["additionalProperties"], json!(false));
    }

    #[test]
    fn true_unchanged_in_non_strict_mode() {
        // Non-strict mode preserves original additionalProperties
        let schema = json!({
            "type": "object",
            "additionalProperties": true,
            "properties": {
                "id": { "type": "string", "ucp_request": "omit" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create").strict(false);
        let result = resolve(&schema, &options).unwrap();

        assert_eq!(result["additionalProperties"], json!(true));
    }

    #[test]
    fn schema_form_transformed() {
        let schema = json!({
            "type": "object",
            "additionalProperties": {
                "type": "object",
                "properties": {
                    "internal": { "type": "string", "ucp_request": "omit" }
                }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        // The schema inside additionalProperties should be transformed
        assert!(result["additionalProperties"]["properties"]
            .get("internal")
            .is_none());
    }
}

// === Integration with real-world schema ===

mod integration {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn load_fixture(name: &str) -> Value {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        let content = fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("Failed to read fixture: {}", path.display()));
        serde_json::from_str(&content).expect("Failed to parse fixture JSON")
    }

    #[test]
    fn checkout_create_request() {
        let schema = load_fixture("checkout.json");
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        // id should be omitted for create
        assert!(result["properties"].get("id").is_none());

        // line_items should be required
        assert!(result["properties"].get("line_items").is_some());
        assert!(result["required"]
            .as_array()
            .unwrap()
            .contains(&json!("line_items")));

        // buyer should be optional (present but not required)
        assert!(result["properties"].get("buyer").is_some());
        assert!(!result["required"]
            .as_array()
            .unwrap()
            .contains(&json!("buyer")));

        // status should be omitted
        assert!(result["properties"].get("status").is_none());

        // totals should be omitted
        assert!(result["properties"].get("totals").is_none());
    }

    #[test]
    fn checkout_update_request() {
        let schema = load_fixture("checkout.json");
        let options = ResolveOptions::new(Direction::Request, "update");
        let result = resolve(&schema, &options).unwrap();

        // id should be required for update
        assert!(result["properties"].get("id").is_some());
        assert!(result["required"]
            .as_array()
            .unwrap()
            .contains(&json!("id")));

        // line_items should be optional for update
        assert!(result["properties"].get("line_items").is_some());
        assert!(!result["required"]
            .as_array()
            .unwrap()
            .contains(&json!("line_items")));
    }

    #[test]
    fn checkout_read_request() {
        let schema = load_fixture("checkout.json");
        let options = ResolveOptions::new(Direction::Request, "read");
        let result = resolve(&schema, &options).unwrap();

        // id should be required for read
        assert!(result["properties"].get("id").is_some());
        assert!(result["required"]
            .as_array()
            .unwrap()
            .contains(&json!("id")));

        // line_items should be omitted for read request
        assert!(result["properties"].get("line_items").is_none());

        // buyer should be omitted for read request
        assert!(result["properties"].get("buyer").is_none());
    }

    #[test]
    fn checkout_response() {
        let schema = load_fixture("checkout.json");
        let options = ResolveOptions::new(Direction::Response, "create");
        let result = resolve(&schema, &options).unwrap();

        // Response should include most fields (no ucp_response annotations except on some)
        assert!(result["properties"].get("id").is_some());
        assert!(result["properties"].get("status").is_some());
        assert!(result["properties"].get("totals").is_some());
    }

    #[test]
    fn invalid_annotation_type_from_file() {
        let schema = load_fixture("invalid/bad_annotation_type.json");
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options);

        assert!(matches!(
            result,
            Err(ResolveError::InvalidAnnotationType { .. })
        ));
    }

    #[test]
    fn unknown_visibility_from_file() {
        let schema = load_fixture("invalid/unknown_visibility.json");
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options);

        assert!(matches!(
            result,
            Err(ResolveError::UnknownVisibility { .. })
        ));
    }
}

// === Strict Mode Tests ===

mod strict_mode {
    use super::*;

    #[test]
    fn default_is_not_strict() {
        // Default options should have strict=false (respects schema extensibility)
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        // Should NOT have additionalProperties: false added
        assert!(result.get("additionalProperties").is_none());
    }

    #[test]
    fn injects_additional_properties_false() {
        // Object schemas without additionalProperties get false added in strict mode
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create").strict(true);
        let result = resolve(&schema, &options).unwrap();

        assert_eq!(result["additionalProperties"], json!(false));
    }

    #[test]
    fn preserves_explicit_false() {
        // Already false should stay false in strict mode
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "name": { "type": "string" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create").strict(true);
        let result = resolve(&schema, &options).unwrap();

        assert_eq!(result["additionalProperties"], json!(false));
    }

    #[test]
    fn preserves_custom_schema() {
        // Custom additionalProperties schema should be preserved even in strict mode
        let schema = json!({
            "type": "object",
            "additionalProperties": { "type": "string" },
            "properties": {
                "name": { "type": "string" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create").strict(true);
        let result = resolve(&schema, &options).unwrap();

        // Should preserve the schema, not replace with false
        assert_eq!(result["additionalProperties"], json!({ "type": "string" }));
    }

    #[test]
    fn applies_to_nested_objects() {
        // Nested objects should also get additionalProperties: false in strict mode
        let schema = json!({
            "type": "object",
            "properties": {
                "address": {
                    "type": "object",
                    "properties": {
                        "city": { "type": "string" }
                    }
                }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create").strict(true);
        let result = resolve(&schema, &options).unwrap();

        // Root level
        assert_eq!(result["additionalProperties"], json!(false));
        // Nested object
        assert_eq!(
            result["properties"]["address"]["additionalProperties"],
            json!(false)
        );
    }

    #[test]
    fn applies_to_array_items() {
        // Object items in arrays should also get additionalProperties: false in strict mode
        let schema = json!({
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" }
                }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create").strict(true);
        let result = resolve(&schema, &options).unwrap();

        // Array itself doesn't need additionalProperties
        assert!(result.get("additionalProperties").is_none());
        // But items do
        assert_eq!(result["items"]["additionalProperties"], json!(false));
    }

    #[test]
    fn applies_to_defs() {
        // Definitions should also be closed in strict mode
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            },
            "$defs": {
                "Address": {
                    "type": "object",
                    "properties": {
                        "city": { "type": "string" }
                    }
                }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create").strict(true);
        let result = resolve(&schema, &options).unwrap();

        assert_eq!(
            result["$defs"]["Address"]["additionalProperties"],
            json!(false)
        );
    }

    #[test]
    fn uses_unevaluated_for_allof() {
        // allOf schemas use unevaluatedProperties (not additionalProperties) because
        // unevaluatedProperties looks across all subschemas, allowing $ref inheritance
        // to work correctly while still rejecting unknown properties.
        let schema = json!({
            "allOf": [
                {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" }
                    }
                },
                {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" }
                    }
                }
            ]
        });
        let options = ResolveOptions::new(Direction::Request, "create").strict(true);
        let result = resolve(&schema, &options).unwrap();

        // Top level should have unevaluatedProperties: false (for composition)
        assert_eq!(result["unevaluatedProperties"], json!(false));
        // Branches should NOT have additionalProperties injected
        assert!(result["allOf"][0].get("additionalProperties").is_none());
        assert!(result["allOf"][1].get("additionalProperties").is_none());
    }

    #[test]
    fn non_strict_mode_skips_injection() {
        // With strict=false, additionalProperties should not be touched
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create").strict(false);
        let result = resolve(&schema, &options).unwrap();

        // Should not have additionalProperties added
        assert!(result.get("additionalProperties").is_none());
    }

    #[test]
    fn non_strict_mode_preserves_true() {
        // With strict=false, explicit true should remain
        let schema = json!({
            "type": "object",
            "additionalProperties": true,
            "properties": {
                "name": { "type": "string" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create").strict(false);
        let result = resolve(&schema, &options).unwrap();

        assert_eq!(result["additionalProperties"], json!(true));
    }

    #[test]
    fn detects_object_by_properties_key() {
        // Even without "type": "object", presence of "properties" should trigger strict mode
        let schema = json!({
            "properties": {
                "name": { "type": "string" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create").strict(true);
        let result = resolve(&schema, &options).unwrap();

        assert_eq!(result["additionalProperties"], json!(false));
    }
}
