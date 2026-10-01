//! Stage 2: Upfront `$defs` hoisting, role container extraction, and collision qualification.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::codegen::normalizer::{
    has_directional_annotations, is_reverse_domain_name, normalize_def_schema, qualify_def_name,
    ref_to_def_name, rewrite_refs_to_defs, slice_directional_schemas, to_pascal_case,
};
use crate::codegen::reachability::{
    compute_inactive_local_defs, has_root_schema_body, is_container_op_key, normalize_lexical_path,
    LoadedSchema,
};
use crate::codegen::CodegenError;
use crate::loader::for_each_schema_object_mut;

const ROLE_SCHEMA_KEYS: &[&str] = &["platform_schema", "business_schema", "response_schema"];

enum LocalDefAction {
    Skip,
    RoleContainer,
    Overlay(String),
    Hoist(String),
}

#[allow(clippy::type_complexity)]
pub(super) fn hoist_defs(
    loaded: &[LoadedSchema],
    reachable_indices: &BTreeSet<usize>,
    active_cap_indices: &BTreeSet<usize>,
    active_ext_indices: &BTreeSet<usize>,
    active_capabilities: &BTreeSet<String>,
    defs: &mut BTreeMap<String, Value>,
) -> Result<
    (
        BTreeMap<usize, Value>,
        BTreeMap<String, (usize, Value)>,
        Vec<(String, Value)>,
    ),
    CodegenError,
> {
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

    let mut working_schemas: BTreeMap<usize, Value> = reachable_indices
        .iter()
        .map(|&idx| (idx, loaded[idx].schema.clone()))
        .collect();
    apply_collision_renames_to_schemas(loaded, &mut working_schemas, &local_def_renames);

    let mut pending_overlays = Vec::new();
    for &idx in reachable_indices {
        let item = &loaded[idx];
        let Some(defs_obj) = working_schemas
            .get(&idx)
            .and_then(|s| s.get("$defs"))
            .and_then(Value::as_object)
        else {
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
                    hoist_self_named_role_schemas(&item.stem_pascal, def_val, defs)?;
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
                    insert_sliced_or_normalized_def(
                        def_val,
                        &hoisted_name,
                        &item.stem_pascal,
                        defs,
                    )?;
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
        let Some(mut root_val) = working_schemas.get(&idx).cloned() else {
            continue;
        };
        if let Some(obj) = root_val.as_object_mut() {
            obj.remove("$defs");
            obj.remove("definitions");
        }
        if !has_root_schema_body(&root_val) {
            continue;
        }
        rewrite_refs_to_defs(&mut root_val, &item.stem_pascal, Some(&item.stem_pascal));
        root_raw_schemas.insert(item.stem_pascal.clone(), (idx, root_val));
    }

    Ok((working_schemas, root_raw_schemas, pending_overlays))
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
        if item.name.as_deref() == Some(def_key) {
            return LocalDefAction::RoleContainer;
        }
        return LocalDefAction::Skip;
    }
    if inactive_set.is_some_and(|s| s.contains(def_key))
        || (item.is_container && is_container_op_key(def_key))
        || is_pure_reexport_def(&item.stem_pascal, def_key, def_val)
    {
        return LocalDefAction::Skip;
    }
    let target_pascal = to_pascal_case(def_key);
    if is_active_source && is_inplace_overlay(item, &target_pascal, def_val, standalone_type_names)
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
    has_allof_ref_to_target
        || (item.is_extension
            && (obj.get("type").and_then(Value::as_str) == Some("object")
                || obj.contains_key("properties")))
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
        renames.insert(
            (idx, def_key.clone()),
            format!("{parent_pascal}{}", to_pascal_case(&def_key)),
        );
    }
    renames
}

