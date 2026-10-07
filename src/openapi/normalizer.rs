//! Directional schema resolution and UCP keyword stripping.
//!
//! Normalizes JSON Schemas into standard JSON Schema 2020-12 / OpenAPI 3.1 definitions
//! by stripping internal UCP annotations (`ucp_request`, `ucp_response`, `x-ucp-*`),
//! rewriting `$ref` targets to OpenAPI component paths (`#/components/schemas/...`),
//! distributing properties in bare `anyOf` branches, and invoking `resolver.rs` for directional slicing.

use serde_json::Value;

use crate::error::ResolveError;
use crate::openapi::discriminator::{
    ref_to_component_name, to_pascal_case, transform_schema_conditionals,
};
use crate::resolver::resolve;
use crate::types::{Direction, ResolveOptions, UCP_RESERVED_KEYWORDS};

/// UCP internal keywords to strip from OpenAPI output.
pub const UCP_KEYWORDS_TO_STRIP: &[&str] = UCP_RESERVED_KEYWORDS;

/// Recursively strip internal UCP authoring keywords and unnecessary root metadata
/// from a JSON Schema value.
/// Recursively strip internal UCP authoring keywords and unnecessary root metadata
/// from a JSON Schema value.
pub fn strip_ucp_keywords(value: &mut Value) {
    strip_ucp_keywords_scoped(value, true);
}

fn strip_ucp_keywords_scoped(value: &mut Value, is_root: bool) {
    match value {
        Value::Object(map) => {
            // Strip known UCP keywords and x-ucp-* vendor extensions
            map.retain(|k, _| {
                !UCP_KEYWORDS_TO_STRIP.contains(&k.as_str()) && !k.starts_with("x-ucp-")
            });

            // Strip $schema and $id inside component schemas (standard for OpenAPI 3.1 components)
            map.remove("$schema");
            map.remove("$id");

            // Strip capability reverse-domain package name (e.g., "name": "dev.ucp.shopping.checkout")
            if let Some(name_val) = map.get("name").and_then(|v| v.as_str()) {
                if crate::namespace::is_reverse_domain_name(name_val) {
                    map.remove("name");
                }
            }

            // Strip internal schema date version at root if present (scoped to root metadata only)
            if is_root {
                if let Some(ver_val) = map.get("version").and_then(|v| v.as_str()) {
                    if ver_val.len() == 10 && ver_val.chars().filter(|&c| c == '-').count() == 2 {
                        map.remove("version");
                    }
                }
            }

            // Clean up required array to only include fields present in properties
            // (collecting property names from root properties and recursively from any allOf branches)
            let mut prop_keys = std::collections::HashSet::new();
            let has_properties = collect_declared_properties(map, &mut prop_keys);
            let has_ref = map.contains_key("$ref") || allof_has_ref(map);

            if has_properties {
                if let Some(reqs) = map.get_mut("required").and_then(|r| r.as_array_mut()) {
                    reqs.retain(|item| {
                        item.as_str()
                            .map(|k| prop_keys.contains(k) || has_ref)
                            .unwrap_or(true)
                    });
                }
            }

            // Remove empty required array
            if let Some(reqs) = map.get("required").and_then(|r| r.as_array()) {
                if reqs.is_empty() {
                    map.remove("required");
                }
            }

            // Recurse into remaining fields
            for (_k, v) in map.iter_mut() {
                strip_ucp_keywords_scoped(v, false);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                strip_ucp_keywords_scoped(item, false);
            }
        }
        _ => {}
    }
}

/// Helper to recursively collect all property names declared in `properties` and any `allOf` branches.
fn collect_declared_properties(
    obj: &serde_json::Map<String, Value>,
    prop_keys: &mut std::collections::HashSet<String>,
) -> bool {
    let mut has_properties = false;
    if let Some(props) = obj.get("properties").and_then(|p| p.as_object()) {
        for k in props.keys() {
            prop_keys.insert(k.clone());
        }
        has_properties = true;
    }
    if let Some(all_of) = obj.get("allOf").and_then(|a| a.as_array()) {
        for branch in all_of {
            if let Some(branch_obj) = branch.as_object() {
                if collect_declared_properties(branch_obj, prop_keys) {
                    has_properties = true;
                }
            }
        }
    }
    has_properties
}

