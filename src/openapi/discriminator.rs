//! Discriminator synthesis and conditional polymorphism transformation.
//!
//! Transforms UCP `allOf` + `if`/`then` conditional validation branches (PR #688)
//! into first-class OpenAPI 3.1 `oneOf` + `discriminator` { propertyName, mapping } constructs.

use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

use crate::compose::capability_short_name;

/// Convert an identifier string (snake_case, kebab-case, space-separated, or dot-separated)
/// into PascalCase.
pub fn to_pascal_case(s: &str) -> String {
    // If it's a dotted string like dev.ucp.shopping.checkout, take the last segment
    let short_name;
    let s = if s.contains('.') && !s.ends_with(".json") {
        short_name = capability_short_name(s);
        short_name.as_str()
    } else {
        s
    };

    let separators = ['_', '-', ' ', '/'];
    if !s.chars().any(|c| separators.contains(&c)) {
        let mut chars = s.chars();
        return match chars.next() {
            None => String::new(),
            Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        };
    }

    let mut result = String::new();
    for token in s.split(|c| separators.contains(&c)) {
        if token.is_empty() {
            continue;
        }
        let is_all_upper = token
            .chars()
            .all(|c| c.is_ascii_uppercase() || !c.is_alphabetic());
        let mut chars = token.chars();
        if let Some(first) = chars.next() {
            result.push(first.to_ascii_uppercase());
            if is_all_upper {
                for c in chars {
                    result.push(c.to_ascii_lowercase());
                }
            } else {
                for c in chars {
                    result.push(c);
                }
            }
        }
    }

    result
}

/// Convert a `$ref` string, URL, or filename into a Component Schema Name (PascalCase).
///
/// Examples:
/// - `https://ucp.dev/draft/schemas/shopping/types/shipping_destination.json` -> `ShippingDestination`
/// - `../types/location_destination.json` -> `LocationDestination`
/// - `#/components/schemas/PostalAddress` -> `PostalAddress`
/// - `#/$defs/line_item` -> `LineItem`
/// - `checkout.json#/$defs/line_item` -> `LineItem`
/// - `totals.json` -> `Totals`
pub fn ref_to_component_name(ref_str: &str) -> String {
    if let Some(stripped) = ref_str.strip_prefix("#/components/schemas/") {
        return stripped.to_string();
    }
    if let Some(pos) = ref_str.find("#/$defs/") {
        let def_name = &ref_str[pos + "#/$defs/".len()..];
        let def_pascal = to_pascal_case(def_name);
        if pos > 0 {
            let path_part = &ref_str[..pos];
            let last_segment = path_part.rsplit('/').next().unwrap_or(path_part);
            let stem = last_segment.strip_suffix(".json").unwrap_or(last_segment);
            let parent_pascal = to_pascal_case(stem);
            if crate::openapi::normalizer::is_generic_def_name(def_name)
                || crate::openapi::normalizer::is_generic_def_name(&def_pascal)
            {
                return format!("{}{}", parent_pascal, def_pascal);
            }
        }
        return def_pascal;
    }

    // URL or file path: get the last path segment before any fragment
    let path_part = ref_str.split('#').next().unwrap_or(ref_str);
    let last_segment = path_part.rsplit('/').next().unwrap_or(path_part);
    let stem = last_segment.strip_suffix(".json").unwrap_or(last_segment);

    to_pascal_case(stem)
}

/// Information about a single conditional branch in an `allOf` list.
#[derive(Debug, Clone)]
struct ConditionalBranch {
    property_name: String,
    discriminator_value: String,
    target_component_ref: String,
}

