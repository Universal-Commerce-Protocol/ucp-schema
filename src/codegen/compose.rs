//! Stage 3: Capability, sub-type overlay, and container extension composition.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde_json::{Map, Value};

use crate::codegen::normalizer::{
    normalize_def_schema, qualify_container_op_name, rewrite_refs_to_defs,
};
use crate::codegen::reachability::{is_container_op_key, LoadedSchema};
use crate::codegen::CodegenError;
use crate::resolver::resolve;
use crate::types::{Direction, ResolveOptions};

#[allow(clippy::type_complexity)]
pub(super) fn compose_active_extensions(
    loaded: &[LoadedSchema],
    working_schemas: &BTreeMap<usize, Value>,
    active_cap_indices: &BTreeSet<usize>,
    active_ext_indices: &BTreeSet<usize>,
    pending_overlays: Vec<(String, Value)>,
    root_raw_schemas: &mut BTreeMap<String, (usize, Value)>,
    defs: &mut BTreeMap<String, Value>,
) -> Result<(BTreeMap<String, Value>, Vec<(PathBuf, Value)>), CodegenError> {
    for (target_name, overlay) in pending_overlays {
        if let Some((_, target_schema)) = root_raw_schemas.get_mut(&target_name) {
            merge_extension_into_schema(target_schema, overlay, &target_name);
        }
    }

    for &cap_idx in active_cap_indices {
        let cap_item = &loaded[cap_idx];
        if cap_item.is_container {
            continue;
        }
        let Some(cap_name) = cap_item.name.as_deref() else {
            continue;
        };
        let Some((_, target_schema)) = root_raw_schemas.get_mut(&cap_item.stem_pascal) else {
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
            let mut prepared = deref_local_mixin_refs(ext_block, ext_schema);
            rewrite_refs_to_defs(
                &mut prepared,
                &cap_item.stem_pascal,
                Some(&ext_item.stem_pascal),
            );
            merge_extension_into_schema(target_schema, prepared, &cap_item.stem_pascal);
        }
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
        )?;
        hoist_container_operations(cap_item, &composed, defs)?;
        container_schemas.push((cap_item.path.clone(), composed));
    }

    Ok((capability_resources, container_schemas))
}

fn deref_local_mixin_refs(block: &Value, source_schema: &Value) -> Value {
    let Some(defs_obj) = source_schema.get("$defs").and_then(Value::as_object) else {
        return block.clone();
    };
    let mut current = block
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix("#/$defs/"))
        .and_then(|k| defs_obj.get(k))
        .cloned()
        .unwrap_or_else(|| block.clone());

    if let Some(arr) = current
        .as_object_mut()
        .and_then(|o| o.get_mut("allOf"))
        .and_then(Value::as_array_mut)
    {
        for branch in arr.iter_mut() {
            if let Some(target) = branch
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|r| r.strip_prefix("#/$defs/"))
                .and_then(|k| defs_obj.get(k))
            {
                *branch = target.clone();
            }
        }
    }
    current
}

fn merge_extension_into_schema(target: &mut Value, ext_val: Value, target_name: &str) {
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
        merge_single_branch_into_target(target, &branch);
    }
}

fn merge_single_branch_into_target(target: &mut Value, branch: &Value) {
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
                    merge_extension_property(existing_prop, ext_prop);
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

fn merge_extension_property(base_prop: &mut Value, ext_prop: &Value) {
    let (Some(base_obj), Some(ext_obj)) = (base_prop.as_object_mut(), ext_prop.as_object()) else {
        return;
    };
    for key in ["$ref", "ucp_request", "ucp_response", "description"] {
        if let Some(val) = ext_obj.get(key) {
            base_obj.insert(key.to_string(), val.clone());
        }
    }
    if let Some(ext_items) = ext_obj.get("items").filter(|i| i.get("$ref").is_some()) {
        base_obj.insert("items".to_string(), ext_items.clone());
    }
}

fn compose_container_capability(
    cap_item: &LoadedSchema,
    base_container: &Value,
    active_ext_indices: &BTreeSet<usize>,
    loaded: &[LoadedSchema],
    working_schemas: &BTreeMap<usize, Value>,
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
                let mut derefed = deref_local_mixin_refs(op_ext_val, ext_schema);
                rewrite_refs_to_defs(&mut derefed, &target_op_name, Some(&ext_item.stem_pascal));
                merge_extension_into_schema(target_op_schema, derefed, &target_op_name);
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
            let mut derefed = deref_local_mixin_refs(block, ext_schema);
            rewrite_refs_to_defs(&mut derefed, &target_op_name, Some(&ext_item.stem_pascal));
            merge_extension_into_schema(target_op_schema, derefed, &target_op_name);
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

        let derefed = deref_local_mixin_refs(
            &ext_source["$defs"]["dev.ucp.shopping.checkout"],
            &ext_source,
        );
        merge_extension_into_schema(&mut target, derefed, "Checkout");

        assert_eq!(target["required"], json!(["id", "extra_req"]));
        assert_eq!(
            target["properties"]["products"]["items"]["$ref"],
            "#/$defs/FulfillmentProduct"
        );
        assert_eq!(target["properties"]["extra_req"]["type"], "string");
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

        let composed = compose_container_capability(
            &cap_item,
            &base_container,
            &BTreeSet::from([1, 2]),
            &loaded,
            &working_schemas,
        )
        .unwrap();

        let mut defs = BTreeMap::new();
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
