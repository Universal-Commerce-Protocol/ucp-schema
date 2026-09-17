//! Integration tests for ambient `ucp` protocol-namespace materialization.

use serde_json::{json, Value};
use ucp_schema::{
    resolve_with_ucp_members, select_operation_schema, validate_against_schema, Direction,
    ResolveError, ResolveOptions, ValidateError,
};

const TEST_ROOT_ID: &str = "https://example.invalid/schemas/ambient-test";
const HELPER_KEY: &str = "__ucp_ambient_members";

fn helper_ref(key: &str) -> String {
    format!("{TEST_ROOT_ID}#/$defs/{key}")
}

fn default_helper_ref() -> Value {
    json!(helper_ref(HELPER_KEY))
}

fn with_root_id(schema: &Value) -> Value {
    let mut rooted = schema.clone();
    if let Value::Object(map) = &mut rooted {
        map.entry("$id".to_string()).or_insert(json!(TEST_ROOT_ID));
    }
    rooted
}

fn members_schema() -> Value {
    json!({
        "type": "object",
        "description": "Minimal central UCP members fixture for ambient materialization tests.",
        "properties": {
            "map_order": {
                "type": "object",
                "additionalProperties": {
                    "type": "array",
                    "items": { "type": "string" }
                },
                "ucp_request": "omit",
                "ucp_response": "optional"
            },
            "member_config": {
                "type": "object",
                "properties": {
                    "enabled": { "type": "boolean" }
                },
                "ucp_response": "optional"
            }
        },
        "additionalProperties": true
    })
}

fn response_options() -> ResolveOptions {
    ResolveOptions::new(Direction::Response, "search").strict(true)
}

fn request_options() -> ResolveOptions {
    ResolveOptions::new(Direction::Request, "search").strict(true)
}

fn resolve_response(schema: &Value) -> Value {
    let schema = with_root_id(schema);
    resolve_with_ucp_members(&schema, &members_schema(), &response_options()).unwrap()
}

fn resolve_request(schema: &Value) -> Value {
    let schema = with_root_id(schema);
    resolve_with_ucp_members(&schema, &members_schema(), &request_options()).unwrap()
}

fn assert_valid(schema: &Value, payload: Value) {
    validate_against_schema(schema, &payload).unwrap_or_else(|err| {
        panic!("expected valid payload {payload:#}; got {err:?}");
    });
}

fn assert_invalid(schema: &Value, payload: Value) {
    assert!(
        matches!(
            validate_against_schema(schema, &payload),
            Err(ValidateError::Invalid { .. })
        ),
        "expected invalid payload {payload:#}"
    );
}

#[test]
fn materializes_optional_ucp_property_at_structured_scopes() {
    let schema = json!({
        "type": "object",
        "required": ["name"],
        "properties": {
            "name": { "type": "string" },
            "child": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" }
                }
            }
        }
    });

    let resolved = resolve_response(&schema);

    assert_eq!(resolved["properties"]["ucp"]["$ref"], default_helper_ref());
    assert_eq!(
        resolved["properties"]["child"]["properties"]["ucp"]["$ref"],
        default_helper_ref()
    );
    assert!(!resolved["required"]
        .as_array()
        .unwrap()
        .contains(&json!("ucp")));
}

#[test]
fn strict_accepts_valid_ambient_member_and_rejects_unknown_domain_field() {
    let schema = json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" }
        }
    });
    let resolved = resolve_response(&schema);

    assert_valid(
        &resolved,
        json!({
            "name": "Widget",
            "ucp": { "map_order": { "name": ["en", "fr"] } }
        }),
    );
    assert_invalid(
        &resolved,
        json!({
            "name": "Widget",
            "unknown_domain_field": true,
            "ucp": { "future_member": { "anything": true } }
        }),
    );
}