/// Inspect a single `allOf` branch to see if it matches `{ if: { properties: { <prop>: { const: <val> } } }, then: { $ref: <ref> } }`.
fn parse_conditional_branch(branch: &Value) -> Option<ConditionalBranch> {
    let branch_obj = branch.as_object()?;
    let if_obj = branch_obj.get("if")?.as_object()?;
    let then_obj = branch_obj.get("then")?.as_object()?;

    // Target $ref in then
    let target_ref = then_obj.get("$ref")?.as_str()?;
    let component_name = ref_to_component_name(target_ref);
    let target_component_ref = format!("#/components/schemas/{}", component_name);

    // Extract property and const value in `if`
    let props = if_obj.get("properties")?.as_object()?;
    for (prop_name, prop_val) in props {
        let prop_obj = prop_val.as_object()?;
        if let Some(const_val) = prop_obj.get("const").and_then(|v| v.as_str()) {
            return Some(ConditionalBranch {
                property_name: prop_name.clone(),
                discriminator_value: const_val.to_string(),
                target_component_ref,
            });
        }
        if let Some(enum_arr) = prop_obj.get("enum").and_then(|v| v.as_array()) {
            if let Some(first_enum) = enum_arr.first().and_then(|v| v.as_str()) {
                return Some(ConditionalBranch {
                    property_name: prop_name.clone(),
                    discriminator_value: first_enum.to_string(),
                    target_component_ref,
                });
            }
        }
    }

    None
}

/// Transform conditional `allOf` branches in a single schema object into `oneOf` + `discriminator`.
///
/// Returns true if transformation was applied.
pub fn transform_object_conditionals(schema_obj: &mut Map<String, Value>) -> bool {
    let all_of = match schema_obj.get("allOf").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return false,
    };

    let mut conditional_branches = Vec::new();
    let mut remaining_all_of = Vec::new();

    for item in all_of {
        if let Some(cond) = parse_conditional_branch(item) {
            conditional_branches.push(cond);
        } else {
            remaining_all_of.push(item.clone());
        }
    }

    if conditional_branches.is_empty() {
        return false;
    }

    // Verify all conditional branches use the same discriminator property name
    let prop_name = conditional_branches[0].property_name.clone();
    for branch in &conditional_branches {
        if branch.property_name != prop_name {
            // Inconsistent discriminator properties, skip transformation
            return false;
        }
    }

    // Build mapping and oneOf refs
    let mut mapping = BTreeMap::new();
    let mut one_of_refs = BTreeSet::new();

    for branch in conditional_branches {
        mapping.insert(
            branch.discriminator_value,
            branch.target_component_ref.clone(),
        );
        one_of_refs.insert(branch.target_component_ref);
    }

    // Update allOf: keep remaining or remove if empty
    if remaining_all_of.is_empty() {
        schema_obj.remove("allOf");
    } else {
        schema_obj.insert("allOf".to_string(), Value::Array(remaining_all_of));
    }

    // Insert oneOf
    let one_of_array: Vec<Value> = one_of_refs
        .into_iter()
        .map(|r| serde_json::json!({ "$ref": r }))
        .collect();
    schema_obj.insert("oneOf".to_string(), Value::Array(one_of_array));

    // Insert discriminator
    schema_obj.insert(
        "discriminator".to_string(),
        serde_json::json!({
            "propertyName": prop_name,
            "mapping": mapping
        }),
    );

    true
}

/// Recursively walk a JSON Value and transform any `allOf` + `if`/`then` conditional
/// polymorphism into `oneOf` + `discriminator`.
pub fn transform_schema_conditionals(value: &mut Value) -> bool {
    let mut transformed = false;

    match value {
        Value::Object(map) => {
            if transform_object_conditionals(map) {
                transformed = true;
            }

            // Recurse into child properties, $defs, items, etc.
            for (_k, v) in map.iter_mut() {
                if transform_schema_conditionals(v) {
                    transformed = true;
                }
            }
        }
        Value::Array(arr) => {
            for item in arr {
                if transform_schema_conditionals(item) {
                    transformed = true;
                }
            }
        }
        _ => {}
    }

    transformed
}

/// Find all property names that have a constant or single-enum value in a schema.
pub fn find_const_properties(schema: &Value) -> BTreeMap<String, String> {
    let mut const_props = BTreeMap::new();

    // Check direct properties
    if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
        for (k, v) in props {
            if let Some(const_str) = extract_const_or_single_enum(v) {
                const_props.insert(k.clone(), const_str);
            }
        }
    }

    // Check allOf branches
    if let Some(all_of) = schema.get("allOf").and_then(|a| a.as_array()) {
        for branch in all_of {
            if let Some(props) = branch.get("properties").and_then(|p| p.as_object()) {
                for (k, v) in props {
                    if let Some(const_str) = extract_const_or_single_enum(v) {
                        const_props.insert(k.clone(), const_str);
                    }
                }
            }
        }
    }

    const_props
}