/// Helper to check if any `allOf` branch contains a `$ref`.
fn allof_has_ref(obj: &serde_json::Map<String, Value>) -> bool {
    if let Some(all_of) = obj.get("allOf").and_then(|a| a.as_array()) {
        for branch in all_of {
            if let Some(branch_obj) = branch.as_object() {
                if branch_obj.contains_key("$ref") || allof_has_ref(branch_obj) {
                    return true;
                }
            }
        }
    }
    false
}

/// Recursively rewrite self references `"$ref": "#"` to point to the parent schema name.
pub fn rewrite_self_refs_to_parent(value: &mut Value, parent_name: &str) {
    match value {
        Value::Object(map) => {
            if let Some(r) = map.get("$ref").and_then(|v| v.as_str()) {
                if r == "#" {
                    let parent_ref = format!("#/components/schemas/{}", parent_name);
                    map.insert("$ref".to_string(), Value::String(parent_ref));
                }
            }
            for (_k, v) in map.iter_mut() {
                rewrite_self_refs_to_parent(v, parent_name);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                rewrite_self_refs_to_parent(item, parent_name);
            }
        }
        _ => {}
    }
}

/// Check if a definition name is a generic stem that requires parent qualification
/// to avoid naming collisions in components.schemas.
pub fn is_generic_def_name(name: &str) -> bool {
    matches!(
        name,
        "Base"
            | "base"
            | "Entity"
            | "entity"
            | "PlatformSchema"
            | "platform_schema"
            | "BusinessSchema"
            | "business_schema"
            | "ResponseSchema"
            | "response_schema"
    )
}

/// Recursively rewrite `$defs` internal references `"$ref": "#/$defs/<name>"` to `#/components/schemas/<PascalCaseName>`.
/// If the definition has a generic name (e.g. `base`), qualifies it with `parent_name` (e.g. `CheckoutBase`).
pub fn rewrite_defs_refs_to_components(value: &mut Value, parent_name: Option<&str>) {
    match value {
        Value::Object(map) => {
            if let Some(r) = map.get("$ref").and_then(|v| v.as_str()) {
                if let Some(def_key) = r.strip_prefix("#/$defs/") {
                    let def_pascal = to_pascal_case(def_key);
                    let is_generic =
                        is_generic_def_name(def_key) || is_generic_def_name(&def_pascal);
                    let comp_name = if is_generic {
                        if let Some(parent) = parent_name {
                            format!("{}{}", parent, def_pascal)
                        } else {
                            def_pascal
                        }
                    } else {
                        def_pascal
                    };
                    let new_ref = format!("#/components/schemas/{}", comp_name);
                    map.insert("$ref".to_string(), Value::String(new_ref));
                }
            }
            for (_k, v) in map.iter_mut() {
                rewrite_defs_refs_to_components(v, parent_name);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                rewrite_defs_refs_to_components(item, parent_name);
            }
        }
        _ => {}
    }
}