#[test]
fn malformed_known_ambient_member_is_rejected_by_stock_validator() {
    let schema = json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" }
        }
    });
    let resolved = resolve_response(&schema);

    assert_invalid(
        &resolved,
        json!({
            "name": "Widget",
            "ucp": { "map_order": { "name": "not-an-array" } }
        }),
    );
}

#[test]
fn dictionary_key_named_ucp_remains_ordinary_data() {
    let schema = json!({
        "type": "object",
        "properties": {
            "attribution": {
                "type": "object",
                "additionalProperties": { "type": "string" }
            }
        }
    });
    let resolved = resolve_response(&schema);

    assert!(resolved["properties"]["attribution"]["properties"].is_null());
    assert_valid(
        &resolved,
        json!({
            "attribution": { "ucp": "ordinary dictionary value" }
        }),
    );
}

#[test]
fn structured_dictionary_values_are_eligible_for_ambient_ucp() {
    let schema = json!({
        "type": "object",
        "properties": {
            "registry": {
                "type": "object",
                "additionalProperties": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" }
                    }
                }
            }
        }
    });
    let resolved = resolve_response(&schema);

    assert_eq!(
        resolved["properties"]["registry"]["additionalProperties"]["properties"]["ucp"]["$ref"],
        default_helper_ref()
    );
    assert_valid(
        &resolved,
        json!({
            "registry": {
                "ucp": {
                    "id": "dictionary-key-named-ucp",
                    "ucp": { "map_order": { "id": ["first"] } }
                }
            }
        }),
    );
}

#[test]
fn pattern_property_values_are_eligible_for_ambient_ucp() {
    let schema = json!({
        "type": "object",
        "properties": {
            "registry": {
                "type": "object",
                "patternProperties": {
                    "^item:": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" }
                        }
                    }
                }
            }
        }
    });
    let resolved = resolve_response(&schema);

    assert_eq!(
        resolved["properties"]["registry"]["patternProperties"]["^item:"]["properties"]["ucp"]
            ["$ref"],
        default_helper_ref()
    );
    assert_valid(
        &resolved,
        json!({
            "registry": {
                "item:1": {
                    "id": "p1",
                    "ucp": { "map_order": { "id": ["first"] } }
                }
            }
        }),
    );
}

#[test]
fn strict_pattern_properties_value_rejects_unknown_domain_properties() {
    let schema = json!({
        "type": "object",
        "properties": {
            "registry": {
                "type": "object",
                "patternProperties": {
                    "^item:": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" }
                        }
                    }
                }
            }
        }
    });
    let resolved = resolve_response(&schema);

    assert_invalid(
        &resolved,
        json!({
            "registry": {
                "item:1": {
                    "id": "p1",
                    "unknown_domain_field": true,
                    "ucp": { "map_order": { "id": ["first"] } }
                }
            }
        }),
    );
}

#[test]
fn child_instance_applicators_are_traversed_with_normal_scope() {
    let structured = json!({
        "type": "object",
        "properties": {
            "id": { "type": "string" }
        }
    });
    let cases = vec![
        (
            "additionalProperties",
            json!({ "type": "object", "additionalProperties": structured.clone() }),
            "/properties/value/additionalProperties/properties/ucp/$ref",
        ),
        (
            "unevaluatedProperties",
            json!({ "type": "object", "unevaluatedProperties": structured.clone() }),
            "/properties/value/unevaluatedProperties/properties/ucp/$ref",
        ),
        (
            "propertyNames",
            json!({ "type": "object", "propertyNames": structured.clone() }),
            "/properties/value/propertyNames/properties/ucp/$ref",
        ),
        (
            "patternProperties",
            json!({ "type": "object", "patternProperties": { "^item:": structured.clone() } }),
            "/properties/value/patternProperties/^item:/properties/ucp/$ref",
        ),
        (
            "items",
            json!({ "type": "array", "items": structured.clone() }),
            "/properties/value/items/properties/ucp/$ref",
        ),
        (
            "prefixItems",
            json!({ "type": "array", "prefixItems": [structured.clone()] }),
            "/properties/value/prefixItems/0/properties/ucp/$ref",
        ),
        (
            "contains",
            json!({ "type": "array", "contains": structured.clone() }),
            "/properties/value/contains/properties/ucp/$ref",
        ),
        (
            "unevaluatedItems",
            json!({ "type": "array", "prefixItems": [{ "type": "string" }], "unevaluatedItems": structured.clone() }),
            "/properties/value/unevaluatedItems/properties/ucp/$ref",
        ),
        (
            "contentSchema",
            json!({ "type": "string", "contentMediaType": "application/json", "contentSchema": structured }),
            "/properties/value/contentSchema/properties/ucp/$ref",
        ),
    ];

    for (keyword, value_schema, pointer) in cases {
        let schema = json!({
            "type": "object",
            "properties": {
                "value": value_schema
            }
        });
        let resolved = resolve_response(&schema);
        assert_eq!(
            resolved.pointer(pointer),
            Some(&default_helper_ref()),
            "{keyword} should traverse child-instance schema at {pointer}"
        );
    }
}

