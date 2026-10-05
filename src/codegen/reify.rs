//! Stage 4 & Stage 7: Polymorphic conditional variant hoisting, open-union lowering,
//! and decentralized subtype discovery.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Map, Value};

use crate::codegen::hoist::insert_sliced_or_normalized_def;
use crate::codegen::normalizer::{merge_schema_property, to_pascal_case};
use crate::codegen::CodegenError;
use crate::types::UCP_ANNOTATIONS;

/// Derive a canonical PascalCase variant name from `parent_name` and a discriminator `tag_value`.
///
/// - If `to_pascal_case(tag_value)` already ends with `parent_name` (case-insensitive), returns it.
/// - If `parent_name` splits into `>= 2` PascalCase words (e.g. `Fulfillment` + `Method`), replaces
///   the leading PascalCase word with `to_pascal_case(tag_value)` (`ShippingMethod`, `PickupMethod`).
/// - Otherwise prepends `to_pascal_case(tag_value)` to `parent_name` (`Oauth2Provider`).
pub fn derive_variant_name(parent_name: &str, tag_value: &str) -> String {
    let tag_pascal = to_pascal_case(tag_value);
    if tag_pascal
        .to_ascii_lowercase()
        .ends_with(&parent_name.to_ascii_lowercase())
    {
        return tag_pascal;
    }
    let words = split_pascal_words(parent_name);
    if words.len() >= 2 {
        let suffix: String = words[1..].concat();
        if tag_pascal
            .to_ascii_lowercase()
            .ends_with(&suffix.to_ascii_lowercase())
        {
            return tag_pascal;
        }
        return format!("{tag_pascal}{suffix}");
    }
    format!("{tag_pascal}{parent_name}")
}

/// Hoist inline `if`/`then` shape variants in `root_raw_schemas` (`FulfillmentMethod` ->
/// `ShippingMethod`, `PickupMethod`) and `defs` (`Provider` -> `Oauth2Provider`) into named
/// schemas before directional slicing, leaving scalar value constraints (`Total`, `PostalAddress`)
/// untouched.
pub fn hoist_inline_conditional_variants(
    root_raw_schemas: &mut BTreeMap<String, Value>,
    defs: &mut BTreeMap<String, Value>,
    sliced_base_names: &mut BTreeSet<String>,
) -> Result<(), CodegenError> {
    let root_entries: Vec<(String, Value)> = root_raw_schemas
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for (parent_name, mut parent_schema) in root_entries {
        let hoisted = extract_hoisted_variants(&parent_name, &mut parent_schema);
        if hoisted.is_empty() {
            continue;
        }
        root_raw_schemas.insert(parent_name, parent_schema);
        for (variant_name, variant_schema) in hoisted {
            root_raw_schemas.insert(variant_name, variant_schema);
        }
    }

    let def_entries: Vec<(String, Value)> =
        defs.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    for (parent_name, mut parent_schema) in def_entries {
        let hoisted = extract_hoisted_variants(&parent_name, &mut parent_schema);
        if hoisted.is_empty() {
            continue;
        }
        defs.insert(parent_name.clone(), parent_schema);
        for (variant_name, variant_schema) in hoisted {
            insert_sliced_or_normalized_def(
                &variant_schema,
                &variant_name,
                &parent_name,
                false,
                defs,
                sliced_base_names,
            )?;
        }
    }

    Ok(())
}

fn extract_hoisted_variants(parent_name: &str, parent_schema: &mut Value) -> Vec<(String, Value)> {
    let base_template = parent_schema.clone();
    let Some(base_obj) = base_template.as_object() else {
        return Vec::new();
    };
    let Some(all_of) = parent_schema
        .as_object_mut()
        .and_then(|o| o.get_mut("allOf"))
        .and_then(Value::as_array_mut)
    else {
        return Vec::new();
    };

    let mut hoisted = Vec::new();
    for branch in all_of.iter_mut() {
        let Some(branch_obj) = branch.as_object_mut() else {
            continue;
        };
        let Some(if_obj) = branch_obj.get("if").and_then(Value::as_object) else {
            continue;
        };
        let Some(then_obj) = branch_obj.get("then").and_then(Value::as_object).cloned() else {
            continue;
        };
        if then_obj.contains_key("$ref") || !is_shape_variant_branch(base_obj, &then_obj) {
            continue;
        }
        let Some((disc_prop, tag_val)) = extract_if_discriminator(if_obj) else {
            continue;
        };

        let variant_name = derive_variant_name(parent_name, &tag_val);
        let variant_schema = build_hoisted_variant_schema(
            &base_template,
            &then_obj,
            &variant_name,
            &disc_prop,
            &tag_val,
        );
        branch_obj.insert(
            "then".to_string(),
            json!({ "$ref": format!("#/$defs/{variant_name}") }),
        );
        hoisted.push((variant_name, variant_schema));
    }
    hoisted
}

