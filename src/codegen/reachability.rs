//! Stage 1: Schema discovery, capability/extension selection, and transitive `$ref` reachability.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Component, Path, PathBuf};

use serde_json::{Map, Value};

use crate::codegen::normalizer::{is_reverse_domain_name, to_pascal_case};
use crate::codegen::CodegenError;
use crate::compose::is_container_schema;
use crate::error::ComposeError;
use crate::loader::{collect_schema_files, for_each_schema_object, load_schema};

pub(super) const AMBIENT_ROOT_STEMS: &[&str] = &[
    "profile",
    "ucp",
    "capability",
    "service",
    "payment_handler",
    "error_response",
];

#[derive(Debug, Clone)]
pub(super) struct LoadedSchema {
    pub path: PathBuf,
    pub stem: String,
    pub stem_pascal: String,
    pub name: Option<String>,
    pub is_extension: bool,
    pub is_container: bool,
    pub is_capability: bool,
    pub in_types_dir: bool,
    pub schema: Value,
}

impl LoadedSchema {
    pub(super) fn from_file(file_path: impl AsRef<Path>, schema: Value) -> Self {
        let path = normalize_lexical_path(file_path.as_ref());
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        let stem_pascal = to_pascal_case(&stem);
        let name = schema
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| is_reverse_domain_name(s))
            .map(str::to_string);
        let in_types_dir = path.components().any(|c| c.as_os_str() == "types");
        let in_transports_dir = path.components().any(|c| c.as_os_str() == "transports");
        let is_extension = is_extension_schema(&schema);
        let is_container =
            is_container_capability(&schema, is_extension, in_types_dir || in_transports_dir);
        let is_capability = !is_extension
            && !in_types_dir
            && !in_transports_dir
            && !AMBIENT_ROOT_STEMS.contains(&stem.as_str())
            && (name.is_some() || is_container);
        Self {
            path,
            stem,
            stem_pascal,
            name,
            is_extension,
            is_container,
            is_capability,
            in_types_dir,
            schema,
        }
    }
}

pub(super) fn load_all_schemas(schema_dir: &Path) -> Result<Vec<LoadedSchema>, CodegenError> {
    collect_schema_files(schema_dir)
        .into_iter()
        .map(|p| Ok(LoadedSchema::from_file(&p, load_schema(&p)?)))
        .collect()
}

#[allow(clippy::type_complexity)]
pub(super) fn select_active_schemas(
    loaded: &[LoadedSchema],
    cap_queries: Option<&[String]>,
    ext_queries: Option<&[String]>,
) -> Result<(BTreeSet<usize>, BTreeSet<usize>, BTreeSet<String>), CodegenError> {
    let cap_queries = cap_queries.filter(|q| !q.is_empty());
    let ext_queries = ext_queries.filter(|q| !q.is_empty());
    let mut active_caps = BTreeSet::new();
    let mut active_exts = BTreeSet::new();
    let resolve_query = |query: &str, kind: &str| -> Result<usize, CodegenError> {
        let trimmed = query.trim();
        loaded
            .iter()
            .position(|item| item.name.as_deref() == Some(trimmed))
            .ok_or_else(|| {
                CodegenError::ComposeError(ComposeError::InvalidCapability {
                    name: query.to_string(),
                    message: format!("{kind} schema not found in schema_dir"),
                })
            })
    };

    if let Some(queries) = cap_queries {
        for query in queries {
            let idx = resolve_query(query, "capability")?;
            if loaded[idx].is_extension {
                active_exts.insert(idx);
            } else {
                active_caps.insert(idx);
            }
        }
    } else {
        active_caps.extend(
            loaded
                .iter()
                .enumerate()
                .filter_map(|(i, s)| s.is_capability.then_some(i)),
        );
    }

    if let Some(queries) = ext_queries {
        for query in queries {
            active_exts.insert(resolve_query(query, "extension")?);
        }
    } else if cap_queries.is_none() {
        active_exts.extend(
            loaded
                .iter()
                .enumerate()
                .filter_map(|(i, s)| s.is_extension.then_some(i)),
        );
    }

    let mut active_capabilities = BTreeSet::new();
    for &idx in &active_caps {
        let item = &loaded[idx];
        active_capabilities.insert(item.stem.clone());
        if let Some(name) = &item.name {
            active_capabilities.insert(name.clone());
        }
    }

    Ok((active_caps, active_exts, active_capabilities))
}