#[test]
fn unevaluated_items_value_can_use_ambient_ucp_members() {
    let schema = json!({
        "type": "array",
        "prefixItems": [{ "type": "string" }],
        "unevaluatedItems": {
            "type": "object",
            "properties": {
                "id": { "type": "string" }
            }
        }
    });
    let resolved = resolve_response(&schema);

    assert_eq!(
        resolved["unevaluatedItems"]["properties"]["ucp"]["$ref"],
        default_helper_ref()
    );
    assert_valid(
        &resolved,
        json!([
            "first",
            {
                "id": "p1",
                "ucp": { "map_order": { "id": ["first"] } }
            }
        ]),
    );
}

#[test]
fn child_instances_beneath_direct_namespace_return_to_normal_scope() {
    let schema = json!({
        "type": "object",
        "properties": {
            "ucp": {
                "type": "object",
                "properties": {
                    "child": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" }
                        }
                    }
                },
                "patternProperties": {
                    "^member:": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" }
                        }
                    }
                }
            }
        }
    });
    let resolved = resolve_response(&schema);

    assert_eq!(
        resolved["properties"]["ucp"]["properties"]["child"]["properties"]["ucp"]["$ref"],
        default_helper_ref()
    );
    assert_eq!(
        resolved["properties"]["ucp"]["patternProperties"]["^member:"]["properties"]["ucp"]["$ref"],
        default_helper_ref()
    );
    assert_eq!(
        resolved["properties"]["ucp"]["properties"]["ucp"],
        json!(false)
    );
}

#[test]
fn direct_namespace_same_instance_applicators_preserve_direct_scope() {
    let cases = vec![
        (
            "allOf",
            json!({ "allOf": [{ "type": "object", "properties": { "ucp": { "type": "object" } } }] }),
            "/properties/ucp/allOf/0/properties/ucp",
        ),
        (
            "anyOf",
            json!({ "anyOf": [{ "type": "object", "properties": { "ucp": { "type": "object" } } }] }),
            "/properties/ucp/anyOf/0/properties/ucp",
        ),
        (
            "oneOf",
            json!({ "oneOf": [{ "type": "object", "properties": { "ucp": { "type": "object" } } }] }),
            "/properties/ucp/oneOf/0/properties/ucp",
        ),
        (
            "if",
            json!({ "if": { "type": "object", "properties": { "ucp": { "type": "object" } } } }),
            "/properties/ucp/if/properties/ucp",
        ),
        (
            "then",
            json!({ "then": { "type": "object", "properties": { "ucp": { "type": "object" } } } }),
            "/properties/ucp/then/properties/ucp",
        ),
        (
            "else",
            json!({ "else": { "type": "object", "properties": { "ucp": { "type": "object" } } } }),
            "/properties/ucp/else/properties/ucp",
        ),
        (
            "not",
            json!({ "not": { "type": "object", "properties": { "ucp": { "type": "object" } } } }),
            "/properties/ucp/not/properties/ucp",
        ),
        (
            "dependentSchemas",
            json!({ "dependentSchemas": { "flag": { "type": "object", "properties": { "ucp": { "type": "object" } } } } }),
            "/properties/ucp/dependentSchemas/flag/properties/ucp",
        ),
    ];

    for (keyword, ucp_schema, pointer) in cases {
        let schema = json!({
            "type": "object",
            "properties": {
                "ucp": ucp_schema
            }
        });
        let resolved = resolve_response(&schema);
        assert_eq!(
            resolved.pointer(pointer),
            Some(&json!(false)),
            "{keyword} should preserve direct namespace scope at {pointer}"
        );
    }
}

