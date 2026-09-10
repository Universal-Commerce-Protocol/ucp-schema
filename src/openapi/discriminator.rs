//! Discriminator synthesis and conditional polymorphism transformation.
//!
//! Transforms UCP `allOf` + `if`/`then` conditional validation branches (PR #688)
//! into first-class OpenAPI 3.1 `oneOf` + `discriminator` { propertyName, mapping } constructs.

use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

use crate::compose::capability_short_name;
use crate::openapi::normalizer::attach_const_defaults;

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
            conditional_branches.push((cond, item.clone()));
        } else {
            remaining_all_of.push(item.clone());
        }
    }

    if conditional_branches.is_empty() {
        return false;
    }

    // Isolate branches using the primary discriminator property
    let prop_name = conditional_branches[0].0.property_name.clone();
    let mut matching_branches = Vec::new();

    for (branch, original_item) in conditional_branches {
        if branch.property_name == prop_name {
            matching_branches.push(branch);
        } else {
            remaining_all_of.push(original_item);
        }
    }

    // Build mapping and oneOf refs
    let mut mapping = BTreeMap::new();
    let mut one_of_refs = BTreeSet::new();

    for branch in matching_branches {
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

/// Hoists inline conditional `if`/`then` branches out of parent schemas and synthesizes
/// first-class component variant schemas for them, rewriting the original branches to `$ref`.
pub fn hoist_inline_conditional_variants(schemas: &mut BTreeMap<String, Value>) {
    let schema_names: Vec<String> = schemas.keys().cloned().collect();

    for parent_name in schema_names {
        let parent_schema_val = match schemas.get(&parent_name) {
            Some(s) => s.clone(),
            None => continue,
        };

        let mut parent_obj = match parent_schema_val.as_object() {
            Some(obj) => obj.clone(),
            None => continue,
        };

        let mut all_of_arr = match parent_obj.get("allOf").and_then(|v| v.as_array()) {
            Some(arr) => arr.clone(),
            None => continue,
        };

        let mut modified = false;

        for branch in all_of_arr.iter_mut() {
            if let Some(branch_obj) = branch.as_object_mut() {
                if let Some(if_obj) = branch_obj.get("if").and_then(|v| v.as_object()) {
                    if let Some(then_obj) = branch_obj.get("then").and_then(|v| v.as_object()) {
                        // Skip if it is already a $ref
                        if then_obj.contains_key("$ref") {
                            continue;
                        }

                        // Only hoist if the conditional branch defines a specialized variant introducing
                        // new properties beyond the parent base schema (polymorphism), rather than merely
                        // applying a validation constraint to an existing property.
                        let parent_props = parent_obj.get("properties").and_then(|p| p.as_object());
                        let introduces_new_props = if let Some(then_props) =
                            then_obj.get("properties").and_then(|p| p.as_object())
                        {
                            match parent_props {
                                Some(pp) => then_props.keys().any(|k| !pp.contains_key(k)),
                                None => !then_props.is_empty(),
                            }
                        } else {
                            false
                        };

                        if !introduces_new_props {
                            continue;
                        }

                        if let Some(props) = if_obj.get("properties").and_then(|p| p.as_object()) {
                            let mut prop_name_match = None;
                            let mut const_val_match = None;

                            for (k, v) in props {
                                if let Some(val) = extract_const_or_single_enum(v) {
                                    prop_name_match = Some(k.clone());
                                    const_val_match = Some(val);
                                    break;
                                }
                            }

                            if let (Some(prop_name), Some(const_val)) =
                                (prop_name_match, const_val_match)
                            {
                                let const_pascal = to_pascal_case(&const_val);
                                let variant_name = if const_pascal
                                    .to_lowercase()
                                    .ends_with(&parent_name.to_lowercase())
                                {
                                    const_pascal
                                } else {
                                    format!("{}{}", const_pascal, parent_name)
                                };

                                let mut variant_schema = parent_schema_val.clone();
                                if let Some(variant_obj) = variant_schema.as_object_mut() {
                                    variant_obj.remove("allOf");

                                    // Merge `then.properties` into properties.
                                    if let Some(then_props) =
                                        then_obj.get("properties").and_then(|p| p.as_object())
                                    {
                                        let props_entry = variant_obj
                                            .entry("properties".to_string())
                                            .or_insert_with(
                                                || Value::Object(serde_json::Map::new()),
                                            )
                                            .as_object_mut()
                                            .unwrap();
                                        for (k, v) in then_props {
                                            props_entry.insert(k.clone(), v.clone());
                                        }
                                    }

                                    // Merge `then.required` into required (avoid duplicates).
                                    if let Some(then_req) =
                                        then_obj.get("required").and_then(|r| r.as_array())
                                    {
                                        let req_entry = variant_obj
                                            .entry("required".to_string())
                                            .or_insert_with(|| Value::Array(Vec::new()))
                                            .as_array_mut()
                                            .unwrap();
                                        for r in then_req {
                                            if !req_entry.contains(r) {
                                                req_entry.push(r.clone());
                                            }
                                        }
                                    }

                                    // Fix discriminator property in variant.
                                    if let Some(props_entry) = variant_obj
                                        .get_mut("properties")
                                        .and_then(|p| p.as_object_mut())
                                    {
                                        let fixed_disc = serde_json::json!({
                                            "type": "string",
                                            "const": const_val,
                                            "default": const_val
                                        });
                                        props_entry.insert(prop_name.clone(), fixed_disc);
                                    }
                                }

                                schemas.insert(variant_name.clone(), variant_schema);

                                branch_obj.insert(
                                    "then".to_string(),
                                    serde_json::json!({
                                        "$ref": format!("#/components/schemas/{}", variant_name)
                                    }),
                                );

                                modified = true;
                            }
                        }
                    }
                }
            }
        }

        if modified {
            parent_obj.insert("allOf".to_string(), Value::Array(all_of_arr));
            schemas.insert(parent_name, Value::Object(parent_obj));
        }
    }
}

/// Discovers and registers extended subtypes into base polymorphic schemas.
///
/// In UCP, extensions introduce new polymorphic variants in standalone files without
/// modifying upstream schemas (e.g. `locker_destination.json` extending `fulfillment_destination.json`).
///
/// A schema `Derived` is recognized as an extended subtype of `Base` if:
/// 1. `Base` is a polymorphic schema (has `discriminator` with `propertyName` and `mapping`, or `oneOf`).
/// 2. `Derived` extends `Base` via an `allOf` entry referencing `Base`.
/// 3. `Derived` defines a constant/single-enum value for `Base`'s discriminator property.
pub fn register_extended_subtypes(schemas: &mut BTreeMap<String, Value>) {
    let schema_names: Vec<String> = schemas.keys().cloned().collect();

    // Step 1: Identify all base schemas that define a discriminator
    let mut base_schemas = Vec::new();
    for name in &schema_names {
        if let Some(schema_val) = schemas.get(name) {
            if let Some(prop_name) = schema_val
                .get("discriminator")
                .and_then(|d| d.get("propertyName"))
                .and_then(|p| p.as_str())
            {
                base_schemas.push((name.clone(), prop_name.to_string()));
            }
        }
    }

    if base_schemas.is_empty() {
        return;
    }

    // Step 2: For each base schema, find all derived schemas extending it via allOf
    // Store: (base_name, derived_name, const_val, base_root_name)
    let mut registrations = Vec::new();

    for (base_name, prop_name) in &base_schemas {
        let base_root = base_name
            .strip_suffix("CreateRequest")
            .or_else(|| base_name.strip_suffix("UpdateRequest"))
            .or_else(|| base_name.strip_suffix("CompleteRequest"))
            .unwrap_or(base_name.as_str());
        let base_suffix = base_name.strip_prefix(base_root).unwrap_or("");

        for derived_name in &schema_names {
            if derived_name == base_name || derived_name == base_root {
                continue;
            }

            let derived_val = match schemas.get(derived_name) {
                Some(v) => v,
                None => continue,
            };

            // Check if derived extends base_root (or base_name)
            let extends_base =
                if let Some(all_of) = derived_val.get("allOf").and_then(|a| a.as_array()) {
                    all_of.iter().any(|item| {
                        if let Some(ref_str) = item.get("$ref").and_then(|r| r.as_str()) {
                            if ref_str == format!("#/components/schemas/{}", base_name)
                                || ref_str == format!("#/components/schemas/{}", base_root)
                            {
                                return true;
                            }
                            let comp = ref_to_component_name(ref_str);
                            comp == *base_name || comp == base_root
                        } else {
                            false
                        }
                    })
                } else {
                    false
                };

            if !extends_base {
                continue;
            }

            // If base has a directional suffix, only match derived that has the same suffix
            // (or unsuffixed derived if no suffixed derived exists)
            let derived_root = derived_name
                .strip_suffix("CreateRequest")
                .or_else(|| derived_name.strip_suffix("UpdateRequest"))
                .or_else(|| derived_name.strip_suffix("CompleteRequest"))
                .unwrap_or(derived_name.as_str());
            let derived_suffix = derived_name.strip_prefix(derived_root).unwrap_or("");

            if !base_suffix.is_empty() {
                let suffixed_derived = format!("{}{}", derived_root, base_suffix);
                if schemas.contains_key(&suffixed_derived) {
                    if derived_suffix != base_suffix {
                        continue;
                    }
                } else if !derived_suffix.is_empty() {
                    continue;
                }
            } else if !derived_suffix.is_empty() {
                continue;
            }

            // Check if derived defines the discriminator property as const/enum
            if let Some(const_val) = get_const_property_value(derived_val, prop_name) {
                registrations.push((
                    base_name.clone(),
                    derived_name.clone(),
                    const_val,
                    base_root.to_string(),
                    prop_name.clone(),
                ));
            }
        }
    }

    // Step 3: Apply registrations
    for (base_name, derived_name, const_val, base_root, prop_name) in registrations {
        let derived_ref = format!("#/components/schemas/{}", derived_name);

        // 3a. Update Base: add to discriminator.mapping and oneOf
        if let Some(base_val) = schemas.get_mut(&base_name) {
            if let Some(base_obj) = base_val.as_object_mut() {
                // Update mapping
                let disc_entry = base_obj
                    .entry("discriminator".to_string())
                    .or_insert_with(|| {
                        serde_json::json!({
                            "propertyName": prop_name,
                            "mapping": {}
                        })
                    });
                if let Some(mapping) = disc_entry
                    .get_mut("mapping")
                    .and_then(|m| m.as_object_mut())
                {
                    mapping.insert(const_val.clone(), Value::String(derived_ref.clone()));
                }

                // Update oneOf
                let one_of_entry = base_obj
                    .entry("oneOf".to_string())
                    .or_insert_with(|| Value::Array(Vec::new()));
                if let Some(one_of_arr) = one_of_entry.as_array_mut() {
                    let already_present = one_of_arr.iter().any(|item| {
                        item.get("$ref").and_then(|r| r.as_str()) == Some(&derived_ref)
                    });
                    if !already_present {
                        one_of_arr.push(serde_json::json!({ "$ref": derived_ref }));
                        // Sort oneOf for determinism
                        one_of_arr.sort_by(|a, b| {
                            let ref_a = a.get("$ref").and_then(|r| r.as_str()).unwrap_or("");
                            let ref_b = b.get("$ref").and_then(|r| r.as_str()).unwrap_or("");
                            ref_a.cmp(ref_b)
                        });
                    }
                }
            }
        }

        // 3b. Update Derived: inherit base properties & required, clean up allOf
        let (inherited_props, inherited_req) = {
            let base_lookup = schemas
                .get(&base_name)
                .filter(|b| b.get("properties").is_some())
                .or_else(|| schemas.get(&base_root));
            if let Some(b) = base_lookup {
                let props = b.get("properties").and_then(|p| p.as_object()).cloned();
                let req = b.get("required").and_then(|r| r.as_array()).cloned();
                (props, req)
            } else {
                (None, None)
            }
        };

        if let Some(derived_val) = schemas.get_mut(&derived_name) {
            if let Some(derived_obj) = derived_val.as_object_mut() {
                // Inherit missing properties from Base
                if let Some(props_map) = inherited_props {
                    let derived_props = derived_obj
                        .entry("properties".to_string())
                        .or_insert_with(|| Value::Object(serde_json::Map::new()))
                        .as_object_mut();
                    if let Some(dp) = derived_props {
                        for (k, v) in props_map {
                            if !dp.contains_key(&k) {
                                dp.insert(k, v);
                            }
                        }
                    }
                }

                // Inherit required fields from Base
                if let Some(req_arr) = inherited_req {
                    let derived_req = derived_obj
                        .entry("required".to_string())
                        .or_insert_with(|| Value::Array(Vec::new()))
                        .as_array_mut();
                    if let Some(dr) = derived_req {
                        for r in req_arr {
                            if !dr.contains(&r) {
                                dr.push(r);
                            }
                        }
                    }
                }

                // Remove base ref from allOf
                if let Some(all_of) = derived_obj.get_mut("allOf").and_then(|a| a.as_array_mut()) {
                    all_of.retain(|item| {
                        if let Some(ref_str) = item.get("$ref").and_then(|r| r.as_str()) {
                            let comp = ref_to_component_name(ref_str);
                            comp != base_name && comp != base_root
                        } else {
                            true
                        }
                    });
                    if all_of.is_empty() {
                        derived_obj.remove("allOf");
                    }
                }

                // Ensure discriminator property in derived has default
                attach_const_defaults(derived_val);
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

    #[test]
    fn test_register_extended_subtypes() {
        let mut schemas = BTreeMap::new();

        schemas.insert(
            "FulfillmentDestination".to_string(),
            json!({
                "title": "Fulfillment Destination",
                "type": "object",
                "required": ["type", "id"],
                "properties": {
                    "id": { "type": "string", "description": "Destination ID" },
                    "type": { "type": "string", "description": "Discriminator" }
                },
                "oneOf": [
                    { "$ref": "#/components/schemas/LocationDestination" },
                    { "$ref": "#/components/schemas/ShippingDestination" }
                ],
                "discriminator": {
                    "propertyName": "type",
                    "mapping": {
                        "business_location": "#/components/schemas/LocationDestination",
                        "shipping_address": "#/components/schemas/ShippingDestination"
                    }
                }
            }),
        );

        schemas.insert(
            "LockerDestination".to_string(),
            json!({
                "title": "Locker Destination",
                "type": "object",
                "allOf": [
                    { "$ref": "#/components/schemas/FulfillmentDestination" }
                ],
                "required": ["locker_id"],
                "properties": {
                    "type": { "type": "string", "const": "parcel_locker" },
                    "locker_id": { "type": "string" },
                    "carrier": { "type": "string" }
                }
            }),
        );

        register_extended_subtypes(&mut schemas);

        let base = &schemas["FulfillmentDestination"];
        let disc = &base["discriminator"];
        assert_eq!(
            disc["mapping"]["parcel_locker"],
            "#/components/schemas/LockerDestination"
        );
        let one_of = base["oneOf"].as_array().unwrap();
        assert_eq!(one_of.len(), 3);
        assert!(one_of
            .iter()
            .any(|item| item["$ref"] == "#/components/schemas/LockerDestination"));

        let derived = &schemas["LockerDestination"];
        assert!(derived.get("allOf").is_none());
        assert!(derived["properties"].get("id").is_some());
        assert_eq!(derived["properties"]["type"]["default"], "parcel_locker");
        let req = derived["required"].as_array().unwrap();
        assert!(req.iter().any(|r| r == "locker_id"));
        assert!(req.iter().any(|r| r == "id"));
        assert!(req.iter().any(|r| r == "type"));
    }
}
