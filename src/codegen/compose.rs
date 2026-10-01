//! Stage 3: Capability, sub-type overlay, and container extension composition.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde_json::{Map, Value};

use crate::codegen::normalizer::{
    normalize_def_schema, qualify_container_op_name, qualify_def_name, rewrite_refs_to_defs,
};
use crate::codegen::reachability::{is_container_op_key, LoadedSchema};
use crate::codegen::CodegenError;
use crate::resolver::resolve;
use crate::types::{Direction, ResolveOptions};

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
pub(super) fn compose_active_extensions(
    loaded: &[LoadedSchema],
    working_schemas: &BTreeMap<usize, Value>,
    active_cap_indices: &BTreeSet<usize>,
    active_ext_indices: &BTreeSet<usize>,
    pending_overlays: Vec<(String, Value)>,
    root_raw_schemas: &mut BTreeMap<String, (usize, Value)>,
    defs: &mut BTreeMap<String, Value>,
    inlined_mixin_defs: &mut BTreeSet<String>,
) -> Result<(BTreeMap<String, Value>, Vec<(PathBuf, Value)>), CodegenError> {
    for (target_name, overlay) in pending_overlays {
        let Some((target_idx, mut target_schema)) = root_raw_schemas.remove(&target_name) else {
            continue;
        };
        merge_extension_into_schema(
            &mut target_schema,
            overlay,
            &target_name,
            root_raw_schemas,
            defs,
        );
        root_raw_schemas.insert(target_name, (target_idx, target_schema));
    }

    for &cap_idx in active_cap_indices {
        let cap_item = &loaded[cap_idx];
        if cap_item.is_container {
            continue;
        }
        let Some(cap_name) = cap_item.name.as_deref() else {
            continue;
        };
        let Some((target_idx, mut target_schema)) = root_raw_schemas.remove(&cap_item.stem_pascal)
        else {
            continue;
        };

        for &ext_idx in active_ext_indices {
            let ext_item = &loaded[ext_idx];
            let Some(ext_schema) = working_schemas.get(&ext_idx) else {
                continue;
            };
            let Some(ext_block) = ext_schema
                .get("$defs")
                .and_then(Value::as_object)
                .and_then(|d| d.get(cap_name))
            else {
                continue;
            };
            let mut prepared = deref_local_mixin_refs(
                ext_block,
                ext_schema,
                &ext_item.stem_pascal,
                inlined_mixin_defs,
            );
            rewrite_refs_to_defs(
                &mut prepared,
                &cap_item.stem_pascal,
                Some(&ext_item.stem_pascal),
            );
            merge_extension_into_schema(
                &mut target_schema,
                prepared,
                &cap_item.stem_pascal,
                root_raw_schemas,
                defs,
            );
        }

        root_raw_schemas.insert(cap_item.stem_pascal.clone(), (target_idx, target_schema));
    }

    let capability_resources = active_cap_indices
        .iter()
        .filter_map(|&idx| {
            let name = &loaded[idx].stem_pascal;
            root_raw_schemas
                .get(name)
                .map(|(_, val)| (name.clone(), val.clone()))
        })
        .collect();

    let mut container_schemas = Vec::new();
    for &cap_idx in active_cap_indices {
        let cap_item = &loaded[cap_idx];
        if !cap_item.is_container {
            continue;
        }
        let Some(base_container) = working_schemas.get(&cap_idx) else {
            continue;
        };
        let composed = compose_container_capability(
            cap_item,
            base_container,
            active_ext_indices,
            loaded,
            working_schemas,
            root_raw_schemas,
            defs,
            inlined_mixin_defs,
        )?;
        hoist_container_operations(cap_item, &composed, defs)?;
        container_schemas.push((cap_item.path.clone(), composed));
    }

    Ok((capability_resources, container_schemas))
}