/// Extract const string value or single-item enum value from a property definition.
fn extract_const_or_single_enum(prop_val: &Value) -> Option<String> {
    let obj = prop_val.as_object()?;
    if let Some(c) = obj.get("const").and_then(|v| v.as_str()) {
        return Some(c.to_string());
    }
    if let Some(enum_arr) = obj.get("enum").and_then(|v| v.as_array()) {
        if enum_arr.len() == 1 {
            if let Some(first) = enum_arr[0].as_str() {
                return Some(first.to_string());
            }
        }
    }
    None
}

/// Get the constant value of a specific property in a schema (from direct properties or allOf).
pub fn get_const_property_value(schema: &Value, prop_name: &str) -> Option<String> {
    if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
        if let Some(prop_val) = props.get(prop_name) {
            if let Some(val) = extract_const_or_single_enum(prop_val) {
                return Some(val);
            }
        }
    }

    if let Some(all_of) = schema.get("allOf").and_then(|a| a.as_array()) {
        for branch in all_of {
            if let Some(props) = branch.get("properties").and_then(|p| p.as_object()) {
                if let Some(prop_val) = props.get(prop_name) {
                    if let Some(val) = extract_const_or_single_enum(prop_val) {
                        return Some(val);
                    }
                }
            }
        }
    }

    None
}