#[test]
fn direct_namespace_conditional_rejects_ucp_recursion() {
    let schema = json!({
        "type": "object",
        "properties": {
            "ucp": {
                "type": "object",
                "properties": {
                    "kind": { "const": "guarded" }
                },
                "if": {
                    "properties": {
                        "kind": { "const": "guarded" }
                    }
                },
                "then": {
                    "properties": {
                        "hint": { "type": "string" },
                        "ucp": { "type": "object" }
                    }
                }
            }
        }
    });
    let resolved = resolve_response(&schema);

    assert_eq!(
        resolved["properties"]["ucp"]["then"]["properties"]["ucp"],
        json!(false)
    );
    assert_invalid(
        &resolved,
        json!({ "ucp": { "kind": "guarded", "ucp": {} } }),
    );
    assert_valid(
        &resolved,
        json!({ "ucp": { "kind": "guarded", "hint": "kept", "future_member": true } }),
    );
}

#[test]
fn direct_ucp_ucp_is_rejected_but_deeper_structured_children_can_have_ucp() {
    let schema = json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" }
        }
    });
    let resolved = resolve_response(&schema);

    assert_invalid(&resolved, json!({ "name": "Widget", "ucp": { "ucp": {} } }));
    assert_valid(
        &resolved,
        json!({
            "name": "Widget",
            "ucp": {
                "member_config": {
                    "enabled": true,
                    "ucp": { "map_order": { "enabled": ["first"] } }
                }
            }
        }),
    );
}

#[test]
fn explicit_ucp_property_is_namespace_not_overwritten_and_rejects_direct_recursion() {
    let schema = json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "ucp": {
                "type": "object",
                "properties": {
                    "hint": { "type": "string" }
                },
                "additionalProperties": true
            }
        }
    });
    let resolved = resolve_response(&schema);

    assert!(resolved["properties"]["ucp"].get("$ref").is_none());
    assert_eq!(
        resolved["properties"]["ucp"]["properties"]["hint"]["type"],
        "string"
    );
    assert_eq!(
        resolved["properties"]["ucp"]["properties"]["ucp"],
        json!(false)
    );

    assert_valid(
        &resolved,
        json!({
            "name": "Widget",
            "ucp": { "hint": "kept", "future_member": { "anything": true } }
        }),
    );
    assert_invalid(&resolved, json!({ "name": "Widget", "ucp": { "ucp": {} } }));
}

#[test]
fn explicit_ucp_composition_branches_are_direct_namespace_schemas() {
    let schema = json!({
        "type": "object",
        "properties": {
            "ucp": {
                "additionalProperties": false,
                "allOf": [
                    {
                        "type": "object",
                        "properties": {
                            "hint": { "type": "string" }
                        }
                    }
                ]
            }
        }
    });
    let resolved = resolve_response(&schema);
    let explicit_ucp = &resolved["properties"]["ucp"];

    assert_eq!(explicit_ucp["properties"]["ucp"], json!(false));
    assert_eq!(explicit_ucp["additionalProperties"], json!({}));
    assert_eq!(explicit_ucp["unevaluatedProperties"], json!({}));
    assert_eq!(explicit_ucp["allOf"][0]["properties"]["ucp"], json!(false));
    assert_valid(
        &resolved,
        json!({ "ucp": { "hint": "kept", "future_member": true } }),
    );
    assert_invalid(&resolved, json!({ "ucp": { "ucp": {} } }));
}