pub(super) fn compute_reachable_closure(
    loaded: &[LoadedSchema],
    active_caps: &BTreeSet<usize>,
    active_exts: &BTreeSet<usize>,
    active_capabilities: &BTreeSet<String>,
    include_all: bool,
) -> BTreeSet<usize> {
    if include_all {
        return (0..loaded.len()).collect();
    }

    let path_to_idx: BTreeMap<PathBuf, usize> = loaded
        .iter()
        .enumerate()
        .map(|(i, item)| (item.path.clone(), i))
        .collect();

    let mut reachable = BTreeSet::new();
    let mut queue = VecDeque::new();

    for (idx, item) in loaded.iter().enumerate() {
        let is_ambient = AMBIENT_ROOT_STEMS.contains(&item.stem.as_str())
            && (!item.in_types_dir || item.stem == "error_response");
        if (is_ambient || active_caps.contains(&idx) || active_exts.contains(&idx))
            && reachable.insert(idx)
        {
            queue.push_back(idx);
        }
    }

    while let Some(idx) = queue.pop_front() {
        let item = &loaded[idx];
        let Some(parent_dir) = item.path.parent() else {
            continue;
        };
        let inactive_locals = compute_inactive_local_defs(item, active_capabilities);
        for ref_str in collect_active_external_refs(item, &inactive_locals) {
            let file_part = ref_str.split('#').next().unwrap_or("");
            if file_part.is_empty() {
                continue;
            }
            let resolved_path = normalize_lexical_path(&parent_dir.join(file_part));
            if let Some(&t_idx) = path_to_idx.get(&resolved_path) {
                if reachable.insert(t_idx) {
                    queue.push_back(t_idx);
                }
            }
        }
    }
    reachable
}

pub(super) fn compute_inactive_local_defs(
    item: &LoadedSchema,
    active_capabilities: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut inactive = BTreeSet::new();
    let Some(defs_obj) = item.schema.get("$defs").and_then(Value::as_object) else {
        return inactive;
    };

    if item.is_capability {
        for (def_key, def_val) in defs_obj {
            if !active_capabilities.contains(def_key)
                && is_unreferenced_cross_capability_overlay(item, def_key, def_val)
            {
                inactive.insert(def_key.clone());
            }
        }
    }
    if !item.is_extension {
        return inactive;
    }

    let mut active_seeds = Vec::new();
    let mut inactive_seeds = Vec::new();
    for (k, v) in defs_obj {
        if !is_reverse_domain_name(k) {
            continue;
        }
        if item.name.as_deref() == Some(k.as_str()) || active_capabilities.contains(k) {
            active_seeds.push(v);
        } else {
            inactive.insert(k.clone());
            inactive_seeds.push(v);
        }
    }

    let used_by_active = transitive_local_def_refs(&active_seeds, defs_obj);
    let used_by_inactive = transitive_local_def_refs(&inactive_seeds, defs_obj);

    for (def_key, def_val) in defs_obj {
        if is_reverse_domain_name(def_key) || used_by_active.contains(def_key) {
            continue;
        }
        let is_external_ref_alias = def_val
            .get("$ref")
            .and_then(Value::as_str)
            .is_some_and(|r| !r.starts_with('#'));
        if used_by_inactive.contains(def_key) || is_external_ref_alias {
            inactive.insert(def_key.clone());
        }
    }
    inactive
}

fn is_unreferenced_cross_capability_overlay(
    item: &LoadedSchema,
    def_key: &str,
    def_val: &Value,
) -> bool {
    let target_file = format!("{def_key}.json");
    let target_suffix = format!("/{target_file}");
    let extends_other_cap = def_val
        .get("allOf")
        .and_then(Value::as_array)
        .is_some_and(|all_of| {
            all_of.iter().any(|b| {
                b.get("$ref")
                    .and_then(Value::as_str)
                    .is_some_and(|r| r == target_file || r.ends_with(&target_suffix))
            })
        });
    if !extends_other_cap {
        return false;
    }
    let local_ref = format!("#/$defs/{def_key}");
    let mut referenced = false;
    if let Some(root_obj) = item.schema.as_object() {
        for (k, v) in root_obj {
            if k == "$defs" {
                if let Some(defs) = v.as_object() {
                    for (other_k, other_v) in defs {
                        if other_k != def_key {
                            for_each_schema_object(other_v, &mut |obj| {
                                if obj.get("$ref").and_then(Value::as_str) == Some(&local_ref) {
                                    referenced = true;
                                }
                            });
                        }
                    }
                }
            } else {
                for_each_schema_object(v, &mut |obj| {
                    if obj.get("$ref").and_then(Value::as_str) == Some(&local_ref) {
                        referenced = true;
                    }
                });
            }
        }
    }
    !referenced
}

pub(super) fn is_container_op_key(key: &str) -> bool {
    key.ends_with("_request") || key.ends_with("_response")
}