fn deref_local_mixin_refs(
    block: &Value,
    source_schema: &Value,
    parent_pascal: &str,
    inlined_mixin_defs: &mut BTreeSet<String>,
) -> Value {
    let Some(defs_obj) = source_schema.get("$defs").and_then(Value::as_object) else {
        return block.clone();
    };
    let mut current = if let Some((k, target)) = block
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix("#/$defs/"))
        .and_then(|k| defs_obj.get(k).map(|v| (k, v)))
    {
        inlined_mixin_defs.insert(qualify_def_name(parent_pascal, k));
        target.clone()
    } else {
        block.clone()
    };

    if let Some(arr) = current
        .as_object_mut()
        .and_then(|o| o.get_mut("allOf"))
        .and_then(Value::as_array_mut)
    {
        for branch in arr.iter_mut() {
            if let Some((k, target)) = branch
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|r| r.strip_prefix("#/$defs/"))
                .and_then(|k| defs_obj.get(k).map(|v| (k, v)))
            {
                inlined_mixin_defs.insert(qualify_def_name(parent_pascal, k));
                *branch = target.clone();
            }
        }
    }
    current
}

fn merge_extension_into_schema(
    target: &mut Value,
    ext_val: Value,
    target_name: &str,
    root_raw_schemas: &mut BTreeMap<String, (usize, Value)>,
    defs: &mut BTreeMap<String, Value>,
) {
    let Some(obj) = ext_val.as_object() else {
        return;
    };
    let self_ref = format!("#/$defs/{target_name}");
    let mut branches = Vec::new();

    if let Some(Value::Array(all_of)) = obj.get("allOf") {
        branches.extend(
            all_of
                .iter()
                .filter(|item| item.get("$ref").and_then(Value::as_str) != Some(&self_ref))
                .cloned(),
        );
    }
    if obj.contains_key("properties") || obj.contains_key("required") {
        let mut rest = obj.clone();
        rest.remove("allOf");
        rest.remove("title");
        rest.remove("description");
        branches.push(Value::Object(rest));
    }

    for branch in branches {
        merge_single_branch_into_target(target, &branch, root_raw_schemas, defs);
    }
}

fn merge_single_branch_into_target(
    target: &mut Value,
    branch: &Value,
    root_raw_schemas: &mut BTreeMap<String, (usize, Value)>,
    defs: &mut BTreeMap<String, Value>,
) {
    let (Some(target_obj), Some(branch_obj)) = (target.as_object_mut(), branch.as_object()) else {
        return;
    };

    if let Some(ext_props) = branch_obj.get("properties").and_then(Value::as_object) {
        if let Some(target_props_map) = target_obj
            .entry("properties".to_string())
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
        {
            for (prop_name, ext_prop) in ext_props {
                if let Some(existing_prop) = target_props_map.get_mut(prop_name) {
                    merge_extension_property(existing_prop, ext_prop, root_raw_schemas, defs);
                } else {
                    target_props_map.insert(prop_name.clone(), ext_prop.clone());
                }
            }
        }
    }

    let Some(ext_reqs) = branch_obj.get("required").and_then(Value::as_array) else {
        return;
    };
    if let Some(req_arr) = target_obj
        .entry("required".to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
    {
        for req in ext_reqs {
            if !req_arr.contains(req) {
                req_arr.push(req.clone());
            }
        }
    }
}

fn merge_extension_property(
    base_prop: &mut Value,
    ext_prop: &Value,
    root_raw_schemas: &mut BTreeMap<String, (usize, Value)>,
    defs: &mut BTreeMap<String, Value>,
) {
    let (Some(base_obj), Some(ext_obj)) = (base_prop.as_object_mut(), ext_prop.as_object()) else {
        return;
    };
    for key in ["$ref", "ucp_request", "ucp_response", "description"] {
        if let Some(val) = ext_obj.get(key) {
            base_obj.insert(key.to_string(), val.clone());
        }
    }
    if let Some(ext_items) = ext_obj.get("items") {
        merge_extension_items(base_obj, ext_items, root_raw_schemas, defs);
    }

    let has_ext_props = ext_obj
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|m| !m.is_empty());
    let has_ext_reqs = ext_obj
        .get("required")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty());
    if !(has_ext_props || has_ext_reqs) {
        return;
    }

    if !base_obj.contains_key("properties") && !ext_obj.contains_key("$ref") {
        if let Some(target_def) = base_obj
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix("#/$defs/"))
            .map(str::to_string)
        {
            let mut overlay = Map::new();
            if let Some(props) = ext_obj.get("properties") {
                overlay.insert("properties".to_string(), props.clone());
            }
            if let Some(reqs) = ext_obj.get("required") {
                overlay.insert("required".to_string(), reqs.clone());
            }
            let overlay_val = Value::Object(overlay);
            if let Some((idx, mut target_schema)) = root_raw_schemas.remove(&target_def) {
                merge_single_branch_into_target(
                    &mut target_schema,
                    &overlay_val,
                    root_raw_schemas,
                    defs,
                );
                root_raw_schemas.insert(target_def, (idx, target_schema));
                return;
            }
            if let Some(mut target_schema) = defs.remove(&target_def) {
                merge_single_branch_into_target(
                    &mut target_schema,
                    &overlay_val,
                    root_raw_schemas,
                    defs,
                );
                defs.insert(target_def, target_schema);
                return;
            }
        }
    }

    let Some(Value::Object(ext_sub)) = ext_obj.get("properties") else {
        return;
    };
    let Some(base_sub) = base_obj
        .entry("properties".to_string())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
    else {
        return;
    };
    for (k, v) in ext_sub {
        if let Some(existing) = base_sub.get_mut(k) {
            merge_extension_property(existing, v, root_raw_schemas, defs);
        } else {
            base_sub.insert(k.clone(), v.clone());
        }
    }
}

