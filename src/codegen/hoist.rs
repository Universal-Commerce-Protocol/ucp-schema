//! Stage 2: Upfront `$defs` hoisting, role container extraction, and collision qualification.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::codegen::normalizer::{
    has_directional_annotations, is_reverse_domain_name, normalize_def_schema, qualify_def_name,
    ref_to_def_name, rewrite_refs_to_defs, slice_directional_schemas, to_pascal_case,
};
use crate::codegen::reachability::{
    compute_inactive_local_defs, is_container_op_key, normalize_lexical_path, LoadedSchema,
};
use crate::codegen::{has_root_schema_body, CodegenError, SchemaMap};
use crate::loader::{for_each_schema_object, for_each_schema_object_mut};
use crate::resolver::resolve;
use crate::types::{Direction, ResolveOptions};

const ROLE_SCHEMA_KEYS: &[&str] = &["platform_schema", "business_schema", "response_schema"];

enum LocalDefAction {
    Skip,
    RoleContainer,
    Overlay(String),
    Hoist(String),
}

pub(super) fn hoist_defs(
    loaded: &mut [LoadedSchema],
    reachable_indices: &BTreeSet<usize>,
    active_cap_indices: &BTreeSet<usize>,
    active_ext_indices: &BTreeSet<usize>,
    active_capabilities: &BTreeSet<String>,
    defs: &mut SchemaMap,
    sliced_base_names: &mut BTreeSet<String>,
) -> Result<(SchemaMap, Vec<(String, Value)>), CodegenError> {
    let standalone_type_names: BTreeSet<String> = reachable_indices
        .iter()
        .filter_map(|&idx| {
            let item = &loaded[idx];
            (!item.is_container && has_root_schema_body(&item.schema))
                .then(|| item.stem_pascal.clone())
        })
        .collect();

    let inactive_local_defs: BTreeMap<usize, BTreeSet<String>> = reachable_indices
        .iter()
        .filter_map(|&idx| {
            let inactive = compute_inactive_local_defs(&loaded[idx], active_capabilities);
            (!inactive.is_empty()).then_some((idx, inactive))
        })
        .collect();

    let is_active_source =
        |idx: usize| active_ext_indices.contains(&idx) || active_cap_indices.contains(&idx);
    let local_def_renames = compute_collision_renames(
        loaded,
        reachable_indices,
        &inactive_local_defs,
        &standalone_type_names,
        &is_active_source,
    );
    apply_collision_renames_to_schemas(loaded, reachable_indices, &local_def_renames);

    let mut pending_overlays = Vec::new();
    for &idx in reachable_indices {
        let item = &loaded[idx];
        let Some(defs_obj) = item.schema.get("$defs").and_then(Value::as_object) else {
            continue;
        };
        let inactive_set = inactive_local_defs.get(&idx);

        for (def_key, def_val) in defs_obj {
            match classify_local_def(
                item,
                def_key,
                def_val,
                inactive_set,
                &standalone_type_names,
                is_active_source(idx),
            ) {
                LocalDefAction::Skip => continue,
                LocalDefAction::RoleContainer => {
                    hoist_self_named_role_schemas(
                        &item.stem_pascal,
                        def_val,
                        defs,
                        sliced_base_names,
                    )?;
                }
                LocalDefAction::Overlay(target_pascal) => {
                    let mut overlay = def_val.clone();
                    rewrite_refs_to_defs(&mut overlay, &target_pascal, Some(&item.stem_pascal));
                    pending_overlays.push((target_pascal, overlay));
                }
                LocalDefAction::Hoist(default_name) => {
                    let hoisted_name = local_def_renames
                        .get(&(idx, def_key.clone()))
                        .cloned()
                        .unwrap_or(default_name);
                    if item.stem == "ucp" {
                        insert_ucp_def(
                            def_key,
                            def_val,
                            defs_obj.get("base"),
                            &hoisted_name,
                            defs,
                        )?;
                    } else {
                        insert_sliced_or_normalized_def(
                            def_val,
                            &hoisted_name,
                            &item.stem_pascal,
                            false,
                            defs,
                            sliced_base_names,
                        )?;
                    }
                }
            }
        }
    }

    let mut root_raw_schemas = BTreeMap::new();
    for &idx in reachable_indices {
        let item = &loaded[idx];
        if item.is_container {
            continue;
        }
        let mut root_val = item.schema.clone();
        if let Some(obj) = root_val.as_object_mut() {
            obj.remove("$defs");
            obj.remove("definitions");
        }
        if !has_root_schema_body(&root_val) {
            continue;
        }
        rewrite_refs_to_defs(&mut root_val, &item.stem_pascal, Some(&item.stem_pascal));
        root_raw_schemas.insert(item.stem_pascal.clone(), root_val);
    }

    Ok((root_raw_schemas, pending_overlays))
}