pub(super) fn normalize_lexical_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn is_extension_schema(schema: &Value) -> bool {
    let own_name = schema.get("name").and_then(Value::as_str);
    schema
        .get("$defs")
        .and_then(Value::as_object)
        .is_some_and(|defs| {
            defs.keys()
                .any(|k| is_reverse_domain_name(k) && Some(k.as_str()) != own_name)
        })
}

fn is_container_capability(schema: &Value, is_extension: bool, in_types_dir: bool) -> bool {
    if is_extension || in_types_dir || !is_container_schema(schema) {
        return false;
    }
    let Some(obj) = schema.as_object() else {
        return false;
    };
    if obj.contains_key("oneOf")
        || obj.contains_key("anyOf")
        || obj.contains_key("propertyNames")
        || obj
            .get("additionalProperties")
            .is_some_and(Value::is_object)
    {
        return false;
    }
    obj.get("$defs")
        .and_then(Value::as_object)
        .is_some_and(|defs| defs.keys().any(|k| is_container_op_key(k)))
}

fn transitive_local_def_refs(seeds: &[&Value], defs_obj: &Map<String, Value>) -> BTreeSet<String> {
    let mut visited = BTreeSet::new();
    let mut queue = VecDeque::new();
    let push_refs = |val: &Value, q: &mut VecDeque<String>| {
        for_each_schema_object(val, &mut |obj| {
            if let Some(k) = obj
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|r| r.strip_prefix("#/$defs/"))
            {
                q.push_back(k.to_string());
            }
        });
    };

    for seed in seeds {
        push_refs(seed, &mut queue);
    }
    while let Some(def_key) = queue.pop_front() {
        if visited.insert(def_key.clone()) {
            if let Some(def_val) = defs_obj.get(&def_key) {
                push_refs(def_val, &mut queue);
            }
        }
    }
    visited
}