fn merge_extension_items(
    base_obj: &mut Map<String, Value>,
    ext_items: &Value,
    root_raw_schemas: &mut BTreeMap<String, (usize, Value)>,
    defs: &mut BTreeMap<String, Value>,
) {
    if let Some(ext_ref_str) = ext_items.get("$ref").and_then(Value::as_str) {
        let keep_base_ref = base_obj
            .get("items")
            .and_then(|i| i.get("$ref"))
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix("#/$defs/"))
            .zip(ext_ref_str.strip_prefix("#/$defs/"))
            .is_some_and(|(base_def, ext_def)| {
                def_extends_target(base_def, ext_def, root_raw_schemas, defs)
            });
        if !keep_base_ref {
            base_obj.insert("items".to_string(), ext_items.clone());
        }
        return;
    }

    let Some(ext_items_obj) = ext_items.as_object() else {
        return;
    };
    let cond_branches: Vec<Value> = if let Some(Value::Array(arr)) = ext_items_obj.get("allOf") {
        arr.clone()
    } else if ext_items_obj.contains_key("if") || ext_items_obj.contains_key("then") {
        vec![ext_items.clone()]
    } else {
        return;
    };

    if let Some(item_def) = base_obj
        .get("items")
        .and_then(|i| i.get("$ref"))
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix("#/$defs/"))
        .map(str::to_string)
    {
        if let Some((_, target_schema)) = root_raw_schemas.get_mut(&item_def) {
            append_unique_allof_branches(target_schema, cond_branches);
            return;
        }
        if let Some(target_schema) = defs.get_mut(&item_def) {
            append_unique_allof_branches(target_schema, cond_branches);
            return;
        }
    }

    if let Some(base_items) = base_obj.get_mut("items") {
        append_unique_allof_branches(base_items, cond_branches);
    }
}