fn classify_local_def(
    item: &LoadedSchema,
    def_key: &str,
    def_val: &Value,
    inactive_set: Option<&BTreeSet<String>>,
    standalone_type_names: &BTreeSet<String>,
    is_active_source: bool,
) -> LocalDefAction {
    if is_reverse_domain_name(def_key) {
        return if item.name.as_deref() == Some(def_key) {
            LocalDefAction::RoleContainer
        } else {
            LocalDefAction::Skip
        };
    }
    if inactive_set.is_some_and(|s| s.contains(def_key))
        || (item.is_container && is_container_op_key(def_key))
        || is_pure_reexport_def(&item.stem_pascal, def_key, def_val)
    {
        return LocalDefAction::Skip;
    }
    let target_pascal = to_pascal_case(def_key);
    if is_active_source
        && is_inplace_overlay(
            item,
            def_key,
            &target_pascal,
            def_val,
            standalone_type_names,
        )
    {
        return LocalDefAction::Overlay(target_pascal);
    }
    LocalDefAction::Hoist(qualify_def_name(&item.stem_pascal, def_key))
}

fn is_pure_reexport_def(parent_pascal: &str, def_key: &str, def_val: &Value) -> bool {
    let Some(obj) = def_val.as_object() else {
        return false;
    };
    let Some(Value::String(ref_str)) = obj.get("$ref") else {
        return false;
    };
    !ref_str.contains('#')
        && !["properties", "allOf", "oneOf", "anyOf"]
            .iter()
            .any(|k| obj.contains_key(*k))
        && ref_to_def_name(ref_str, Some(parent_pascal)) == to_pascal_case(def_key)
}

fn is_inplace_overlay(
    item: &LoadedSchema,
    def_key: &str,
    target_pascal: &str,
    def_val: &Value,
    standalone_type_names: &BTreeSet<String>,
) -> bool {
    if !standalone_type_names.contains(target_pascal) {
        return false;
    }
    let Some(obj) = def_val.as_object() else {
        return false;
    };
    let has_allof_ref_to_target =
        obj.get("allOf")
            .and_then(Value::as_array)
            .is_some_and(|arr| {
                arr.iter().any(|branch| {
                    branch.get("$ref").and_then(Value::as_str).is_some_and(|r| {
                        ref_to_def_name(r, Some(&item.stem_pascal)) == target_pascal
                    })
                })
            });
    if has_allof_ref_to_target {
        return true;
    }
    if !item.is_extension
        || !(obj.get("type").and_then(Value::as_str) == Some("object")
            || obj.contains_key("properties"))
    {
        return false;
    }
    let expected_ref = format!("#/$defs/{def_key}");
    item.schema
        .get("$defs")
        .and_then(Value::as_object)
        .is_some_and(|defs| {
            defs.iter()
                .filter(|(k, _)| is_reverse_domain_name(k))
                .any(|(_, v)| {
                    let mut found = false;
                    for_each_schema_object(v, &mut |sub| {
                        if sub.get("$ref").and_then(Value::as_str) == Some(&expected_ref) {
                            found = true;
                        }
                    });
                    found
                })
        })
}

fn compute_collision_renames(
    loaded: &[LoadedSchema],
    reachable_indices: &BTreeSet<usize>,
    inactive_local_defs: &BTreeMap<usize, BTreeSet<String>>,
    standalone_type_names: &BTreeSet<String>,
    is_active_source: &dyn Fn(usize) -> bool,
) -> BTreeMap<(usize, String), String> {
    let mut candidate_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut entries = Vec::new();

    for &idx in reachable_indices {
        let item = &loaded[idx];
        let Some(defs_obj) = item.schema.get("$defs").and_then(Value::as_object) else {
            continue;
        };
        let inactive_set = inactive_local_defs.get(&idx);
        for (def_key, def_val) in defs_obj {
            let LocalDefAction::Hoist(default_name) = classify_local_def(
                item,
                def_key,
                def_val,
                inactive_set,
                standalone_type_names,
                is_active_source(idx),
            ) else {
                continue;
            };
            *candidate_counts.entry(default_name.clone()).or_insert(0) += 1;
            entries.push((idx, def_key.clone(), default_name));
        }
    }

    let mut renames = BTreeMap::new();
    for (idx, def_key, default_name) in entries {
        let count = candidate_counts.get(&default_name).copied().unwrap_or(0);
        if !(standalone_type_names.contains(&default_name) || count > 1) {
            continue;
        }
        let parent_pascal = &loaded[idx].stem_pascal;
        let def_pascal = to_pascal_case(&def_key);
        let qualified = if def_pascal.starts_with(parent_pascal) {
            def_pascal
        } else {
            format!("{parent_pascal}{def_pascal}")
        };
        renames.insert((idx, def_key), qualified);
    }
    renames
}