/// Synthesize explicit OpenAPI 3.1 `discriminator` on any `oneOf` union schema
/// (such as `Message` -> `MessageError`, `MessageWarning`, `MessageInfo`)
/// whose target variant schemas define a consistent constant discriminator property.
pub fn synthesize_oneof_discriminators(schemas: &mut BTreeMap<String, Value>) {
    let schema_names: Vec<String> = schemas.keys().cloned().collect();

    for schema_name in schema_names {
        let needs_discriminator = {
            if let Some(schema_val) = schemas.get(&schema_name) {
                schema_val.get("oneOf").is_some() && schema_val.get("discriminator").is_none()
            } else {
                false
            }
        };

        if !needs_discriminator {
            continue;
        }

        let variant_comp_names: Vec<String> = {
            let schema_val = match schemas.get(&schema_name) {
                Some(s) => s,
                None => continue,
            };
            let one_of_arr = match schema_val.get("oneOf").and_then(|v| v.as_array()) {
                Some(arr) => arr,
                None => continue,
            };
            let mut names = Vec::new();
            for item in one_of_arr {
                if let Some(ref_str) = item.get("$ref").and_then(|v| v.as_str()) {
                    if let Some(comp_name) = ref_str.strip_prefix("#/components/schemas/") {
                        names.push(comp_name.to_string());
                    }
                }
            }
            names
        };

        if variant_comp_names.len() < 2 {
            continue;
        }

        let first_variant = match schemas.get(&variant_comp_names[0]) {
            Some(v) => v,
            None => continue,
        };

        let candidate_props = find_const_properties(first_variant);
        if candidate_props.is_empty() {
            continue;
        }

        for (prop_name, _) in candidate_props {
            let mut mapping = BTreeMap::new();
            let mut all_match = true;

            for comp_name in &variant_comp_names {
                let variant_schema = match schemas.get(comp_name) {
                    Some(s) => s,
                    None => {
                        all_match = false;
                        break;
                    }
                };

                if let Some(const_val) = get_const_property_value(variant_schema, &prop_name) {
                    let comp_ref = format!("#/components/schemas/{}", comp_name);
                    mapping.insert(const_val, comp_ref);
                } else {
                    all_match = false;
                    break;
                }
            }

            if all_match && mapping.len() == variant_comp_names.len() {
                if let Some(Value::Object(map)) = schemas.get_mut(&schema_name) {
                    map.insert(
                        "discriminator".to_string(),
                        serde_json::json!({
                            "propertyName": prop_name,
                            "mapping": mapping
                        }),
                    );
                }
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_to_pascal_case() {
        assert_eq!(
            to_pascal_case("shipping_destination"),
            "ShippingDestination"
        );
        assert_eq!(
            to_pascal_case("fulfillment_destination"),
            "FulfillmentDestination"
        );
        assert_eq!(
            to_pascal_case("Fulfillment Destination"),
            "FulfillmentDestination"
        );
        assert_eq!(to_pascal_case("ucp-agent"), "UcpAgent");
        assert_eq!(to_pascal_case("UCP-Agent"), "UcpAgent");
        assert_eq!(to_pascal_case("id"), "Id");
        assert_eq!(to_pascal_case("dev.ucp.shopping.checkout"), "Checkout");
        assert_eq!(to_pascal_case("ShippingDestination"), "ShippingDestination");
    }

    #[test]
    fn test_ref_to_component_name() {
        assert_eq!(
            ref_to_component_name(
                "https://ucp.dev/draft/schemas/shopping/types/shipping_destination.json"
            ),
            "ShippingDestination"
        );
        assert_eq!(
            ref_to_component_name("../types/location_destination.json"),
            "LocationDestination"
        );
        assert_eq!(
            ref_to_component_name("#/components/schemas/PostalAddress"),
            "PostalAddress"
        );
        assert_eq!(ref_to_component_name("#/$defs/line_item"), "LineItem");
        assert_eq!(
            ref_to_component_name("checkout.json#/$defs/line_item"),
            "LineItem"
        );
        assert_eq!(
            ref_to_component_name(
                "https://ucp.dev/schemas/shopping/checkout.json#/$defs/line_item"
            ),
            "LineItem"
        );
        assert_eq!(ref_to_component_name("totals.json"), "Totals");
    }

    #[test]
    fn test_transform_fulfillment_destination() {
        let mut schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://ucp.dev/draft/schemas/shopping/types/fulfillment_destination.json",
            "title": "Fulfillment Destination",
            "type": "object",
            "required": ["type", "id"],
            "properties": {
                "type": {
                    "type": "string",
                    "description": "Discriminator"
                },
                "id": {
                    "type": "string"
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
        });

        let changed = transform_schema_conditionals(&mut schema);
        assert!(changed);

        assert!(schema.get("allOf").is_none());

        let one_of = schema.get("oneOf").unwrap().as_array().unwrap();
        assert_eq!(one_of.len(), 2);
        assert_eq!(
            one_of[0],
            json!({ "$ref": "#/components/schemas/LocationDestination" })
        );
        assert_eq!(
            one_of[1],
            json!({ "$ref": "#/components/schemas/ShippingDestination" })
        );

        let discriminator = schema.get("discriminator").unwrap();
        assert_eq!(discriminator["propertyName"], "type");
        assert_eq!(
            discriminator["mapping"]["shipping_address"],
            "#/components/schemas/ShippingDestination"
        );
        assert_eq!(
            discriminator["mapping"]["business_location"],
            "#/components/schemas/LocationDestination"
        );
    }

    #[test]
    fn test_synthesize_oneof_discriminators_for_message() {
        let mut schemas = BTreeMap::new();

        schemas.insert(
            "Message".to_string(),
            json!({
                "title": "Message",
                "type": "object",
                "oneOf": [
                    { "$ref": "#/components/schemas/MessageError" },
                    { "$ref": "#/components/schemas/MessageWarning" },
                    { "$ref": "#/components/schemas/MessageInfo" }
                ]
            }),
        );

        schemas.insert(
            "MessageError".to_string(),
            json!({
                "title": "Message Error",
                "type": "object",
                "properties": {
                    "type": { "type": "string", "const": "error" },
                    "content": { "type": "string" }
                }
            }),
        );

        schemas.insert(
            "MessageWarning".to_string(),
            json!({
                "title": "Message Warning",
                "type": "object",
                "properties": {
                    "type": { "type": "string", "const": "warning" },
                    "content": { "type": "string" }
                }
            }),
        );

        schemas.insert(
            "MessageInfo".to_string(),
            json!({
                "title": "Message Info",
                "type": "object",
                "properties": {
                    "type": { "type": "string", "const": "info" },
                    "content": { "type": "string" }
                }
            }),
        );

        synthesize_oneof_discriminators(&mut schemas);

        let message = &schemas["Message"];
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
}