fn is_shape_variant_branch(parent_obj: &Map<String, Value>, then_obj: &Map<String, Value>) -> bool {
    let Some(Value::Object(then_props)) = then_obj.get("properties") else {
        return false;
    };
    let parent_props = parent_obj.get("properties").and_then(Value::as_object);
    for (prop_key, then_prop_val) in then_props {
        let Some(base_prop_val) = parent_props.and_then(|pp| pp.get(prop_key)) else {
            return true;
        };
        let changes_direct_ref = then_prop_val.get("$ref").is_some()
            && then_prop_val.get("$ref") != base_prop_val.get("$ref");
        let then_items_ref = then_prop_val.get("items").and_then(|i| i.get("$ref"));
        let base_items_ref = base_prop_val.get("items").and_then(|i| i.get("$ref"));
        let changes_items_ref = then_items_ref.is_some() && then_items_ref != base_items_ref;
        if changes_direct_ref || changes_items_ref {
            return true;
        }
    }
    false
}

fn build_hoisted_variant_schema(
    base_template: &Value,
    then_obj: &Map<String, Value>,
    variant_name: &str,
    disc_prop: &str,
    tag_val: &str,
) -> Value {
    let mut variant_schema = base_template.clone();
    let Some(variant_obj) = variant_schema.as_object_mut() else {
        return variant_schema;
    };
    variant_obj.remove("allOf");
    variant_obj.remove("dependentRequired");
    variant_obj.insert("title".to_string(), Value::String(variant_name.to_string()));

    let variant_props = variant_obj
        .entry("properties".to_string())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .expect("properties is an object");
    if let Some(Value::Object(then_props)) = then_obj.get("properties") {
        for (k, then_prop) in then_props {
            if let Some(existing_prop) = variant_props.get_mut(k) {
                merge_schema_property(existing_prop, then_prop);
            } else {
                variant_props.insert(k.clone(), then_prop.clone());
            }
        }
    }

    let disc_entry = variant_props
        .entry(disc_prop.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(disc_map) = disc_entry.as_object_mut() {
        for key in UCP_ANNOTATIONS {
            disc_map.remove(*key);
        }
        disc_map.remove("enum");
        disc_map.remove("$ref");
        disc_map.insert("type".to_string(), Value::String("string".to_string()));
        disc_map.insert("const".to_string(), Value::String(tag_val.to_string()));
    }

    let req_arr = variant_obj
        .entry("required".to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .expect("required is an array");
    if let Some(Value::Array(then_reqs)) = then_obj.get("required") {
        for req in then_reqs {
            if !req_arr.contains(req) {
                req_arr.push(req.clone());
            }
        }
    }
    let disc_val = Value::String(disc_prop.to_string());
    if !req_arr.contains(&disc_val) {
        req_arr.push(disc_val);
    }

    variant_schema
}

pub(crate) fn extract_if_discriminator(if_obj: &Map<String, Value>) -> Option<(String, String)> {
    let props = if_obj.get("properties")?.as_object()?;
    for (prop_name, prop_val) in props {
        if let Some(tag) = extract_const_or_single_enum_str(prop_val) {
            return Some((prop_name.clone(), tag));
        }
    }
    None
}

pub(crate) fn extract_const_or_single_enum_str(prop_val: &Value) -> Option<String> {
    let obj = prop_val.as_object()?;
    if let Some(s) = obj.get("const").and_then(Value::as_str) {
        return Some(s.to_string());
    }
    let arr = obj.get("enum")?.as_array()?;
    (arr.len() == 1)
        .then(|| arr[0].as_str().map(String::from))
        .flatten()
}

fn split_pascal_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    for ch in s.chars() {
        if ch.is_ascii_uppercase() && !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
        current.push(ch);
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_variant_name_handles_compound_and_single_word_parents() {
        assert_eq!(
            derive_variant_name("FulfillmentMethod", "shipping"),
            "ShippingMethod"
        );
        assert_eq!(
            derive_variant_name("FulfillmentMethod", "pickup"),
            "PickupMethod"
        );
        assert_eq!(derive_variant_name("Provider", "oauth2"), "Oauth2Provider");
        assert_eq!(
            derive_variant_name("FulfillmentMethod", "shipping_method"),
            "ShippingMethod"
        );
        assert_eq!(
            derive_variant_name("Provider", "oauth2_provider"),
            "Oauth2Provider"
        );
    }

    #[test]
    fn hoist_inline_conditional_variants_reifies_shape_variants_and_skips_scalar_constraints() {
        let mut root_raw = BTreeMap::from([
            (
                "FulfillmentMethod".to_string(),
                json!({
                    "type": "object",
                    "required": ["id", "type"],
                    "properties": {
                        "id": { "type": "string", "ucp_request": "omit" },
                        "type": { "type": "string", "ucp_request": { "create": "required", "update": "optional" } },
                        "destinations": {
                            "type": "array",
                            "ucp_request": "omit",
                            "items": { "$ref": "#/$defs/FulfillmentDestination" }
                        }
                    },
                    "allOf": [
                        {
                            "if": { "properties": { "type": { "const": "shipping" } } },
                            "then": {
                                "properties": {
                                    "destinations": {
                                        "ucp_request": "optional",
                                        "items": { "$ref": "#/$defs/ShippingDestination" }
                                    }
                                }
                            }
                        },
                        {
                            "if": { "properties": { "type": { "const": "pickup" } } },
                            "then": {
                                "properties": {
                                    "destinations": {
                                        "ucp_request": "omit",
                                        "items": { "$ref": "#/$defs/LocationDestination" }
                                    }
                                }
                            }
                        }
                    ]
                }),
            ),
            (
                "Total".to_string(),
                json!({
                    "type": "object",
                    "required": ["type", "amount"],
                    "properties": {
                        "type": { "type": "string" },
                        "amount": { "$ref": "#/$defs/SignedAmount" }
                    },
                    "allOf": [
                        {
                            "if": { "properties": { "type": { "const": "discount" } } },
                            "then": { "properties": { "amount": { "exclusiveMaximum": 0 } } }
                        }
                    ]
                }),
            ),
        ]);

        let mut defs = BTreeMap::from([(
            "Provider".to_string(),
            json!({
                "title": "Provider",
                "type": "object",
                "required": ["type"],
                "properties": {
                    "type": { "type": "string" }
                },
                "allOf": [
                    {
                        "if": { "properties": { "type": { "const": "oauth2" } } },
                        "then": {
                            "required": ["auth_url"],
                            "properties": {
                                "auth_url": { "type": "string", "format": "uri" }
                            }
                        }
                    }
                ],
                "additionalProperties": true
            }),
        )]);
        let mut sliced_base_names = BTreeSet::new();

        hoist_inline_conditional_variants(&mut root_raw, &mut defs, &mut sliced_base_names)
            .unwrap();

        assert!(root_raw.contains_key("ShippingMethod"));
        assert!(root_raw.contains_key("PickupMethod"));
        assert_eq!(
            root_raw["ShippingMethod"]["properties"]["destinations"]["type"],
            "array"
        );
        assert_eq!(
            root_raw["ShippingMethod"]["properties"]["destinations"]["ucp_request"],
            "optional"
        );
        assert_eq!(
            root_raw["ShippingMethod"]["properties"]["destinations"]["items"]["$ref"],
            "#/$defs/ShippingDestination"
        );
        assert_eq!(
            root_raw["FulfillmentMethod"]["allOf"][0]["then"]["$ref"],
            "#/$defs/ShippingMethod"
        );

        assert!(defs.contains_key("Oauth2Provider"));
        assert_eq!(defs["Oauth2Provider"]["title"], "Oauth2Provider");
        assert_eq!(
            defs["Oauth2Provider"]["properties"]["type"]["const"],
            "oauth2"
        );
        assert_eq!(
            defs["Oauth2Provider"]["properties"]["auth_url"]["format"],
            "uri"
        );
        assert_eq!(
            defs["Provider"]["allOf"][0]["then"]["$ref"],
            "#/$defs/Oauth2Provider"
        );

        assert!(!root_raw.contains_key("DiscountTotal"));
        assert!(root_raw["Total"]["allOf"][0]["then"].get("$ref").is_none());
    }
}