fn apply_collision_renames_to_schemas(
    loaded: &mut [LoadedSchema],
    reachable_indices: &BTreeSet<usize>,
    local_def_renames: &BTreeMap<(usize, String), String>,
) {
    if local_def_renames.is_empty() {
        return;
    }
    let path_to_idx: BTreeMap<_, usize> = loaded
        .iter()
        .enumerate()
        .map(|(i, item)| (item.path.clone(), i))
        .collect();

    for &idx in reachable_indices {
        let current_dir = loaded[idx].path.parent().map(std::path::Path::to_path_buf);
        for_each_schema_object_mut(&mut loaded[idx].schema, &mut |obj| {
            let Some(ref_str) = obj.get("$ref").and_then(Value::as_str) else {
                return;
            };
            let lookup_key = if let Some(local_key) = ref_str.strip_prefix("#/$defs/") {
                Some((idx, local_key.to_string()))
            } else if let Some((file_part, def_key)) = ref_str.split_once("#/$defs/") {
                current_dir
                    .as_ref()
                    .and_then(|dir| path_to_idx.get(&normalize_lexical_path(&dir.join(file_part))))
                    .map(|&target_idx| (target_idx, def_key.to_string()))
            } else {
                None
            };
            if let Some(qualified) = lookup_key.and_then(|k| local_def_renames.get(&k)) {
                obj.insert(
                    "$ref".to_string(),
                    Value::String(format!("#/$defs/{qualified}")),
                );
            }
        });
    }
}

fn hoist_self_named_role_schemas(
    stem_pascal: &str,
    role_container: &Value,
    defs: &mut BTreeMap<String, Value>,
    sliced_base_names: &mut BTreeSet<String>,
) -> Result<(), CodegenError> {
    let Some(obj) = role_container.as_object() else {
        return Ok(());
    };
    for &role_key in ROLE_SCHEMA_KEYS {
        let Some(role_val) = obj.get(role_key).filter(|v| v.is_object()) else {
            continue;
        };
        let hoisted_name = qualify_def_name(stem_pascal, role_key);
        insert_sliced_or_normalized_def(
            role_val,
            &hoisted_name,
            stem_pascal,
            false,
            defs,
            sliced_base_names,
        )?;
    }
    Ok(())
}

pub(super) fn insert_sliced_or_normalized_def(
    raw_val: &Value,
    hoisted_name: &str,
    parent_pascal: &str,
    force_slice: bool,
    defs: &mut BTreeMap<String, Value>,
    sliced_base_names: &mut BTreeSet<String>,
) -> Result<(), CodegenError> {
    if force_slice || has_directional_annotations(raw_val) {
        sliced_base_names.insert(hoisted_name.to_string());
        let mut prepared = raw_val.clone();
        rewrite_refs_to_defs(&mut prepared, hoisted_name, Some(parent_pascal));
        for (slice_name, slice_val) in slice_directional_schemas(&prepared, hoisted_name)? {
            defs.insert(slice_name, slice_val);
        }
    } else {
        defs.insert(
            hoisted_name.to_string(),
            normalize_def_schema(raw_val, hoisted_name, Some(parent_pascal)),
        );
    }
    Ok(())
}