fn apply_collision_renames_to_schemas(
    loaded: &[LoadedSchema],
    working_schemas: &mut BTreeMap<usize, Value>,
    local_def_renames: &BTreeMap<(usize, String), String>,
) {
    if local_def_renames.is_empty() {
        return;
    }

    for (&idx, schema_val) in working_schemas.iter_mut() {
        let current_dir = loaded[idx].path.parent();
        for_each_schema_object_mut(schema_val, &mut |obj| {
            let Some(Value::String(ref_str)) = obj.get("$ref") else {
                return;
            };
            if let Some(local_key) = ref_str.strip_prefix("#/$defs/") {
                if let Some(qualified) = local_def_renames.get(&(idx, local_key.to_string())) {
                    obj.insert(
                        "$ref".to_string(),
                        Value::String(format!("#/$defs/{qualified}")),
                    );
                }
                return;
            }
            let Some((file_part, def_key)) = ref_str.split_once("#/$defs/") else {
                return;
            };
            let target_path = current_dir.map(|dir| normalize_lexical_path(&dir.join(file_part)));
            let Some((target_idx, _)) = loaded
                .iter()
                .enumerate()
                .find(|(_, item)| target_path.as_ref() == Some(&item.path))
            else {
                return;
            };
            if let Some(qualified) = local_def_renames.get(&(target_idx, def_key.to_string())) {
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
) -> Result<(), CodegenError> {
    let Some(obj) = role_container.as_object() else {
        return Ok(());
    };
    for &role_key in ROLE_SCHEMA_KEYS {
        let Some(role_val) = obj.get(role_key).filter(|v| v.is_object()) else {
            continue;
        };
        let hoisted_name = qualify_def_name(stem_pascal, role_key);
        insert_sliced_or_normalized_def(role_val, &hoisted_name, stem_pascal, defs)?;
    }
    Ok(())
}

fn insert_sliced_or_normalized_def(
    raw_val: &Value,
    hoisted_name: &str,
    parent_pascal: &str,
    defs: &mut BTreeMap<String, Value>,
) -> Result<(), CodegenError> {
    if has_directional_annotations(raw_val) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn make_loaded(
        path: &str,
        stem: &str,
        name: Option<&str>,
        is_extension: bool,
        schema: Value,
    ) -> LoadedSchema {
        LoadedSchema {
            path: PathBuf::from(path),
            stem: stem.to_string(),
            stem_pascal: to_pascal_case(stem),
            name: name.map(str::to_string),
            is_extension,
            is_container: false,
            is_capability: !is_extension && name.is_some(),
            in_types_dir: false,
            schema,
        }
    }

    #[test]
    fn hoist_defs_qualifies_colliding_local_defs_and_skips_pure_reexports() {
        let loaded = vec![
            make_loaded(
                "/schemas/alpha.json",
                "alpha",
                Some("dev.ucp.shopping.alpha"),
                false,
                json!({
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
            make_loaded(
                "/schemas/beta.json",
                "beta",
                Some("dev.ucp.shopping.beta"),
                false,
                json!({
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
            make_loaded(
                "/schemas/types/line_item.json",
                "line_item",
                None,
                false,
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

        let (_, root_raw, overlays) = hoist_defs(
            &loaded,
            &reachable,
            &active_caps,
            &active_exts,
            &active_names,
            &mut defs,
        )
        .unwrap();

        assert!(overlays.is_empty());
        // Local-vs-local collision (`count > 1`) qualifies both `shared_detail` defs
        assert!(defs.contains_key("AlphaSharedDetail"));
        assert!(defs.contains_key("BetaSharedDetail"));
        // Self-named role container hoisted
        assert!(defs.contains_key("BetaPlatformSchema"));
        // Pure re-export `line_item` in alpha.json was skipped (not hoisted into defs)
        assert!(!defs.contains_key("LineItem"));

        // Local and cross-file `$ref`s to renamed `$defs` were updated
        assert_eq!(
            root_raw["Alpha"].1["properties"]["detail"]["$ref"],
            "#/$defs/AlphaSharedDetail"
        );
        assert_eq!(
            root_raw["Beta"].1["properties"]["detail"]["$ref"],
            "#/$defs/BetaSharedDetail"
        );
        assert_eq!(
            root_raw["Beta"].1["properties"]["alpha_detail"]["$ref"],
            "#/$defs/AlphaSharedDetail"
        );
    }
}