#[test]
fn request_omitted_known_member_is_false_while_unknown_future_member_stays_open() {
    let schema = json!({
        "type": "object",
        "properties": {
            "query": { "type": "string" }
        }
    });
    let resolved = resolve_request(&schema);

    let helper = resolved["$defs"][HELPER_KEY].clone();
    assert!(helper.get("$id").is_none());
    assert_eq!(helper["properties"]["map_order"], json!(false));
    assert_eq!(helper["additionalProperties"], json!({}));

    assert_invalid(
        &resolved,
        json!({ "query": "boots", "ucp": { "map_order": { "x": ["a"] } } }),
    );
    assert_valid(
        &resolved,
        json!({ "query": "boots", "ucp": { "future_member": { "anything": true } } }),
    );
}

#[test]
fn members_registry_rejects_unbundled_local_refs() {
    let schema = with_root_id(&json!({
        "type": "object",
        "properties": {
            "query": { "type": "string" }
        }
    }));
    let members = json!({
        "type": "object",
        "$defs": {
            "map_order": {
                "type": "object",
                "additionalProperties": {
                    "type": "array",
                    "items": { "type": "string" }
                }
            }
        },
        "properties": {
            "map_order": { "$ref": "#/$defs/map_order" }
        },
        "additionalProperties": true
    });

    let result = resolve_with_ucp_members(&schema, &members, &response_options());
    let Err(ResolveError::InvalidSchema { message }) = result else {
        panic!("expected unbundled members registry to be rejected");
    };
    assert!(message.contains("#/$defs/map_order"));
}

#[test]
fn members_registry_requires_direct_object_properties() {
    let schema = with_root_id(&json!({
        "type": "object",
        "properties": {
            "query": { "type": "string" }
        }
    }));

    for members in [
        json!(false),
        json!({ "allOf": [{ "properties": { "branch_member": { "type": "object" } } }] }),
        json!({ "properties": false }),
    ] {
        let result = resolve_with_ucp_members(&schema, &members, &request_options());
        assert!(matches!(result, Err(ResolveError::InvalidSchema { .. })));
    }
}

#[test]
fn include_future_surfaces_omitted_central_member_instead_of_false() {
    let schema = json!({
        "type": "object",
        "properties": {
            "query": { "type": "string" }
        }
    });
    let members = json!({
        "type": "object",
        "properties": {
            "planned_member": {
                "type": "object",
                "properties": {
                    "enabled": { "type": "boolean" }
                },
                "ucp_request": {
                    "transition": {
                        "from": "omit",
                        "to": "optional",
                        "description": "Planned request member."
                    }
                }
            }
        },
        "additionalProperties": true
    });
    let options = ResolveOptions::new(Direction::Request, "search")
        .strict(true)
        .include_future(true);
    let rooted_schema = with_root_id(&schema);
    let resolved = resolve_with_ucp_members(&rooted_schema, &members, &options).unwrap();

    let planned = &resolved["$defs"][HELPER_KEY]["properties"]["planned_member"];
    assert_ne!(planned, &json!(false));
    assert_eq!(planned["x-ucp-schema-transition"]["from"], json!("omit"));
    assert_valid(
        &resolved,
        json!({
            "query": "boots",
            "ucp": { "planned_member": { "enabled": true } }
        }),
    );
}

#[test]
fn explicit_ucp_omitted_by_annotation_is_not_reintroduced() {
    let schema = json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "ucp": {
                "type": "object",
                "ucp_request": "omit",
                "properties": {
                    "map_order": { "type": "object" }
                },
                "additionalProperties": true
            }
        }
    });

    let resolved = resolve_request(&schema);

    assert!(resolved["properties"].get("ucp").is_none());
    assert_invalid(
        &resolved,
        json!({ "name": "Widget", "ucp": { "future": true } }),
    );
}