fn insert_ucp_def(
    def_key: &str,
    raw_val: &Value,
    raw_ucp_base: Option<&Value>,
    hoisted_name: &str,
    defs: &mut BTreeMap<String, Value>,
) -> Result<(), CodegenError> {
    let mut prepared = raw_val.clone();
    let is_request_envelope = def_key.starts_with("request_");
    let is_response_envelope = def_key.starts_with("response_");

    if (is_request_envelope || is_response_envelope) && def_key != "base" {
        if let Some(base_val) = raw_ucp_base.and_then(Value::as_object) {
            for_each_schema_object_mut(&mut prepared, &mut |obj| {
                let is_base_ref = obj
                    .get("$ref")
                    .and_then(Value::as_str)
                    .is_some_and(|r| r == "#/$defs/base" || r == "#/$defs/UcpBase");
                if !is_base_ref {
                    return;
                }
                obj.remove("$ref");
                for (k, v) in base_val {
                    obj.entry(k.clone()).or_insert_with(|| v.clone());
                }
            });
        }
    }

    if is_request_envelope || is_response_envelope || has_directional_annotations(&prepared) {
        let opts = if is_request_envelope {
            ResolveOptions::new(Direction::Request, "create")
        } else {
            ResolveOptions::new(Direction::Response, "read")
        };
        prepared = resolve(&prepared, &opts)?;
    }

    defs.insert(
        hoisted_name.to_string(),
        normalize_def_schema(&prepared, hoisted_name, Some("Ucp")),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hoist_defs_qualifies_colliding_local_defs_and_skips_pure_reexports() {
        let mut loaded = vec![
            LoadedSchema::from_file(
                "/schemas/alpha.json",
                json!({
                    "name": "dev.ucp.shopping.alpha",
                    "type": "object",
                    "properties": {
                        "detail": { "$ref": "#/$defs/shared_detail" },
                        "item": { "$ref": "#/$defs/line_item" }
                    },
                    "$defs": {
                        "shared_detail": { "type": "object", "properties": { "a": { "type": "string" } } },
                        "line_item": { "$ref": "types/line_item.json" }
                    }
                }),
            ),
            LoadedSchema::from_file(
                "/schemas/beta.json",
                json!({
                    "name": "dev.ucp.shopping.beta",
                    "type": "object",
                    "properties": {
                        "detail": { "$ref": "#/$defs/shared_detail" },
                        "alpha_detail": { "$ref": "alpha.json#/$defs/shared_detail" }
                    },
                    "$defs": {
                        "shared_detail": { "type": "object", "properties": { "b": { "type": "number" } } },
                        "dev.ucp.shopping.beta": {
                            "platform_schema": {
                                "type": "object",
                                "properties": { "endpoint": { "type": "string" } }
                            }
                        }
                    }
                }),
            ),
            LoadedSchema::from_file(
                "/schemas/types/line_item.json",
                json!({
                    "type": "object",
                    "properties": { "id": { "type": "string" } }
                }),
            ),
        ];

        let reachable = BTreeSet::from([0, 1, 2]);
        let active_caps = BTreeSet::from([0, 1]);
        let active_exts = BTreeSet::new();
        let active_names = BTreeSet::from(["alpha".to_string(), "beta".to_string()]);
        let mut defs = BTreeMap::new();
        let mut sliced_base_names = BTreeSet::new();

        let (root_raw, overlays) = hoist_defs(
            &mut loaded,
            &reachable,
            &active_caps,
            &active_exts,
            &active_names,
            &mut defs,
            &mut sliced_base_names,
        )
        .unwrap();

        assert!(overlays.is_empty());
        assert!(defs.contains_key("AlphaSharedDetail"));
        assert!(defs.contains_key("BetaSharedDetail"));
        assert!(defs.contains_key("BetaPlatformSchema"));
        assert!(!defs.contains_key("LineItem"));

        assert_eq!(
            root_raw["Alpha"]["properties"]["detail"]["$ref"],
            "#/$defs/AlphaSharedDetail"
        );
        assert_eq!(
            root_raw["Beta"]["properties"]["detail"]["$ref"],
            "#/$defs/BetaSharedDetail"
        );
        assert_eq!(
            root_raw["Beta"]["properties"]["alpha_detail"]["$ref"],
            "#/$defs/AlphaSharedDetail"
        );
    }

    #[test]
    fn hoist_defs_resolves_ucp_envelopes_without_create_or_update_suffixes() {
        let mut loaded = vec![LoadedSchema::from_file(
            "/schemas/ucp.json",
            json!({
                "$defs": {
                    "base": {
                        "type": "object",
                        "required": ["version"],
                        "properties": {
                            "version": { "type": "string" },
                            "map_order": { "type": "object", "ucp_request": "omit" }
                        }
                    },
                    "request_checkout_schema": {
                        "allOf": [
                            { "$ref": "#/$defs/base" },
                            { "type": "object", "properties": { "capabilities": { "type": "object" } } }
                        ]
                    },
                    "response_checkout_schema": {
                        "allOf": [
                            { "$ref": "#/$defs/base" },
                            {
                                "type": "object",
                                "required": ["payment_handlers"],
                                "properties": { "payment_handlers": { "type": "object" } }
                            }
                        ]
                    }
                }
            }),
        )];

        let reachable = BTreeSet::from([0]);
        let mut defs = BTreeMap::new();
        let mut sliced_base_names = BTreeSet::new();

        hoist_defs(
            &mut loaded,
            &reachable,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut defs,
            &mut sliced_base_names,
        )
        .unwrap();

        assert!(defs.contains_key("UcpBase"));
        assert!(!defs.contains_key("UcpBaseCreateRequest"));
        assert!(!defs.contains_key("UcpBaseUpdateRequest"));
        assert!(!sliced_base_names.contains("UcpBase"));

        let req_base = &defs["RequestCheckoutSchema"]["allOf"][0];
        assert!(req_base["properties"].get("version").is_some());
        assert!(req_base["properties"].get("map_order").is_none());

        let resp_base = &defs["ResponseCheckoutSchema"]["allOf"][0];
        assert!(resp_base["properties"].get("version").is_some());
        assert!(resp_base["properties"].get("map_order").is_some());
    }
}