fn collect_active_external_refs(
    item: &LoadedSchema,
    inactive_locals: &BTreeSet<String>,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut push_ext_refs = |val: &Value| {
        for_each_schema_object(val, &mut |obj| {
            if let Some(r) = obj.get("$ref").and_then(Value::as_str) {
                if !r.starts_with('#') {
                    out.push(r.to_string());
                }
            }
        });
    };

    let Some(root_obj) = item.schema.as_object() else {
        return out;
    };
    for (k, v) in root_obj {
        if matches!(k.as_str(), "embedded" | "requires") {
            continue;
        }
        if k != "$defs" {
            push_ext_refs(v);
            continue;
        }
        let Some(defs_obj) = v.as_object() else {
            continue;
        };
        for (def_k, def_v) in defs_obj {
            if !inactive_locals.contains(def_k) {
                push_ext_refs(def_v);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn select_active_schemas_reclassifies_extensions_and_supports_default_caps_with_explicit_exts()
    {
        let loaded = vec![
            LoadedSchema::from_file(
                "/schemas/shopping/checkout.json",
                json!({"name": "dev.ucp.shopping.checkout", "type": "object", "properties": {"id": {"type": "string"}}}),
            ),
            LoadedSchema::from_file(
                "/schemas/shopping/discount.json",
                json!({"name": "dev.ucp.shopping.discount", "$defs": {"dev.ucp.shopping.checkout": {"properties": {}}}}),
            ),
            LoadedSchema::from_file(
                "/schemas/shopping/types/discount.json",
                json!({"type": "object"}),
            ),
        ];

        // 1. Passing an extension in `cap_queries` reclassifies it into `active_exts`
        let caps = vec![
            "dev.ucp.shopping.checkout".to_string(),
            "dev.ucp.shopping.discount".to_string(),
        ];
        let (active_caps, active_exts, active_names) =
            select_active_schemas(&loaded, Some(&caps), None).unwrap();
        assert_eq!(active_caps, BTreeSet::from([0]));
        assert_eq!(active_exts, BTreeSet::from([1]));
        assert!(active_names.contains("checkout"));
        assert!(active_names.contains("dev.ucp.shopping.checkout"));

        // 2. `cap_queries: None` with explicit `ext_queries` activates all capabilities and selects requested extensions
        let exts = vec!["dev.ucp.shopping.discount".to_string()];
        let (all_caps, selected_exts, _) =
            select_active_schemas(&loaded, None, Some(&exts)).unwrap();
        assert_eq!(all_caps, BTreeSet::from([0]));
        assert_eq!(selected_exts, BTreeSet::from([1]));

        // 3. Empty slices `Some(&[])` normalize to `None` (selecting all capabilities and extensions)
        let (all_caps_empty, all_exts_empty, _) =
            select_active_schemas(&loaded, Some(&[]), Some(&[])).unwrap();
        assert_eq!(all_caps_empty, BTreeSet::from([0]));
        assert_eq!(all_exts_empty, BTreeSet::from([1]));

        // 4. Short names ("checkout") are rejected with InvalidCapability
        let err = select_active_schemas(&loaded, Some(&["checkout".to_string()]), None)
            .expect_err("short capability names must be rejected");
        assert!(matches!(
            err,
            CodegenError::ComposeError(ComposeError::InvalidCapability { .. })
        ));
    }

    #[test]
    fn compute_inactive_local_defs_prunes_inactive_extension_helpers_and_cart_checkout() {
        let ext = LoadedSchema::from_file(
            "/schemas/shopping/ext.json",
            json!({
                "name": "dev.ucp.shopping.ext",
                "$defs": {
                    "active_helper": { "type": "object" },
                    "shared_helper": { "type": "object" },
                    "inactive_helper": {
                        "type": "object",
                        "properties": { "nested": { "$ref": "#/$defs/inactive_leaf" } }
                    },
                    "inactive_leaf": { "type": "string" },
                    "dev.ucp.shopping.checkout": {
                        "properties": {
                            "a": { "$ref": "#/$defs/active_helper" },
                            "s": { "$ref": "#/$defs/shared_helper" }
                        }
                    },
                    "dev.ucp.shopping.order": {
                        "properties": {
                            "i": { "$ref": "#/$defs/inactive_helper" },
                            "s": { "$ref": "#/$defs/shared_helper" }
                        }
                    }
                }
            }),
        );

        let active_caps = BTreeSet::from([
            "checkout".to_string(),
            "dev.ucp.shopping.checkout".to_string(),
        ]);
        let inactive = compute_inactive_local_defs(&ext, &active_caps);
        assert!(inactive.contains("inactive_helper"));
        assert!(inactive.contains("inactive_leaf"));
        assert!(!inactive.contains("active_helper"));
        assert!(!inactive.contains("shared_helper"));

        let cart = LoadedSchema::from_file(
            "/schemas/shopping/cart.json",
            json!({
                "name": "dev.ucp.shopping.cart",
                "type": "object",
                "$defs": {
                    "checkout": {
                        "allOf": [{ "$ref": "checkout.json" }, { "type": "object" }]
                    },
                    "internal_helper": {
                        "allOf": [{ "$ref": "internal_helper.json" }],
                        "type": "object"
                    }
                },
                "properties": {
                    "helper": { "$ref": "#/$defs/internal_helper" }
                }
            }),
        );
        let cart_only_caps = BTreeSet::from(["cart".to_string()]);
        let cart_inactive = compute_inactive_local_defs(&cart, &cart_only_caps);
        assert!(cart_inactive.contains("checkout"));
        assert!(!cart_inactive.contains("internal_helper"));
    }

    #[test]
    fn compute_reachable_closure_follows_active_refs_and_skips_inactive_extension_refs() {
        let loaded = vec![
            LoadedSchema::from_file("/schemas/ucp.json", json!({"type": "object"})),
            LoadedSchema::from_file(
                "/schemas/shopping/types/service.json",
                json!({"type": "object"}),
            ),
            LoadedSchema::from_file(
                "/schemas/shopping/checkout.json",
                json!({
                    "name": "dev.ucp.shopping.checkout",
                    "type": "object",
                    "properties": { "buyer": { "$ref": "types/buyer.json" } }
                }),
            ),
            LoadedSchema::from_file(
                "/schemas/shopping/types/buyer.json",
                json!({"type": "object"}),
            ),
            LoadedSchema::from_file(
                "/schemas/shopping/types/order_only.json",
                json!({"type": "object"}),
            ),
            LoadedSchema::from_file(
                "/schemas/shopping/ext.json",
                json!({
                    "name": "dev.ucp.shopping.ext",
                    "$defs": {
                        "order_helper": { "$ref": "types/order_only.json" },
                        "dev.ucp.shopping.checkout": { "properties": {} },
                        "dev.ucp.shopping.order": {
                            "properties": { "o": { "$ref": "#/$defs/order_helper" } }
                        }
                    }
                }),
            ),
        ];

        let reachable = compute_reachable_closure(
            &loaded,
            &BTreeSet::from([2]),
            &BTreeSet::from([5]),
            &BTreeSet::from([
                "checkout".to_string(),
                "dev.ucp.shopping.checkout".to_string(),
            ]),
            false,
        );
        // Ambient root ucp.json (0), checkout.json (2), buyer.json (3), ext.json (5) are reachable;
        // types/service.json (1, in types/ dir) and types/order_only.json (4, only referenced by inactive order) are not.
        assert_eq!(reachable, BTreeSet::from([0, 2, 3, 5]));
    }
}