fn def_extends_target(
    base_def: &str,
    ext_def: &str,
    root_raw_schemas: &BTreeMap<String, (usize, Value)>,
    defs: &BTreeMap<String, Value>,
) -> bool {
    if base_def == ext_def {
        return true;
    }
    let expected_ref = format!("#/$defs/{ext_def}");
    let schema = defs
        .get(base_def)
        .or_else(|| root_raw_schemas.get(base_def).map(|(_, s)| s));
    let Some(all_of) = schema
        .and_then(|s| s.get("allOf"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    all_of
        .iter()
        .any(|b| b.get("$ref").and_then(Value::as_str) == Some(&expected_ref))
}

fn append_unique_allof_branches(target: &mut Value, branches: Vec<Value>) {
    let Some(target_obj) = target.as_object_mut() else {
        return;
    };
    let Some(all_of) = target_obj
        .entry("allOf".to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
    else {
        return;
    };
    for branch in branches {
        if !all_of.contains(&branch) {
            all_of.push(branch);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn compose_container_capability(
    cap_item: &LoadedSchema,
    base_container: &Value,
    active_ext_indices: &BTreeSet<usize>,
    loaded: &[LoadedSchema],
    working_schemas: &BTreeMap<usize, Value>,
    root_raw_schemas: &mut BTreeMap<String, (usize, Value)>,
    defs: &mut BTreeMap<String, Value>,
    inlined_mixin_defs: &mut BTreeSet<String>,
) -> Result<Value, CodegenError> {
    let mut composed = base_container.clone();
    let Some(cap_name) = cap_item.name.as_deref() else {
        return Ok(composed);
    };
    let Some(container_defs) = composed
        .as_object_mut()
        .and_then(|o| o.get_mut("$defs"))
        .and_then(Value::as_object_mut)
    else {
        return Ok(composed);
    };

    for &ext_idx in active_ext_indices {
        let ext_item = &loaded[ext_idx];
        let Some(ext_schema) = working_schemas.get(&ext_idx) else {
            continue;
        };
        let Some(ext_cap_block) = ext_schema
            .get("$defs")
            .and_then(Value::as_object)
            .and_then(|d| d.get(cap_name))
        else {
            continue;
        };

        if let Some(nested_defs) = ext_cap_block.get("$defs").and_then(Value::as_object) {
            for (op_key, op_ext_val) in nested_defs {
                let Some(target_op_schema) = container_defs.get_mut(op_key) else {
                    continue;
                };
                let target_op_name = qualify_container_op_name(&cap_item.stem_pascal, op_key);
                let mut derefed = deref_local_mixin_refs(
                    op_ext_val,
                    ext_schema,
                    &ext_item.stem_pascal,
                    inlined_mixin_defs,
                );
                rewrite_refs_to_defs(&mut derefed, &target_op_name, Some(&ext_item.stem_pascal));
                merge_extension_into_schema(
                    target_op_schema,
                    derefed,
                    &target_op_name,
                    root_raw_schemas,
                    defs,
                );
            }
            continue;
        }

        let candidate_blocks: Vec<&Value> =
            if let Some(one_of) = ext_cap_block.get("oneOf").and_then(Value::as_array) {
                one_of.iter().collect()
            } else {
                vec![ext_cap_block]
            };

        for block in candidate_blocks {
            let Some(op_key) = find_referenced_container_op(block, container_defs) else {
                continue;
            };
            let Some(target_op_schema) = container_defs.get_mut(&op_key) else {
                continue;
            };
            let target_op_name = qualify_container_op_name(&cap_item.stem_pascal, &op_key);
            let mut derefed = deref_local_mixin_refs(
                block,
                ext_schema,
                &ext_item.stem_pascal,
                inlined_mixin_defs,
            );
            rewrite_refs_to_defs(&mut derefed, &target_op_name, Some(&ext_item.stem_pascal));
            merge_extension_into_schema(
                target_op_schema,
                derefed,
                &target_op_name,
                root_raw_schemas,
                defs,
            );
        }
    }

    Ok(composed)
}

fn find_referenced_container_op(
    block: &Value,
    container_defs: &Map<String, Value>,
) -> Option<String> {
    let all_of = block.get("allOf").and_then(Value::as_array)?;
    all_of.iter().find_map(|branch| {
        let ref_str = branch.get("$ref").and_then(Value::as_str)?;
        let (_, def_key) = ref_str.split_once("#/$defs/")?;
        container_defs
            .contains_key(def_key)
            .then(|| def_key.to_string())
    })
}

fn hoist_container_operations(
    cap_item: &LoadedSchema,
    composed_container: &Value,
    defs: &mut BTreeMap<String, Value>,
) -> Result<(), CodegenError> {
    let Some(container_defs) = composed_container.get("$defs").and_then(Value::as_object) else {
        return Ok(());
    };

    for (op_key, op_schema) in container_defs {
        if !is_container_op_key(op_key) {
            continue;
        }
        let hoisted_name = qualify_container_op_name(&cap_item.stem_pascal, op_key);
        let opts = match op_key.strip_suffix("_request") {
            Some(op_prefix) => ResolveOptions::new(Direction::Request, op_prefix),
            None => ResolveOptions::new(Direction::Response, "read"),
        };
        let resolved = resolve(op_schema, &opts)?;
        defs.insert(
            hoisted_name.clone(),
            normalize_def_schema(&resolved, &hoisted_name, Some(&cap_item.stem_pascal)),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen::normalizer::to_pascal_case;
    use serde_json::json;

    #[test]
    fn merge_extension_into_schema_merges_properties_items_and_required_without_duplicates() {
        let mut target = json!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": { "type": "string" },
                "products": {
                    "type": "array",
                    "items": { "$ref": "#/$defs/Product" }
                }
            }
        });

        let ext_source = json!({
            "$defs": {
                "mixin_props": {
                    "type": "object",
                    "required": ["id", "extra_req"],
                    "properties": {
                        "products": {
                            "type": "array",
                            "items": { "$ref": "#/$defs/FulfillmentProduct" }
                        },
                        "extra_req": { "type": "string" }
                    }
                },
                "dev.ucp.shopping.checkout": {
                    "allOf": [
                        { "$ref": "#/$defs/Checkout" },
                        { "$ref": "#/$defs/mixin_props" }
                    ]
                }
            }
        });

        let mut inlined_mixin_defs = BTreeSet::new();
        let derefed = deref_local_mixin_refs(
            &ext_source["$defs"]["dev.ucp.shopping.checkout"],
            &ext_source,
            "Fulfillment",
            &mut inlined_mixin_defs,
        );
        assert!(inlined_mixin_defs.contains("MixinProps"));
        let mut root_raw_schemas = BTreeMap::new();
        let mut defs = BTreeMap::new();
        merge_extension_into_schema(
            &mut target,
            derefed,
            "Checkout",
            &mut root_raw_schemas,
            &mut defs,
        );

        assert_eq!(target["required"], json!(["id", "extra_req"]));
        assert_eq!(
            target["properties"]["products"]["items"]["$ref"],
            "#/$defs/FulfillmentProduct"
        );
        assert_eq!(target["properties"]["extra_req"]["type"], "string");
    }

    #[test]
    fn merge_extension_redirects_ref_property_constraints_and_conditional_items_to_target_defs() {
        let mut booking = json!({
            "type": "object",
            "properties": {
                "actions": { "$ref": "#/$defs/Actions" },
                "payment": { "$ref": "#/$defs/Payment" },
                "policies": {
                    "type": "array",
                    "items": { "$ref": "#/$defs/Policy" }
                }
            }
        });
        let mut root_raw_schemas = BTreeMap::from([
            (
                "Actions".to_string(),
                (
                    1,
                    json!({
                        "type": "object",
                        "propertyNames": { "$ref": "#/$defs/ReverseDomainName" }
                    }),
                ),
            ),
            (
                "Payment".to_string(),
                (
                    2,
                    json!({
                        "type": "object",
                        "properties": {
                            "instruments": {
                                "type": "array",
                                "items": { "$ref": "#/$defs/SelectedPaymentInstrument" }
                            }
                        }
                    }),
                ),
            ),
            ("Policy".to_string(), (3, json!({ "type": "object" }))),
        ]);
        let mut defs = BTreeMap::from([(
            "SelectedPaymentInstrument".to_string(),
            json!({
                "allOf": [{ "$ref": "#/$defs/PaymentInstrument" }]
            }),
        )]);

        let ext = json!({
            "type": "object",
            "properties": {
                "actions": {
                    "ucp_request": "omit",
                    "properties": {
                        "dev.ucp.common.payment.three_ds_challenge": { "type": "array" }
                    }
                },
                "payment": {
                    "type": "object",
                    "properties": {
                        "instruments": {
                            "type": "array",
                            "items": { "$ref": "#/$defs/PaymentInstrument" },
                            "ucp_request": { "complete": "required" }
                        }
                    }
                },
                "policies": {
                    "type": "array",
                    "items": {
                        "if": { "properties": { "type": { "const": "dev.ucp.lodging.policy.cancellation" } } },
                        "then": { "$ref": "#/$defs/CancellationItem" }
                    }
                }
            }
        });

        merge_extension_into_schema(
            &mut booking,
            ext,
            "Booking",
            &mut root_raw_schemas,
            &mut defs,
        );

        assert!(booking["properties"]["actions"].get("properties").is_none());
        assert_eq!(booking["properties"]["actions"]["ucp_request"], "omit");
        assert!(root_raw_schemas["Actions"].1["properties"]
            .get("dev.ucp.common.payment.three_ds_challenge")
            .is_some());

        assert!(booking["properties"]["payment"].get("properties").is_none());
        assert_eq!(
            root_raw_schemas["Payment"].1["properties"]["instruments"]["items"]["$ref"],
            "#/$defs/SelectedPaymentInstrument"
        );
        assert_eq!(
            root_raw_schemas["Payment"].1["properties"]["instruments"]["ucp_request"]["complete"],
            "required"
        );

        assert_eq!(
            booking["properties"]["policies"]["items"],
            json!({ "$ref": "#/$defs/Policy" })
        );
        assert_eq!(
            root_raw_schemas["Policy"].1["allOf"][0]["then"]["$ref"],
            "#/$defs/CancellationItem"
        );
    }

    #[test]
    fn compose_container_capability_supports_nested_defs_and_oneof_allof_extensions() {
        let cap_item = LoadedSchema {
            path: PathBuf::from("/schemas/shopping/catalog_lookup.json"),
            stem: "catalog_lookup".to_string(),
            stem_pascal: to_pascal_case("catalog_lookup"),
            name: Some("dev.ucp.shopping.catalog.lookup".to_string()),
            is_extension: false,
            is_container: true,
            is_capability: true,
            in_types_dir: false,
            schema: json!({}),
        };

        let base_container = json!({
            "name": "dev.ucp.shopping.catalog.lookup",
            "$defs": {
                "lookup_request": {
                    "type": "object",
                    "properties": { "ids": { "type": "array" } }
                },
                "lookup_response": {
                    "type": "object",
                    "properties": { "products": { "type": "array" } }
                }
            }
        });

        let ext_nested = LoadedSchema {
            path: PathBuf::from("/schemas/shopping/fulfillment.json"),
            stem: "fulfillment".to_string(),
            stem_pascal: "Fulfillment".to_string(),
            name: Some("dev.ucp.shopping.fulfillment".to_string()),
            is_extension: true,
            is_container: false,
            is_capability: false,
            in_types_dir: false,
            schema: json!({}),
        };
        let ext_oneof = LoadedSchema {
            path: PathBuf::from("/schemas/common/loyalty.json"),
            stem: "loyalty".to_string(),
            stem_pascal: "Loyalty".to_string(),
            name: Some("dev.ucp.common.loyalty".to_string()),
            is_extension: true,
            is_container: false,
            is_capability: false,
            in_types_dir: false,
            schema: json!({}),
        };

        let loaded = vec![cap_item.clone(), ext_nested, ext_oneof];
        let mut working_schemas = BTreeMap::new();
        working_schemas.insert(
            1,
            json!({
                "$defs": {
                    "req_mixin": {
                        "type": "object",
                        "properties": { "context": { "$ref": "types/context.json" } }
                    },
                    "dev.ucp.shopping.catalog.lookup": {
                        "$defs": {
                            "lookup_request": { "$ref": "#/$defs/req_mixin" }
                        }
                    }
                }
            }),
        );
        working_schemas.insert(
            2,
            json!({
                "$defs": {
                    "dev.ucp.shopping.catalog.lookup": {
                        "oneOf": [
                            {
                                "allOf": [
                                    { "$ref": "catalog_lookup.json#/$defs/lookup_response" },
                                    {
                                        "type": "object",
                                        "properties": {
                                            "loyalty": { "$ref": "types/loyalty.json" }
                                        }
                                    }
                                ]
                            }
                        ]
                    }
                }
            }),
        );

        let mut root_raw_schemas = BTreeMap::new();
        let mut defs = BTreeMap::new();
        let mut inlined_mixin_defs = BTreeSet::new();
        let composed = compose_container_capability(
            &cap_item,
            &base_container,
            &BTreeSet::from([1, 2]),
            &loaded,
            &working_schemas,
            &mut root_raw_schemas,
            &mut defs,
            &mut inlined_mixin_defs,
        )
        .unwrap();

        assert!(inlined_mixin_defs.contains("ReqMixin"));
        hoist_container_operations(&cap_item, &composed, &mut defs).unwrap();

        assert_eq!(
            defs["CatalogLookupRequest"]["properties"]["context"]["$ref"],
            "#/$defs/Context"
        );
        assert_eq!(
            defs["CatalogLookupResponse"]["properties"]["loyalty"]["$ref"],
            "#/$defs/Loyalty"
        );
    }
}
