//! Stage 1: Schema discovery, capability/extension selection, and transitive `$ref` reachability.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Component, Path, PathBuf};

use serde_json::{Map, Value};

use crate::codegen::normalizer::{is_reverse_domain_name, to_pascal_case};
use crate::codegen::CodegenError;
use crate::compose::{capability_short_name, is_container_schema};
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

pub(super) fn load_all_schemas(schema_dir: &Path) -> Result<Vec<LoadedSchema>, CodegenError> {
    let files = collect_schema_files(schema_dir);
    let mut loaded = Vec::with_capacity(files.len());
    for file_path in files {
        let schema = load_schema(&file_path)?;
        let path = normalize_lexical_path(&file_path);
        let stem = file_stem_str(&path);
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
        loaded.push(LoadedSchema {
            path,
            stem,
            stem_pascal,
            name,
            is_extension,
            is_container,
            is_capability,
            in_types_dir,
            schema,
        });
    }
    Ok(loaded)
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

    match cap_queries {
        Some(queries) => {
            for query in queries {
                let idx = find_matching_schema(loaded, query, false).ok_or_else(|| {
                    CodegenError::ComposeError(ComposeError::InvalidCapability {
                        name: query.clone(),
                        message: "capability schema not found in schema_dir".to_string(),
                    })
                })?;
                if loaded[idx].is_extension {
                    active_exts.insert(idx);
                } else {
                    active_caps.insert(idx);
                }
            }
        }
        None => {
            active_caps.extend(
                loaded
                    .iter()
                    .enumerate()
                    .filter_map(|(i, s)| s.is_capability.then_some(i)),
            );
        }
    }

    match ext_queries {
        Some(queries) => {
            for query in queries {
                let idx = find_matching_schema(loaded, query, true).ok_or_else(|| {
                    CodegenError::ComposeError(ComposeError::InvalidCapability {
                        name: query.clone(),
                        message: "extension schema not found in schema_dir".to_string(),
                    })
                })?;
                active_exts.insert(idx);
            }
        }
        None if cap_queries.is_none() => {
            active_exts.extend(
                loaded
                    .iter()
                    .enumerate()
                    .filter_map(|(i, s)| s.is_extension.then_some(i)),
            );
        }
        None => {}
    }

    let mut active_capabilities = BTreeSet::new();
    for &idx in &active_caps {
        let item = &loaded[idx];
        active_capabilities.insert(item.stem.clone());
        if let Some(name) = &item.name {
            active_capabilities.insert(name.clone());
            active_capabilities.insert(capability_short_name(name));
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
        for ref_str in collect_active_external_refs(item, &inactive_locals, active_capabilities) {
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

    if item.stem == "cart"
        && !active_capabilities.contains("checkout")
        && defs_obj.contains_key("checkout")
    {
        inactive.insert("checkout".to_string());
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
        let is_active = item.name.as_deref() == Some(k.as_str())
            || active_capabilities.contains(k)
            || active_capabilities.contains(&capability_short_name(k));
        if is_active {
            active_seeds.push(v);
        } else {
            inactive_seeds.push(v);
        }
    }

    let used_by_active = transitive_local_def_refs(&active_seeds, defs_obj);
    let used_by_inactive = transitive_local_def_refs(&inactive_seeds, defs_obj);

    for def_key in defs_obj.keys() {
        if !is_reverse_domain_name(def_key)
            && used_by_inactive.contains(def_key)
            && !used_by_active.contains(def_key)
        {
            inactive.insert(def_key.clone());
        }
    }
    inactive
}

pub(super) fn is_container_op_key(key: &str) -> bool {
    key.ends_with("_request") || key.ends_with("_response")
}

pub(super) fn has_root_schema_body(schema: &Value) -> bool {
    let Some(obj) = schema.as_object() else {
        return false;
    };
    [
        "properties",
        "allOf",
        "oneOf",
        "anyOf",
        "$ref",
        "enum",
        "const",
        "pattern",
        "propertyNames",
        "items",
    ]
    .iter()
    .any(|k| obj.contains_key(*k))
        || obj
            .get("additionalProperties")
            .is_some_and(Value::is_object)
        || obj
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| t != "object")
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

fn find_matching_schema(
    loaded: &[LoadedSchema],
    query: &str,
    prefer_extension: bool,
) -> Option<usize> {
    let trimmed = query.trim().trim_end_matches(".json");
    if let Some((idx, _)) = loaded
        .iter()
        .enumerate()
        .find(|(_, item)| item.name.as_deref() == Some(trimmed))
    {
        return Some(idx);
    }

    let short = capability_short_name(trimmed);
    let candidates: Vec<usize> = loaded
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            item.stem == trimmed
                || item.stem == short
                || item
                    .name
                    .as_deref()
                    .is_some_and(|n| capability_short_name(n) == short)
        })
        .map(|(idx, _)| idx)
        .collect();

    if let Some(&idx) = candidates
        .iter()
        .find(|&&i| prefer_extension && loaded[i].is_extension)
    {
        return Some(idx);
    }
    candidates
        .iter()
        .find(|&&i| loaded[i].name.is_some())
        .or_else(|| candidates.iter().find(|&&i| !loaded[i].in_types_dir))
        .or_else(|| candidates.first())
        .copied()
}

fn transitive_local_def_refs(seeds: &[&Value], defs_obj: &Map<String, Value>) -> BTreeSet<String> {
    let mut visited = BTreeSet::new();
    let mut queue = VecDeque::new();
    let push_refs = |val: &Value, q: &mut VecDeque<String>| {
        for_each_schema_object(val, &mut |obj| {
            let Some(Value::String(r)) = obj.get("$ref") else {
                return;
            };
            if let Some(k) = r.strip_prefix("#/$defs/") {
                q.push_back(k.to_string());
            }
        });
    };

    for seed in seeds {
        push_refs(seed, &mut queue);
    }
    while let Some(def_key) = queue.pop_front() {
        if !visited.insert(def_key.clone()) {
            continue;
        }
        if let Some(def_val) = defs_obj.get(&def_key) {
            push_refs(def_val, &mut queue);
        }
    }
    visited
}

fn collect_active_external_refs(
    item: &LoadedSchema,
    inactive_locals: &BTreeSet<String>,
    active_capabilities: &BTreeSet<String>,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut push_ext_refs = |val: &Value| {
        for_each_schema_object(val, &mut |obj| {
            let Some(r) = obj.get("$ref").and_then(Value::as_str) else {
                return;
            };
            if !r.starts_with('#') {
                out.push(r.to_string());
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
            if inactive_locals.contains(def_k) {
                continue;
            }
            if is_reverse_domain_name(def_k) {
                let is_active = item.name.as_deref() == Some(def_k.as_str())
                    || active_capabilities.contains(def_k)
                    || active_capabilities.contains(&capability_short_name(def_k));
                if !is_active {
                    continue;
                }
            }
            push_ext_refs(def_v);
        }
    }
    out
}

fn file_stem_str(path: &Path) -> String {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    name.strip_suffix(".json").unwrap_or(name).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_loaded(
        path: &str,
        stem: &str,
        name: Option<&str>,
        is_extension: bool,
        in_types_dir: bool,
        schema: Value,
    ) -> LoadedSchema {
        LoadedSchema {
            path: PathBuf::from(path),
            stem: stem.to_string(),
            stem_pascal: to_pascal_case(stem),
            name: name.map(str::to_string),
            is_extension,
            is_container: false,
            is_capability: !is_extension && !in_types_dir && name.is_some(),
            in_types_dir,
            schema,
        }
    }

    #[test]
    fn select_active_schemas_reclassifies_extensions_and_supports_default_caps_with_explicit_exts()
    {
        let loaded = vec![
            make_loaded(
                "/schemas/shopping/checkout.json",
                "checkout",
                Some("dev.ucp.shopping.checkout"),
                false,
                false,
                json!({"type": "object", "properties": {"id": {"type": "string"}}}),
            ),
            make_loaded(
                "/schemas/shopping/discount.json",
                "discount",
                Some("dev.ucp.shopping.discount"),
                true,
                false,
                json!({"$defs": {"dev.ucp.shopping.checkout": {"properties": {}}}}),
            ),
            make_loaded(
                "/schemas/shopping/types/discount.json",
                "discount",
                None,
                false,
                true,
                json!({"type": "object"}),
            ),
        ];

        // 1. Passing an extension in `cap_queries` reclassifies it into `active_exts`
        let caps = vec!["checkout.json".to_string(), "discount".to_string()];
        let (active_caps, active_exts, active_names) =
            select_active_schemas(&loaded, Some(&caps), None).unwrap();
        assert_eq!(active_caps, BTreeSet::from([0]));
        assert_eq!(active_exts, BTreeSet::from([1]));
        assert!(active_names.contains("checkout"));
        assert!(active_names.contains("dev.ucp.shopping.checkout"));

        // 2. `cap_queries: None` with explicit `ext_queries` activates all capabilities and prefers extension over types/
        let exts = vec!["discount".to_string()];
        let (all_caps, selected_exts, _) =
            select_active_schemas(&loaded, None, Some(&exts)).unwrap();
        assert_eq!(all_caps, BTreeSet::from([0]));
        assert_eq!(selected_exts, BTreeSet::from([1]));

        // 3. Empty slices `Some(&[])` normalize to `None` (selecting all capabilities and extensions)
        let (all_caps_empty, all_exts_empty, _) =
            select_active_schemas(&loaded, Some(&[]), Some(&[])).unwrap();
        assert_eq!(all_caps_empty, BTreeSet::from([0]));
        assert_eq!(all_exts_empty, BTreeSet::from([1]));
    }

    #[test]
    fn compute_inactive_local_defs_prunes_inactive_extension_helpers_and_cart_checkout() {
        let ext = make_loaded(
            "/schemas/shopping/ext.json",
            "ext",
            Some("dev.ucp.shopping.ext"),
            true,
            false,
            json!({
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

        let cart = make_loaded(
            "/schemas/shopping/cart.json",
            "cart",
            Some("dev.ucp.shopping.cart"),
            false,
            false,
            json!({
                "type": "object",
                "$defs": { "checkout": { "type": "object" } }
            }),
        );
        let cart_only_caps = BTreeSet::from(["cart".to_string()]);
        assert!(compute_inactive_local_defs(&cart, &cart_only_caps).contains("checkout"));
    }

    #[test]
    fn compute_reachable_closure_follows_active_refs_and_skips_inactive_extension_refs() {
        let loaded = vec![
            make_loaded(
                "/schemas/ucp.json",
                "ucp",
                None,
                false,
                false,
                json!({"type": "object"}),
            ),
            make_loaded(
                "/schemas/shopping/types/service.json",
                "service",
                None,
                false,
                true,
                json!({"type": "object"}),
            ),
            make_loaded(
                "/schemas/shopping/checkout.json",
                "checkout",
                Some("dev.ucp.shopping.checkout"),
                false,
                false,
                json!({
                    "type": "object",
                    "properties": { "buyer": { "$ref": "types/buyer.json" } }
                }),
            ),
            make_loaded(
                "/schemas/shopping/types/buyer.json",
                "buyer",
                None,
                false,
                true,
                json!({"type": "object"}),
            ),
            make_loaded(
                "/schemas/shopping/types/order_only.json",
                "order_only",
                None,
                false,
                true,
                json!({"type": "object"}),
            ),
            make_loaded(
                "/schemas/shopping/ext.json",
                "ext",
                Some("dev.ucp.shopping.ext"),
                true,
                false,
                json!({
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
            &BTreeSet::from(["checkout".to_string()]),
            false,
        );
        // Ambient root ucp.json (0), checkout.json (2), buyer.json (3), ext.json (5) are reachable;
        // types/service.json (1, in types/ dir) and types/order_only.json (4, only referenced by inactive order) are not.
        assert_eq!(reachable, BTreeSet::from([0, 2, 3, 5]));
    }
}