/// Distribute parent `properties` into bare `anyOf` constraint branches (e.g. for `ValueConstraint`).
///
/// Ensures downstream generators (e.g. `datamodel-codegen`) emit concrete typed models
/// instead of recursive type aliases.
pub fn distribute_properties_in_bare_anyof(value: &mut Value) {
    match value {
        Value::Object(map) => {
            // Recurse into child fields first
            for (_k, v) in map.iter_mut() {
                distribute_properties_in_bare_anyof(v);
            }

            if let Some(parent_props) = map.get("properties").and_then(|p| p.as_object()).cloned() {
                let add_props = map.get("additionalProperties").cloned();
                if let Some(any_of) = map.get_mut("anyOf").and_then(|a| a.as_array_mut()) {
                    let mut should_distribute = false;
                    for branch in any_of.iter() {
                        if let Some(branch_obj) = branch.as_object() {
                            if branch_obj.contains_key("required")
                                && !branch_obj.contains_key("properties")
                            {
                                should_distribute = true;
                                break;
                            }
                        }
                    }

                    if should_distribute {
                        for branch in any_of.iter_mut() {
                            if let Some(branch_obj) = branch.as_object_mut() {
                                if !branch_obj.contains_key("properties") {
                                    branch_obj.insert(
                                        "type".to_string(),
                                        Value::String("object".to_string()),
                                    );
                                    branch_obj.insert(
                                        "properties".to_string(),
                                        Value::Object(parent_props.clone()),
                                    );
                                    if let Some(ref ap) = add_props {
                                        branch_obj
                                            .entry("additionalProperties".to_string())
                                            .or_insert_with(|| ap.clone());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Value::Array(arr) => {
            for item in arr {
                distribute_properties_in_bare_anyof(item);
            }
        }
        _ => {}
    }
}

/// Recursively rewrite external URL and file `$ref` targets to OpenAPI `#/components/schemas/<Name>`.
pub fn rewrite_refs_to_components(value: &mut Value, current_component: &str) {
    match value {
        Value::Object(map) => {
            if let Some(ref_val) = map.get("$ref").and_then(|v| v.as_str()) {
                if ref_val == "#" {
                    let new_ref = format!("#/components/schemas/{}", current_component);
                    map.insert("$ref".to_string(), Value::String(new_ref));
                } else if let Some(def_name) = ref_val.strip_prefix("#/$defs/") {
                    let def_pascal = to_pascal_case(def_name);
                    let is_generic =
                        is_generic_def_name(def_name) || is_generic_def_name(&def_pascal);
                    let comp_name = if is_generic {
                        format!("{}{}", current_component, def_pascal)
                    } else {
                        def_pascal
                    };
                    let new_ref = format!("#/components/schemas/{}", comp_name);
                    map.insert("$ref".to_string(), Value::String(new_ref));
                } else if !ref_val.starts_with("#/components/") {
                    let comp_name = ref_to_component_name(ref_val);
                    let new_ref = format!("#/components/schemas/{}", comp_name);
                    map.insert("$ref".to_string(), Value::String(new_ref));
                }
            }

            for (_k, v) in map.iter_mut() {
                rewrite_refs_to_components(v, current_component);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                rewrite_refs_to_components(item, current_component);
            }
        }
        _ => {}
    }
}

/// If a property has `const: val` or `enum: [val]` without a `default`, attach `default: val`.
/// This ensures downstream code generators (Pydantic, Zod, etc.) generate default values for discriminators.
pub fn attach_const_defaults(value: &mut Value) {
    match value {
        Value::Object(map) => {
            let const_opt = map.get("const").cloned();
            let enum_opt = map.get("enum").and_then(|e| e.as_array()).and_then(|a| {
                if a.len() == 1 {
                    Some(a[0].clone())
                } else {
                    None
                }
            });

            if let Some(const_val) = const_opt {
                if !map.contains_key("default") {
                    map.insert("default".to_string(), const_val.clone());
                }
                if !map.contains_key("type") {
                    if const_val.is_string() {
                        map.insert("type".to_string(), Value::String("string".to_string()));
                    } else if const_val.is_number() {
                        map.insert("type".to_string(), Value::String("number".to_string()));
                    } else if const_val.is_boolean() {
                        map.insert("type".to_string(), Value::String("boolean".to_string()));
                    }
                }
            } else if let Some(enum_val) = enum_opt {
                if !map.contains_key("default") {
                    map.insert("default".to_string(), enum_val.clone());
                }
                if !map.contains_key("type") && enum_val.is_string() {
                    map.insert("type".to_string(), Value::String("string".to_string()));
                }
            }

            for (_k, v) in map.iter_mut() {
                attach_const_defaults(v);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                attach_const_defaults(item);
            }
        }
        _ => {}
    }
}

/// Normalize scalar-or-array polymorphic unions (e.g. `extends` being either `ReverseDomainName` or `array of ReverseDomainName`)
/// into the canonical array form `type: array, items: <schema>`.
///
/// This prevents code generators (like openapi-generator in Java) from generating broken anonymous union types (`List<String>.class`).
pub fn normalize_scalar_or_array_unions(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for v in map.values_mut() {
                normalize_scalar_or_array_unions(v);
            }

            let transformed_array_items =
                if let Some(one_of) = map.get("oneOf").and_then(|v| v.as_array()) {
                    if one_of.len() == 2 {
                        let b0 = &one_of[0];
                        let b1 = &one_of[1];
                        let is_arr0 = b0.get("type").and_then(|t| t.as_str()) == Some("array");
                        let is_arr1 = b1.get("type").and_then(|t| t.as_str()) == Some("array");

                        if is_arr0 ^ is_arr1 {
                            let (scalar_b, array_b) = if is_arr1 { (b0, b1) } else { (b1, b0) };
                            if let Some(items) = array_b.get("items") {
                                let matches = if let (Some(r1), Some(r2)) =
                                    (scalar_b.get("$ref"), items.get("$ref"))
                                {
                                    r1 == r2
                                } else if let (Some(t1), Some(t2)) =
                                    (scalar_b.get("type"), items.get("type"))
                                {
                                    t1 == t2
                                } else {
                                    false
                                };

                                if matches {
                                    Some((items.clone(), map.get("description").cloned()))
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

            if let Some((items, desc)) = transformed_array_items {
                map.remove("oneOf");
                map.insert("type".to_string(), Value::String("array".to_string()));
                map.insert("items".to_string(), items);
                if let Some(d) = desc {
                    map.insert("description".to_string(), d);
                }
            }
        }
        Value::Array(arr) => {
            for v in arr {
                normalize_scalar_or_array_unions(v);
            }
        }
        _ => {}
    }
}

/// Normalize a single standalone component schema for inclusion in `components.schemas`:
/// 1. Strip internal UCP keywords
/// 2. Transform conditional `if`/`then` branches into `oneOf` + `discriminator`
/// 3. Distribute top-level properties into bare `anyOf` branches
/// 4. Rewrite `$defs` refs and external schema URLs to `#/components/schemas/<PascalName>`
/// 5. Attach default values to constant/single-enum properties
pub fn normalize_component_schema(schema: &Value, current_component: &str) -> Value {
    let mut normalized = schema.clone();
    strip_ucp_keywords(&mut normalized);
    transform_schema_conditionals(&mut normalized);
    distribute_properties_in_bare_anyof(&mut normalized);
    normalize_scalar_or_array_unions(&mut normalized);
    rewrite_refs_to_components(&mut normalized, current_component);
    attach_const_defaults(&mut normalized);
    normalized
}

/// Check if a schema explicitly supports the `complete` lifecycle operation.
pub fn schema_supports_complete(schema: &Value) -> bool {
    // 1. Explicit x-ucp-lifecycle annotation
    if let Some(lifecycle) = schema.get("x-ucp-lifecycle").and_then(|l| l.as_array()) {
        if lifecycle.iter().any(|v| v.as_str() == Some("complete")) {
            return true;
        }
    }

    // 2. Explicit complete defs
    if let Some(defs) = schema.get("$defs").and_then(|d| d.as_object()) {
        if defs.keys().any(|k| {
            k == "complete_request"
                || k == "CompleteRequest"
                || k.ends_with(".complete_request")
                || k.ends_with("_complete_request")
        }) {
            return true;
        }
    }

    // 3. Any property annotating complete in ucp_request
    has_complete_annotation(schema)
}

fn has_complete_annotation(val: &Value) -> bool {
    match val {
        Value::Object(map) => {
            if let Some(ucp_req) = map.get("ucp_request").and_then(|u| u.as_object()) {
                if ucp_req.contains_key("complete") {
                    return true;
                }
            }
            map.values().any(has_complete_annotation)
        }
        Value::Array(arr) => arr.iter().any(has_complete_annotation),
        _ => false,
    }
}

/// Perform directional slicing for a capability resource schema.
///
/// Returns:
/// - `<BaseName>CreateRequest` (pruned for `Direction::Request`, `create`)
/// - `<BaseName>UpdateRequest` (pruned for `Direction::Request`, `update`)
/// - Optional `<BaseName>CompleteRequest` (if schema annotates `complete` or defines complete requests)
/// - `<BaseName>` (pruned for `Direction::Response`, full representation)
pub fn slice_directional_schemas(
    raw_schema: &Value,
    base_name: &str,
    strict: bool,
) -> Result<Vec<(String, Value)>, ResolveError> {
    let mut results = Vec::new();

    let supports_complete = schema_supports_complete(raw_schema);
    let mut request_operations = vec![("CreateRequest", "create"), ("UpdateRequest", "update")];
    if supports_complete {
        request_operations.push(("CompleteRequest", "complete"));
    }

    for (suffix, op) in request_operations {
        let name = format!("{}{}", base_name, suffix);
        let opts = ResolveOptions::new(Direction::Request, op).strict(strict);
        let resolved = resolve(raw_schema, &opts)?;
        let mut normalized = normalize_component_schema(&resolved, &name);

        let has_props = normalized
            .get("properties")
            .and_then(|p| p.as_object())
            .map(|p| !p.is_empty())
            .unwrap_or(false)
            || normalized.get("allOf").is_some()
            || normalized.get("oneOf").is_some()
            || normalized.get("anyOf").is_some();

        if has_props {
            if let Some(map) = normalized.as_object_mut() {
                map.insert("title".to_string(), Value::String(name.clone()));
                let op_desc = match op {
                    "create" => "Request payload to create a new",
                    "update" => "Request payload to update an existing",
                    "complete" => "Request payload to complete a",
                    _ => "Request payload for",
                };
                if let Some(orig_desc) = map.get("description").and_then(|d| d.as_str()) {
                    map.insert(
                        "description".to_string(),
                        Value::String(format!("{} {}. {}", op_desc, base_name, orig_desc)),
                    );
                } else {
                    map.insert(
                        "description".to_string(),
                        Value::String(format!("{} {}.", op_desc, base_name)),
                    );
                }
            }
            results.push((name, normalized));
        }
    }

    let resp_opts = ResolveOptions::new(Direction::Response, "read").strict(strict);
    let resp_resolved = resolve(raw_schema, &resp_opts)?;
    let resp_normalized = normalize_component_schema(&resp_resolved, base_name);
    results.push((base_name.to_string(), resp_normalized));

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_strip_ucp_keywords() {
        let mut schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://ucp.dev/draft/schemas/shopping/types/buyer.json",
            "title": "Buyer",
            "type": "object",
            "ucp_shared_request": true,
            "required": ["email"],
            "properties": {
                "email": {
                    "type": "string",
                    "format": "email",
                    "ucp_request": "optional",
                    "x-ucp-schema-transition": { "from": "optional", "to": "required" }
                }
            }
        });

        strip_ucp_keywords(&mut schema);

        assert!(schema.get("$schema").is_none());
        assert!(schema.get("$id").is_none());
        assert!(schema.get("ucp_shared_request").is_none());
        assert_eq!(schema["title"], "Buyer");
        assert!(schema["properties"]["email"].get("ucp_request").is_none());
        assert!(schema["properties"]["email"]
            .get("x-ucp-schema-transition")
            .is_none());
        assert_eq!(schema["properties"]["email"]["type"], "string");
    }

    #[test]
    fn test_rewrite_refs_to_components() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "destination": {
                    "$ref": "https://ucp.dev/draft/schemas/shopping/types/shipping_destination.json"
                },
                "address": {
                    "$ref": "../common/types/postal_address.json"
                },
                "local_def": {
                    "$ref": "#/$defs/item"
                },
                "self_ref": {
                    "$ref": "#"
                },
                "existing_component": {
                    "$ref": "#/components/schemas/Amount"
                }
            }
        });

        rewrite_refs_to_components(&mut schema, "Cart");

        assert_eq!(
            schema["properties"]["destination"]["$ref"],
            "#/components/schemas/ShippingDestination"
        );
        assert_eq!(
            schema["properties"]["address"]["$ref"],
            "#/components/schemas/PostalAddress"
        );
        assert_eq!(
            schema["properties"]["local_def"]["$ref"],
            "#/components/schemas/Item"
        );
        assert_eq!(
            schema["properties"]["self_ref"]["$ref"],
            "#/components/schemas/Cart"
        );
        assert_eq!(
            schema["properties"]["existing_component"]["$ref"],
            "#/components/schemas/Amount"
        );
    }

    #[test]
    fn test_rewrite_self_refs_to_parent() {
        let mut schema = json!({
            "allOf": [
                { "$ref": "#" },
                { "type": "object", "properties": { "selected": { "type": "boolean" } } }
            ]
        });

        rewrite_self_refs_to_parent(&mut schema, "PaymentInstrument");

        assert_eq!(
            schema["allOf"][0]["$ref"],
            "#/components/schemas/PaymentInstrument"
        );
    }

    #[test]
    fn test_distribute_properties_in_bare_anyof() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "enum": {
                    "type": "array",
                    "minItems": 1
                },
                "const": {}
            },
            "anyOf": [
                { "required": ["enum"] },
                { "required": ["const"] }
            ],
            "additionalProperties": false
        });

        distribute_properties_in_bare_anyof(&mut schema);

        let any_of = schema["anyOf"].as_array().unwrap();
        assert_eq!(any_of[0]["type"], "object");
        assert_eq!(any_of[0]["required"], json!(["enum"]));
        assert!(any_of[0]["properties"].get("enum").is_some());
        assert!(any_of[0]["properties"].get("const").is_some());
        assert_eq!(any_of[0]["additionalProperties"], false);

        assert_eq!(any_of[1]["type"], "object");
        assert_eq!(any_of[1]["required"], json!(["const"]));
        assert!(any_of[1]["properties"].get("enum").is_some());
        assert!(any_of[1]["properties"].get("const").is_some());
        assert_eq!(any_of[1]["additionalProperties"], false);
    }

    #[test]
    fn test_slice_directional_schemas() {
        let checkout_schema = json!({
            "title": "Checkout",
            "type": "object",
            "x-ucp-lifecycle": ["complete", "cancel"],
            "required": ["id", "line_items", "status"],
            "properties": {
                "id": {
                    "type": "string",
                    "ucp_request": {
                        "create": "omit",
                        "update": "required"
                    }
                },
                "line_items": {
                    "type": "array",
                    "items": { "$ref": "https://ucp.dev/draft/schemas/shopping/types/line_item.json" },
                    "ucp_request": {
                        "create": "required",
                        "update": "required"
                    }
                },
                "status": {
                    "type": "string",
                    "ucp_request": "omit"
                }
            }
        });

        let sliced = slice_directional_schemas(&checkout_schema, "Checkout", false).unwrap();
        assert_eq!(sliced.len(), 4);

        // CheckoutCreateRequest
        let (name0, create_req) = &sliced[0];
        assert_eq!(name0, "CheckoutCreateRequest");
        assert!(create_req["properties"].get("id").is_none());
        assert!(create_req["properties"].get("status").is_none());
        assert!(create_req["properties"].get("line_items").is_some());
        assert_eq!(
            create_req["properties"]["line_items"]["items"]["$ref"],
            "#/components/schemas/LineItem"
        );

        // CheckoutUpdateRequest
        let (name1, update_req) = &sliced[1];
        assert_eq!(name1, "CheckoutUpdateRequest");
        assert_eq!(update_req["properties"]["id"]["type"], "string");
        assert!(update_req["properties"].get("status").is_none());

        // CheckoutCompleteRequest
        let (name2, complete_req) = &sliced[2];
        assert_eq!(name2, "CheckoutCompleteRequest");
        assert!(complete_req.is_object());

        // Checkout (Response)
        let (name3, resp) = &sliced[3];
        assert_eq!(name3, "Checkout");
        assert_eq!(resp["properties"]["id"]["type"], "string");
        assert_eq!(resp["properties"]["status"]["type"], "string");
    }

    #[test]
    fn test_strip_ucp_keywords_allof_required_preservation() {
        let mut schema = json!({
            "type": "object",
            "required": ["root_prop", "allof_prop", "missing_prop"],
            "properties": {
                "root_prop": { "type": "string" }
            },
            "allOf": [
                {
                    "type": "object",
                    "properties": {
                        "allof_prop": { "type": "integer" }
                    }
                }
            ]
        });

        strip_ucp_keywords(&mut schema);

        let reqs = schema["required"].as_array().unwrap();
        assert!(reqs.contains(&json!("root_prop")));
        assert!(reqs.contains(&json!("allof_prop")));
        assert!(!reqs.contains(&json!("missing_prop")));
    }
}