#[test]
fn allof_and_selected_defs_preserve_absolute_helper_refs() {
    let schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": TEST_ROOT_ID,
        "$defs": {
            "search_response": {
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
            }
        }
    });
    let options = ResolveOptions::new(Direction::Response, "search")
        .strict(true)
        .def_name(Some("search_response".to_string()));
    let resolved = resolve_with_ucp_members(&schema, &members_schema(), &options).unwrap();
    let selected = select_operation_schema(&resolved, &options).unwrap();

    assert_eq!(
        selected["$defs"]["search_response"]["allOf"][0]["properties"]["ucp"]["$ref"],
        default_helper_ref()
    );
    assert_eq!(selected["$id"], TEST_ROOT_ID);
    assert!(selected["$defs"][HELPER_KEY].is_object());
    assert_valid(
        &selected,
        json!({
            "id": "p1",
            "name": "Widget",
            "ucp": { "map_order": { "name": ["first"] } }
        }),
    );
}

#[test]
fn nested_id_scope_does_not_break_absolute_helper_resolution() {
    let schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": TEST_ROOT_ID,
        "type": "object",
        "properties": {
            "nested": {
                "$id": "https://example.invalid/nested-schema",
                "type": "object",
                "properties": {
                    "id": { "type": "string" }
                }
            }
        }
    });
    let resolved = resolve_response(&schema);

    assert_eq!(
        resolved["properties"]["nested"]["properties"]["ucp"]["$ref"],
        default_helper_ref()
    );
    assert_valid(
        &resolved,
        json!({
            "nested": {
                "id": "n1",
                "ucp": { "map_order": { "id": ["first"] } }
            }
        }),
    );
}

#[test]
fn helper_key_collision_uses_suffix_and_preserves_authored_defs() {
    let collision_root_id = "https://example.invalid/schemas/collision";
    let schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": collision_root_id,
        "$defs": {
            "__ucp_ambient_members": { "type": "string" }
        },
        "type": "object",
        "properties": {
            "name": { "type": "string" }
        }
    });
    let resolved =
        resolve_with_ucp_members(&schema, &members_schema(), &response_options()).unwrap();

    assert_eq!(resolved["$defs"][HELPER_KEY]["type"], json!("string"));
    assert!(resolved["$defs"]["__ucp_ambient_members_1"].is_object());
    assert!(resolved["$defs"]["__ucp_ambient_members_1"]
        .get("$id")
        .is_none());
    assert_eq!(
        resolved["properties"]["ucp"]["$ref"],
        json!(format!(
            "{collision_root_id}#/$defs/__ucp_ambient_members_1"
        ))
    );
    assert_valid(
        &resolved,
        json!({
            "name": "Widget",
            "ucp": { "map_order": { "name": ["first"] } }
        }),
    );
}

#[test]
fn missing_or_non_absolute_root_id_is_invalid_schema() {
    let missing_id = json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" }
        }
    });
    let relative_id = json!({
        "$id": "relative/schema.json",
        "type": "object",
        "properties": {
            "name": { "type": "string" }
        }
    });

    for schema in [missing_id, relative_id] {
        let result = resolve_with_ucp_members(&schema, &members_schema(), &response_options());
        assert!(matches!(result, Err(ResolveError::InvalidSchema { .. })));
    }
}

#[test]
fn non_object_root_defs_is_invalid_schema() {
    let schema = json!({
        "$id": TEST_ROOT_ID,
        "$defs": false,
        "type": "object",
        "properties": {
            "name": { "type": "string" }
        }
    });

    let result = resolve_with_ucp_members(&schema, &members_schema(), &response_options());
    assert!(matches!(result, Err(ResolveError::InvalidSchema { .. })));
}
