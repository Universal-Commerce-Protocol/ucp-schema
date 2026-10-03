//! Naming, UCP keyword stripping, `#/$defs/` reference rewriting, base normalization,
//! directional slicing, and `$ref` alignment for code generation.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use crate::compose::capability_short_name;
use crate::error::ResolveError;
use crate::loader::{for_each_schema_object, for_each_schema_object_mut, INSTANCE_DATA_KEYWORDS};
use crate::resolver::resolve;
use crate::types::{is_valid_version, Direction, ResolveOptions, UCP_ANNOTATIONS};

/// Convert a snake_case, kebab-case, dotted, or reverse-domain identifier into PascalCase.
pub fn to_pascal_case(s: &str) -> String {
    if !s.contains(['_', '-', ' ', '/', '.']) && s.starts_with(|c: char| c.is_ascii_uppercase()) {
        return s.to_string();
    }

    let base = if is_reverse_domain_name(s) {
        capability_short_name(s)
    } else {
        s.to_string()
    };

    base.split(['_', '-', ' ', '/', '.'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mixed = part.chars().any(|c| c.is_ascii_uppercase())
                && part.chars().any(|c| c.is_ascii_lowercase());
            let mut chars = part.chars();
            let Some(first) = chars.next() else {
                return String::new();
            };
            let rest = if mixed {
                chars.as_str().to_string()
            } else {
                chars.as_str().to_ascii_lowercase()
            };
            format!("{}{rest}", first.to_ascii_uppercase())
        })
        .collect()
}

/// Returns `true` when `s` contains at least 3 dot-separated segments where every segment
/// is non-empty ASCII alphanumeric, `_`, or `-` (excluding `.json` filenames).
pub fn is_reverse_domain_name(s: &str) -> bool {
    if s.ends_with(".json") {
        return false;
    }
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() >= 3
        && parts.iter().all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
}

/// Returns `true` when a `$defs` key name is generic and must be qualified with its parent
/// schema name when hoisted into the shared `#/$defs/` namespace.
pub fn is_generic_def_name(name: &str) -> bool {
    const GENERIC: &str = "base|Base|entity|Entity|platform_schema|PlatformSchema|business_schema|BusinessSchema|response_schema|ResponseSchema|request|Request|response|Response|error|Error|config|Config|endpoint|Endpoint|quantity|Quantity";
    !name.is_empty() && GENERIC.split('|').any(|g| g == name)
}

/// Qualify a hoisted `$defs` entry name with `parent_pascal` when `def_key` is generic
/// or represents a container operation definition.
pub fn qualify_def_name(parent_pascal: &str, def_key: &str) -> String {
    let def_pascal = to_pascal_case(def_key);
    if is_generic_def_name(def_key) || is_generic_def_name(&def_pascal) {
        if parent_pascal.is_empty() || def_pascal.starts_with(parent_pascal) {
            def_pascal
        } else {
            format!("{parent_pascal}{def_pascal}")
        }
    } else if (def_key.ends_with("_request") || def_key.ends_with("_response"))
        && def_key != "complete_request"
    {
        qualify_container_op_name(parent_pascal, def_key)
    } else {
        def_pascal
    }
}

/// Qualify a container schema operation definition key (`{op}_{direction}`) with the
/// container schema's domain prefix (e.g. `("CatalogSearch", "search_request")` -> `"CatalogSearchRequest"`).
pub fn qualify_container_op_name(stem_pascal: &str, op_key: &str) -> String {
    let op_pascal = to_pascal_case(op_key);
    if stem_pascal.is_empty() || op_pascal.starts_with(stem_pascal) {
        return op_pascal;
    }
    let prefix = match stem_pascal.rfind(|c: char| c.is_ascii_uppercase()) {
        Some(idx) if idx > 0 => &stem_pascal[..idx],
        _ => stem_pascal,
    };
    if op_pascal.starts_with(prefix) {
        op_pascal
    } else {
        format!("{prefix}{op_pascal}")
    }
}

/// Resolve a `$ref` string (`#`, `#/$defs/<key>`, `<file>#/$defs/<key>`, or `<file>.json`)
/// to its canonical PascalCase `$defs` key name.
pub fn ref_to_def_name(ref_str: &str, parent_name: Option<&str>) -> String {
    if ref_str == "#" || ref_str == "#/" {
        return parent_name.map_or_else(|| "Self".to_string(), to_pascal_case);
    }
    if let Some(def_key) = ref_str
        .strip_prefix("#/$defs/")
        .or_else(|| ref_str.strip_prefix("#/definitions/"))
    {
        if def_key.starts_with(|c: char| c.is_ascii_uppercase()) {
            return def_key.to_string();
        }
        let parent_pascal = parent_name.map(to_pascal_case).unwrap_or_default();
        return qualify_def_name(&parent_pascal, def_key);
    }
    if let Some((file_part, def_key)) = ref_str
        .split_once("#/$defs/")
        .or_else(|| ref_str.split_once("#/definitions/"))
    {
        return qualify_def_name(&file_stem_to_pascal(file_part), def_key);
    }
    file_stem_to_pascal(ref_str.split('#').next().unwrap_or(ref_str))
}

/// Strip UCP-specific authoring keywords and root metadata from a schema AST while
/// preserving instance-data payloads (`const`, `enum`, `default`, `examples`).
pub fn strip_ucp_keywords(value: &mut Value) {
    if let Some(root) = value.as_object_mut() {
        if root
            .get("version")
            .and_then(Value::as_str)
            .is_some_and(is_valid_version)
        {
            root.remove("version");
        }
        root.remove("requires");
        root.remove("embedded");
    }

    for_each_schema_object_mut(value, &mut |obj| {
        for key in UCP_ANNOTATIONS {
            obj.remove(*key);
        }
        obj.remove("ucp_shared_request");
        if obj
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(is_reverse_domain_name)
        {
            obj.remove("name");
        }
        for key in ["$schema", "$id"] {
            if obj.get(key).is_some_and(Value::is_string) {
                obj.remove(key);
            }
        }
        obj.retain(|k, _| !k.starts_with("x-ucp-"));
        if obj.contains_key("$ref") {
            obj.remove("type");
        }
        if let Some(Value::Array(all_of)) = obj.get_mut("allOf") {
            for branch in all_of.iter_mut().filter_map(Value::as_object_mut) {
                branch.remove("title");
            }
        }
    });

    if let Some(root) = value.as_object_mut() {
        clean_vacuous_anyof(root);
        prune_dangling_required(root, &BTreeSet::new(), false);
    }
}

/// Rewrite all schema-position `$ref` pointers in `value` to `#/$defs/<PascalName>`.
pub fn rewrite_refs_to_defs(value: &mut Value, current_def: &str, parent_name: Option<&str>) {
    let effective_parent = parent_name.unwrap_or(current_def);
    for_each_schema_object_mut(value, &mut |obj| {
        let Some(Value::String(ref_str)) = obj.get("$ref") else {
            return;
        };
        let target = ref_to_def_name(ref_str, Some(effective_parent));
        obj.insert(
            "$ref".to_string(),
            Value::String(format!("#/$defs/{target}")),
        );
    });
}

/// Distribute top-level `properties`, `required`, `additionalProperties`, and parent `$ref`
/// `allOf` branches into `anyOf` branches when at least one `anyOf` branch specifies `required`
/// without `properties` (e.g., `ValueConstraint`, `StayCreateRequest`, `StayUpdateRequest`).
pub fn distribute_properties_in_bare_anyof(value: &mut Value) {
    match value {
        Value::Object(obj) => {
            for (k, child) in obj.iter_mut() {
                if !INSTANCE_DATA_KEYWORDS.contains(&k.as_str()) {
                    distribute_properties_in_bare_anyof(child);
                }
            }
            distribute_bare_anyof_on_object(obj);
        }
        Value::Array(arr) => {
            for item in arr {
                distribute_properties_in_bare_anyof(item);
            }
        }
        _ => {}
    }
}

/// Collapse 2-element `oneOf` unions of a scalar/ref schema `S` and `{"type": "array", "items": S}`
/// (such as `CapabilityBase.properties.extends`) into the canonical array schema.
pub fn normalize_scalar_or_array_unions(value: &mut Value) {
    for_each_schema_object_mut(value, &mut |obj| {
        let Some(Value::Array(one_of)) = obj.get("oneOf") else {
            return;
        };
        if one_of.len() != 2 {
            return;
        }
        let is_arr0 = one_of[0].get("type").and_then(Value::as_str) == Some("array");
        let is_arr1 = one_of[1].get("type").and_then(Value::as_str) == Some("array");
        if !(is_arr0 ^ is_arr1) {
            return;
        }
        let (scalar_b, array_b) = if is_arr1 {
            (&one_of[0], &one_of[1])
        } else {
            (&one_of[1], &one_of[0])
        };
        let Some(items) = array_b.get("items") else {
            return;
        };
        let matches = scalar_b == items
            || (scalar_b.get("$ref").is_some() && scalar_b.get("$ref") == items.get("$ref"))
            || (scalar_b.get("type").is_some() && scalar_b.get("type") == items.get("type"));
        if !matches {
            return;
        }
        let Some(array_obj) = array_b.as_object().cloned() else {
            return;
        };
        obj.remove("oneOf");
        for (k, v) in array_obj {
            obj.entry(k).or_insert(v);
        }
    });
}

/// Attach `"default": V` and infer `"type"` on schema objects that define a primitive `"const": V`
/// or single-element `"enum": [V]` without `"default"`, skipping `if`, `then`, `else`, `not`,
/// `contains`, `propertyNames`, and `INSTANCE_DATA_KEYWORDS` subtrees.
pub fn attach_const_defaults(value: &mut Value) {
    match value {
        Value::Object(obj) => {
            attach_default_if_const(obj);

            for (key, child) in obj.iter_mut() {
                if ["if", "then", "else", "not", "contains", "propertyNames"]
                    .contains(&key.as_str())
                    || INSTANCE_DATA_KEYWORDS.contains(&key.as_str())
                {
                    continue;
                }
                if [
                    "properties",
                    "$defs",
                    "patternProperties",
                    "dependentSchemas",
                ]
                .contains(&key.as_str())
                {
                    if let Value::Object(prop_map) = child {
                        for prop_schema in prop_map.values_mut() {
                            attach_const_defaults(prop_schema);
                        }
                        continue;
                    }
                }
                attach_const_defaults(child);
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

fn attach_default_if_const(obj: &mut Map<String, Value>) {
    let val = obj.get("const").cloned().or_else(|| {
        let arr = obj.get("enum")?.as_array()?;
        (arr.len() == 1).then(|| arr[0].clone())
    });
    let Some(val) = val else {
        return;
    };
    let ty = match &val {
        Value::String(_) => "string",
        Value::Number(_) => "number",
        Value::Bool(_) => "boolean",
        _ => return,
    };
    obj.entry("default".to_string()).or_insert(val);
    obj.entry("type".to_string())
        .or_insert_with(|| Value::String(ty.to_string()));
}

/// Flatten single-object inheritance and extension `allOf` compositions in `defs` (as well as
/// `items.allOf` on array schemas and validation-only `contains` `allOf` branches on array roots)
/// into flat `"type": "object"` schemas while preserving conditional `if`/`then` union blocks.
pub fn flatten_object_allof(defs: &mut BTreeMap<String, Value>) {
    for schema_val in defs.values_mut() {
        let Some(obj) = schema_val.as_object_mut() else {
            continue;
        };
        if obj.get("type").and_then(Value::as_str) != Some("array") {
            continue;
        }
        let Some(Value::Array(all_of)) = obj.get_mut("allOf") else {
            continue;
        };
        all_of.retain(|b| !b.as_object().is_some_and(|m| m.contains_key("contains")));
        if all_of.is_empty() {
            obj.remove("allOf");
        }
    }

    loop {
        let snapshot = defs.clone();
        let mut changed = false;
        for schema_val in defs.values_mut() {
            if flatten_single_object_allof(schema_val, &snapshot) {
                changed = true;
            }
            if schema_val.get("type").and_then(Value::as_str) != Some("array") {
                continue;
            }
            let Some(items_val) = schema_val.get_mut("items") else {
                continue;
            };
            if flatten_single_object_allof(items_val, &snapshot) {
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

/// Normalize a single `$defs` entry by stripping UCP authoring keywords, distributing bare
/// `anyOf` properties, normalizing scalar-or-array unions, rewriting `$ref` pointers to
/// `#/$defs/<PascalName>`, attaching `const` defaults, synchronizing `"title"` to `current_def`,
/// and setting `"additionalProperties": true` on object schemas that omit `"additionalProperties"`
/// to preserve UCP's open-world property semantics.
pub fn normalize_def_schema(schema: &Value, current_def: &str, parent_name: Option<&str>) -> Value {
    let mut val = schema.clone();
    strip_ucp_keywords(&mut val);
    distribute_properties_in_bare_anyof(&mut val);
    normalize_scalar_or_array_unions(&mut val);
    rewrite_refs_to_defs(&mut val, current_def, parent_name);
    attach_const_defaults(&mut val);
    let Some(obj) = val.as_object_mut() else {
        return val;
    };
    obj.insert("title".to_string(), Value::String(current_def.to_string()));
    let is_object =
        obj.get("type").and_then(Value::as_str) == Some("object") || obj.contains_key("properties");
    if is_object && !obj.contains_key("additionalProperties") {
        obj.insert("additionalProperties".to_string(), Value::Bool(true));
    }
    val
}

/// Returns `true` when `schema` contains any `ucp_request` or `ucp_response` annotation
/// in schema position (ignoring instance data inside `const`, `enum`, `default`, `examples`).
pub fn has_directional_annotations(schema: &Value) -> bool {
    let mut found = false;
    for_each_schema_object(schema, &mut |obj| {
        if UCP_ANNOTATIONS.iter().any(|k| obj.contains_key(*k)) {
            found = true;
        }
    });
    found
}

/// Returns `true` when `schema` supports the `"complete"` request operation via `ucp_request`
/// annotations, `x-ucp-lifecycle`, or a `complete_request` entry in `$defs`.
pub fn schema_supports_complete(schema: &Value) -> bool {
    if schema
        .get("$defs")
        .and_then(Value::as_object)
        .is_some_and(|defs| {
            defs.keys().any(|k| {
                k == "complete_request"
                    || k == "CompleteRequest"
                    || k.ends_with(".complete_request")
                    || k.ends_with("_complete_request")
            })
        })
    {
        return true;
    }

    let mut found = false;
    for_each_schema_object(schema, &mut |obj| {
        if obj
            .get("x-ucp-lifecycle")
            .and_then(Value::as_array)
            .is_some_and(|lc| lc.iter().any(|v| v.as_str() == Some("complete")))
        {
            found = true;
        }
        if obj
            .get("ucp_request")
            .and_then(Value::as_object)
            .is_some_and(|req_map| req_map.contains_key("complete"))
        {
            found = true;
        }
    });
    found
}

/// Slice a UCP-annotated schema into directional request and response schemas:
/// `<BaseName>CreateRequest`, `<BaseName>UpdateRequest`, optional `<BaseName>CompleteRequest`
/// (when `schema_supports_complete(raw_schema)` is `true`), and `<BaseName>` (`Response, "read"`).
///
/// Note: Unlike container operation definitions (`$defs.<op>_request` / `$defs.<op>_response`),
/// which represent explicit RPC messages and are always emitted even when parameterless,
/// `slice_directional_schemas` also runs on nested sub-object value types (e.g.
/// `FulfillmentAvailableMethod`, `TimeInterval`) that mark every field `"ucp_request": "omit"`
/// because the sub-object is server-computed / response-only. Request slices where all fields
/// were omitted are skipped so response-only sub-objects do not emit unreferenced `*Request` types.
pub fn slice_directional_schemas(
    raw_schema: &Value,
    base_name: &str,
) -> Result<Vec<(String, Value)>, ResolveError> {
    let mut out = Vec::new();
    let supports_complete = schema_supports_complete(raw_schema);

    let mut req_ops = vec![("CreateRequest", "create"), ("UpdateRequest", "update")];
    if supports_complete {
        req_ops.push(("CompleteRequest", "complete"));
    }

    for (suffix, op) in req_ops {
        let resolved = resolve(raw_schema, &ResolveOptions::new(Direction::Request, op))?;
        let slice_name = format!("{base_name}{suffix}");
        let mut normalized = normalize_def_schema(&resolved, &slice_name, Some(base_name));
        if is_non_empty_request_slice(&normalized) {
            update_request_slice_metadata(&mut normalized, &slice_name, base_name, op);
            out.push((slice_name, normalized));
        }
    }

    let resp_resolved = resolve(
        raw_schema,
        &ResolveOptions::new(Direction::Response, "read"),
    )?;
    let resp_normalized = normalize_def_schema(&resp_resolved, base_name, Some(base_name));
    out.push((base_name.to_string(), resp_normalized));

    Ok(out)
}

/// Rewrite `#/$defs/<Target>` references inside a directional request schema slice
/// (`<Name>CreateRequest`, `<Name>UpdateRequest`, `<Name>CompleteRequest`) to point to
/// `#/$defs/<Target><suffix>` when `<Target><suffix>` is in `known_defs` (or to
/// `#/$defs/<Target>UpdateRequest` when `suffix == "CompleteRequest"` and
/// `<Target>UpdateRequest` is in `known_defs`).
pub fn align_directional_refs(schema_val: &mut Value, suffix: &str, known_defs: &BTreeSet<String>) {
    for_each_schema_object_mut(schema_val, &mut |obj| {
        let Some(Value::String(ref_str)) = obj.get("$ref") else {
            return;
        };
        let Some(target) = ref_str.strip_prefix("#/$defs/") else {
            return;
        };

        let candidate = format!("{target}{suffix}");
        if known_defs.contains(&candidate) {
            obj.insert(
                "$ref".to_string(),
                Value::String(format!("#/$defs/{candidate}")),
            );
        } else if suffix == "CompleteRequest" {
            let fallback = format!("{target}UpdateRequest");
            if known_defs.contains(&fallback) {
                obj.insert(
                    "$ref".to_string(),
                    Value::String(format!("#/$defs/{fallback}")),
                );
            }
        }
    });
}

fn is_non_empty_request_slice(val: &Value) -> bool {
    let Some(obj) = val.as_object() else {
        return false;
    };
    let has_props = obj
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|m| !m.is_empty());
    let has_composition = ["allOf", "oneOf", "anyOf"].iter().any(|k| {
        obj.get(*k)
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty())
    });
    let has_ref = obj.get("$ref").is_some_and(Value::is_string);
    has_props || has_composition || has_ref
}

fn update_request_slice_metadata(
    slice_val: &mut Value,
    slice_name: &str,
    base_name: &str,
    op: &str,
) {
    let Some(obj) = slice_val.as_object_mut() else {
        return;
    };
    obj.insert("title".to_string(), Value::String(slice_name.to_string()));
    let prefix = match op {
        "create" => format!("Request payload to create a new {base_name}."),
        "update" => format!("Request payload to update an existing {base_name}."),
        "complete" => format!("Request payload to complete a {base_name}."),
        _ => format!("Request payload for {base_name}."),
    };
    let new_desc = match obj.get("description").and_then(Value::as_str) {
        Some(existing) if !existing.is_empty() => format!("{prefix} {existing}"),
        _ => prefix,
    };
    obj.insert("description".to_string(), Value::String(new_desc));
}

fn file_stem_to_pascal(path_or_url: &str) -> String {
    let filename = path_or_url.rsplit('/').next().unwrap_or(path_or_url);
    let stem = filename
        .strip_suffix(".schema.json")
        .or_else(|| filename.strip_suffix(".json"))
        .unwrap_or(filename);
    to_pascal_case(stem)
}

fn inspect_allof_props(obj: &Map<String, Value>, props: &mut BTreeSet<String>) -> (bool, bool) {
    let mut has_props = false;
    if let Some(Value::Object(map)) = obj.get("properties") {
        props.extend(map.keys().cloned());
        has_props = true;
    }
    let mut has_ref = obj.get("$ref").is_some_and(Value::is_string);
    if let Some(Value::Array(all_of)) = obj.get("allOf") {
        for branch in all_of.iter().filter_map(Value::as_object) {
            let (bp, br) = inspect_allof_props(branch, props);
            has_props |= bp;
            has_ref |= br;
        }
    }
    (has_props, has_ref)
}

fn prune_dangling_required(
    obj: &mut Map<String, Value>,
    inherited_props: &BTreeSet<String>,
    inherited_has_ref: bool,
) {
    let mut effective_props = inherited_props.clone();
    let (local_has_props, local_has_ref) = inspect_allof_props(obj, &mut effective_props);
    let effective_has_ref = inherited_has_ref || local_has_ref;

    if let Some(Value::Array(reqs)) = obj.get_mut("required") {
        if local_has_props && !effective_has_ref {
            reqs.retain(|v| v.as_str().is_some_and(|k| effective_props.contains(k)));
        }
        if reqs.is_empty() {
            obj.remove("required");
        }
    }

    for key in ["allOf", "anyOf", "oneOf"] {
        let Some(Value::Array(arr)) = obj.get_mut(key) else {
            continue;
        };
        for branch in arr.iter_mut().filter_map(Value::as_object_mut) {
            prune_dangling_required(branch, &effective_props, effective_has_ref);
        }
    }
    for key in ["if", "then", "else", "not"] {
        if let Some(child) = obj.get_mut(key).and_then(Value::as_object_mut) {
            prune_dangling_required(child, &effective_props, effective_has_ref);
        }
    }
    if let Some(Value::Object(dep_schemas)) = obj.get_mut("dependentSchemas") {
        for child in dep_schemas.values_mut().filter_map(Value::as_object_mut) {
            prune_dangling_required(child, &effective_props, effective_has_ref);
        }
    }

    let empty_props = BTreeSet::new();
    for key in ["properties", "patternProperties", "$defs", "definitions"] {
        let Some(Value::Object(map)) = obj.get_mut(key) else {
            continue;
        };
        for child in map.values_mut().filter_map(Value::as_object_mut) {
            prune_dangling_required(child, &empty_props, false);
        }
    }
    for key in [
        "additionalProperties",
        "unevaluatedProperties",
        "items",
        "contains",
        "propertyNames",
    ] {
        if let Some(child) = obj.get_mut(key).and_then(Value::as_object_mut) {
            prune_dangling_required(child, &empty_props, false);
        }
    }
    if let Some(Value::Array(prefix_items)) = obj.get_mut("prefixItems") {
        for child in prefix_items.iter_mut().filter_map(Value::as_object_mut) {
            prune_dangling_required(child, &empty_props, false);
        }
    }
}

fn prune_empty_prop_stubs(obj: &mut Map<String, Value>) {
    let Some(Value::Object(props)) = obj.get_mut("properties") else {
        return;
    };
    for prop_obj in props.values_mut().filter_map(Value::as_object_mut) {
        prune_empty_prop_stubs(prop_obj);
    }
    props.retain(|_, v| v.as_object().is_none_or(|m| !m.is_empty()));
    if props.is_empty() {
        obj.remove("properties");
    }
}

fn clean_vacuous_anyof(obj: &mut Map<String, Value>) {
    if !obj.get("anyOf").is_some_and(Value::is_array) {
        return;
    }
    let parent_req: BTreeSet<String> = obj
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(String::from)
        .collect();
    let Some(Value::Array(any_of)) = obj.get_mut("anyOf") else {
        return;
    };
    for branch in any_of.iter_mut().filter_map(Value::as_object_mut) {
        prune_empty_prop_stubs(branch);
    }
    let is_tautological = any_of.iter().any(|b| {
        b.as_object().is_some_and(|m| {
            m.keys().all(|k| k == "required")
                && m.get("required")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .all(|r| r.as_str().is_some_and(|s| parent_req.contains(s)))
        })
    });
    if is_tautological {
        obj.remove("anyOf");
    }
}

fn distribute_bare_anyof_on_object(obj: &mut Map<String, Value>) {
    let has_bare_required_branch = obj
        .get("anyOf")
        .and_then(Value::as_array)
        .is_some_and(|arr| {
            arr.iter().any(|b| {
                b.as_object()
                    .is_some_and(|m| m.contains_key("required") && !m.contains_key("properties"))
            })
        });
    if !has_bare_required_branch {
        return;
    }

    let has_top_props = obj
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|m| !m.is_empty());
    let has_parent_refs = obj
        .get("allOf")
        .and_then(Value::as_array)
        .is_some_and(|arr| {
            arr.iter()
                .any(|b| b.get("$ref").is_some_and(Value::is_string))
        });
    if !(has_top_props || has_parent_refs) {
        return;
    }

    let parent_props = obj
        .remove("properties")
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default();
    let parent_reqs = obj
        .remove("required")
        .and_then(|v| match v {
            Value::Array(a) => Some(a),
            _ => None,
        })
        .unwrap_or_default();
    let add_props = obj
        .remove("additionalProperties")
        .unwrap_or(Value::Bool(true));
    obj.remove("type");

    let mut parent_ref_branches = Vec::new();
    if let Some(Value::Array(all_of)) = obj.get_mut("allOf") {
        let mut kept = Vec::new();
        for branch in std::mem::take(all_of) {
            if branch.get("$ref").is_some_and(Value::is_string) {
                parent_ref_branches.push(branch);
            } else {
                kept.push(branch);
            }
        }
        if kept.is_empty() {
            obj.remove("allOf");
        } else {
            *all_of = kept;
        }
    }

    let Some(Value::Array(any_of)) = obj.get_mut("anyOf") else {
        return;
    };
    for branch_obj in any_of.iter_mut().filter_map(Value::as_object_mut) {
        branch_obj.insert("type".to_string(), Value::String("object".to_string()));
        branch_obj
            .entry("additionalProperties".to_string())
            .or_insert_with(|| add_props.clone());

        if !parent_ref_branches.is_empty() {
            let branch_all_of = branch_obj
                .entry("allOf".to_string())
                .or_insert_with(|| Value::Array(Vec::new()));
            if let Value::Array(arr) = branch_all_of {
                let mut prepended = parent_ref_branches.clone();
                for existing in std::mem::take(arr) {
                    if !prepended.contains(&existing) {
                        prepended.push(existing);
                    }
                }
                *arr = prepended;
            }
        }

        let mut combined_reqs = parent_reqs.clone();
        if let Some(Value::Array(branch_reqs)) = branch_obj.remove("required") {
            for req in branch_reqs {
                if !combined_reqs.contains(&req) {
                    combined_reqs.push(req);
                }
            }
        }
        if !combined_reqs.is_empty() {
            branch_obj.insert("required".to_string(), Value::Array(combined_reqs));
        }

        let mut combined_props = parent_props.clone();
        if let Some(Value::Object(branch_props)) = branch_obj.remove("properties") {
            for (k, v) in branch_props {
                if let Some(base_prop) = combined_props.get_mut(&k) {
                    merge_schema_property(base_prop, &v);
                } else {
                    combined_props.insert(k, v);
                }
            }
        }
        if !combined_props.is_empty() {
            branch_obj.insert("properties".to_string(), Value::Object(combined_props));
        }
    }
}

fn flatten_single_object_allof(
    target: &mut Value,
    defs_snapshot: &BTreeMap<String, Value>,
) -> bool {
    let Some(obj) = target.as_object_mut() else {
        return false;
    };
    let Some(Value::Array(all_of)) = obj.get("allOf").cloned() else {
        return false;
    };

    let mut merged_props = Map::new();
    let mut merged_reqs: Vec<Value> = Vec::new();
    let mut kept_all_of: Vec<Value> = Vec::new();
    let mut any_flattened = false;

    for branch in all_of {
        if let Some(ref_name) = branch
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix("#/$defs/"))
        {
            if let Some(base_obj) = defs_snapshot
                .get(ref_name)
                .and_then(Value::as_object)
                .filter(|b| is_flattenable_base_object(b))
            {
                merge_props_and_reqs_from_object(base_obj, &mut merged_props, &mut merged_reqs);
                for cond_key in ["if", "then", "else"] {
                    if let Some(cond_val) = base_obj.get(cond_key) {
                        obj.entry(cond_key.to_string())
                            .or_insert_with(|| cond_val.clone());
                    }
                }
                if let Some(Value::Array(base_all_of)) = base_obj.get("allOf") {
                    for cond in base_all_of {
                        if !kept_all_of.contains(cond) {
                            kept_all_of.push(cond.clone());
                        }
                    }
                }
                any_flattened = true;
                continue;
            }
            kept_all_of.push(branch);
            continue;
        }

        let is_inline_flattenable = branch.as_object().is_some_and(|b| {
            ![
                "$ref", "if", "then", "else", "anyOf", "oneOf", "allOf", "contains",
            ]
            .iter()
            .any(|k| b.contains_key(*k))
        });
        if is_inline_flattenable {
            if let Some(branch_obj) = branch.as_object() {
                merge_props_and_reqs_from_object(branch_obj, &mut merged_props, &mut merged_reqs);
                any_flattened = true;
                continue;
            }
        }
        kept_all_of.push(branch);
    }

    if !any_flattened {
        return false;
    }

    if let Some(Value::Object(own_props)) = obj.remove("properties") {
        for (k, v) in own_props {
            if let Some(existing) = merged_props.get_mut(&k) {
                merge_schema_property(existing, &v);
            } else {
                merged_props.insert(k, v);
            }
        }
    }
    if let Some(Value::Array(own_reqs)) = obj.remove("required") {
        for req in own_reqs {
            if !merged_reqs.contains(&req) {
                merged_reqs.push(req);
            }
        }
    }

    if !obj.contains_key("anyOf") && !obj.contains_key("oneOf") {
        obj.insert("type".to_string(), Value::String("object".to_string()));
        obj.entry("additionalProperties".to_string())
            .or_insert(Value::Bool(true));
    }
    if !merged_props.is_empty() {
        obj.insert("properties".to_string(), Value::Object(merged_props));
    }
    if !merged_reqs.is_empty() {
        obj.insert("required".to_string(), Value::Array(merged_reqs));
    }
    if kept_all_of.is_empty() {
        obj.remove("allOf");
    } else {
        obj.insert("allOf".to_string(), Value::Array(kept_all_of));
    }
    true
}

fn is_flattenable_base_object(base_obj: &Map<String, Value>) -> bool {
    let is_obj = base_obj.get("type").and_then(Value::as_str) == Some("object")
        || base_obj.contains_key("properties");
    if !is_obj {
        return false;
    }
    if ["anyOf", "oneOf", "$ref"]
        .iter()
        .any(|k| base_obj.contains_key(*k))
    {
        return false;
    }
    if base_obj
        .get("then")
        .and_then(Value::as_object)
        .is_some_and(|t| t.contains_key("$ref"))
    {
        return false;
    }
    let Some(Value::Array(all_of)) = base_obj.get("allOf") else {
        return true;
    };
    all_of.iter().all(|b| {
        b.get("if").is_some()
            && !b
                .get("then")
                .and_then(Value::as_object)
                .is_some_and(|t| t.contains_key("$ref"))
    })
}

fn merge_props_and_reqs_from_object(
    source: &Map<String, Value>,
    merged_props: &mut Map<String, Value>,
    merged_reqs: &mut Vec<Value>,
) {
    if let Some(Value::Object(props)) = source.get("properties") {
        for (k, v) in props {
            if let Some(existing) = merged_props.get_mut(k) {
                merge_schema_property(existing, v);
            } else {
                merged_props.insert(k.clone(), v.clone());
            }
        }
    }
    if let Some(Value::Array(reqs)) = source.get("required") {
        for req in reqs {
            if !merged_reqs.contains(req) {
                merged_reqs.push(req.clone());
            }
        }
    }
}

pub(crate) fn merge_schema_property(base_prop: &mut Value, override_prop: &Value) {
    let (Some(base_obj), Some(override_obj)) =
        (base_prop.as_object_mut(), override_prop.as_object())
    else {
        *base_prop = override_prop.clone();
        return;
    };
    if override_obj.contains_key("$ref") {
        *base_prop = override_prop.clone();
        return;
    }
    if base_obj.contains_key("$ref") {
        if override_obj.contains_key("const")
            || override_obj.contains_key("enum")
            || override_obj.contains_key("type")
        {
            *base_prop = override_prop.clone();
        }
        return;
    }
    if override_obj.contains_key("const") {
        base_obj.remove("enum");
    }
    for (k, v) in override_obj {
        if let Some(existing) = base_obj.get_mut(k) {
            if existing.is_object() && v.is_object() {
                merge_schema_property(existing, v);
                continue;
            }
        }
        base_obj.insert(k.clone(), v.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn to_pascal_case_converts_snake_kebab_and_reverse_domain() {
        assert_eq!(to_pascal_case("checkout"), "Checkout");
        assert_eq!(to_pascal_case("line_item"), "LineItem");
        assert_eq!(to_pascal_case("order-line-item"), "OrderLineItem");
        assert_eq!(to_pascal_case("dev.ucp.shopping.checkout"), "Checkout");
        assert_eq!(
            to_pascal_case("dev.ucp.shopping.buyer_consent"),
            "BuyerConsent"
        );
        assert_eq!(
            to_pascal_case("checkout.complete_request"),
            "CheckoutCompleteRequest"
        );
        assert_eq!(to_pascal_case("PlatformSchema"), "PlatformSchema");
        assert_eq!(to_pascal_case("a_b"), "AB");
        assert_eq!(to_pascal_case("AB"), "AB");
        assert_eq!(to_pascal_case("v1_v2"), "V1V2");
        assert_eq!(to_pascal_case("V1V2"), "V1V2");
    }

    #[test]
    fn is_reverse_domain_name_identifies_capability_names() {
        assert!(is_reverse_domain_name("dev.ucp.shopping.checkout"));
        assert!(is_reverse_domain_name("dev.shopify.catalog"));
        assert!(is_reverse_domain_name("dev.ucp.shopping.catalog.search"));
        assert!(!is_reverse_domain_name("checkout"));
        assert!(!is_reverse_domain_name("shopping.checkout"));
        assert!(!is_reverse_domain_name("rest.openapi.json"));
        assert!(!is_reverse_domain_name(
            "https://ucp.dev/schemas/checkout.json"
        ));
    }

    #[test]
    fn is_generic_def_name_matches_spec_list() {
        for generic in [
            "Base",
            "base",
            "Entity",
            "entity",
            "PlatformSchema",
            "platform_schema",
            "BusinessSchema",
            "business_schema",
            "ResponseSchema",
            "response_schema",
            "Request",
            "request",
            "Response",
            "response",
            "Error",
            "error",
            "Config",
            "config",
            "Endpoint",
            "endpoint",
            "Quantity",
            "quantity",
        ] {
            assert!(
                is_generic_def_name(generic),
                "expected {generic} to be generic"
            );
        }
        assert!(!is_generic_def_name("jwk_public_key"));
        assert!(!is_generic_def_name("lookup_variant"));
        assert!(!is_generic_def_name("discounts_object"));
    }

    #[test]
    fn qualify_def_name_qualifies_generic_and_preserves_specific_names() {
        assert_eq!(
            qualify_def_name("Capability", "platform_schema"),
            "CapabilityPlatformSchema"
        );
        assert_eq!(
            qualify_def_name("OrderLineItem", "quantity"),
            "OrderLineItemQuantity"
        );
        assert_eq!(
            qualify_def_name("Pagination", "request"),
            "PaginationRequest"
        );
        assert_eq!(
            qualify_def_name("Permalink", "endpoint"),
            "PermalinkEndpoint"
        );
        assert_eq!(qualify_def_name("Permalink", "config"), "PermalinkConfig");
        assert_eq!(
            qualify_def_name("CatalogLookup", "lookup_variant"),
            "LookupVariant"
        );
        assert_eq!(
            qualify_def_name("Profile", "jwk_public_key"),
            "JwkPublicKey"
        );
        assert_eq!(
            qualify_def_name("CatalogSearch", "search_request"),
            "CatalogSearchRequest"
        );
        assert_eq!(
            qualify_def_name("OrderManage", "cancel_request"),
            "OrderCancelRequest"
        );
        assert_eq!(
            qualify_def_name("Checkout", "complete_request"),
            "CompleteRequest"
        );
    }

    #[test]
    fn qualify_container_op_name_handles_shared_and_distinct_prefixes() {
        assert_eq!(
            qualify_container_op_name("CatalogSearch", "search_request"),
            "CatalogSearchRequest"
        );
        assert_eq!(
            qualify_container_op_name("CatalogSearch", "search_response"),
            "CatalogSearchResponse"
        );
        assert_eq!(
            qualify_container_op_name("CatalogLookup", "lookup_request"),
            "CatalogLookupRequest"
        );
        assert_eq!(
            qualify_container_op_name("CatalogLookup", "get_product_request"),
            "CatalogGetProductRequest"
        );
        assert_eq!(
            qualify_container_op_name("CatalogLookup", "get_product_response"),
            "CatalogGetProductResponse"
        );
        assert_eq!(
            qualify_container_op_name("LocationSearch", "location_search_request"),
            "LocationSearchRequest"
        );
        assert_eq!(
            qualify_container_op_name("LocationSearch", "search_request"),
            "LocationSearchRequest"
        );
    }

    #[test]
    fn ref_to_def_name_resolves_self_internal_and_external_refs() {
        assert_eq!(
            ref_to_def_name("#", Some("PaymentInstrument")),
            "PaymentInstrument"
        );
        assert_eq!(
            ref_to_def_name("#/", Some("PaymentInstrument")),
            "PaymentInstrument"
        );
        assert_eq!(ref_to_def_name("#/", None), "Self");
        assert_eq!(
            ref_to_def_name("#/$defs/base", Some("Profile")),
            "ProfileBase"
        );
        assert_eq!(
            ref_to_def_name("#/$defs/Quantity", Some("Checkout")),
            "Quantity"
        );
        assert_eq!(
            ref_to_def_name("#/$defs/jwk_public_key", Some("Profile")),
            "JwkPublicKey"
        );
        assert_eq!(
            ref_to_def_name("../capability.json#/$defs/platform_schema", Some("Order")),
            "CapabilityPlatformSchema"
        );
        assert_eq!(
            ref_to_def_name(
                "../common/types/pagination.json#/$defs/request",
                Some("CatalogSearch")
            ),
            "PaginationRequest"
        );
        assert_eq!(
            ref_to_def_name(
                "catalog_lookup.json#/$defs/get_product_response",
                Some("Fulfillment")
            ),
            "CatalogGetProductResponse"
        );
        assert_eq!(
            ref_to_def_name("types/line_item.json", Some("Checkout")),
            "LineItem"
        );
        assert_eq!(
            ref_to_def_name(
                "https://ucp.dev/draft/schemas/common/types/error_response.json",
                None
            ),
            "ErrorResponse"
        );
    }

    #[test]
    fn strip_ucp_keywords_removes_annotations_and_preserves_instance_data() {
        let mut schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://ucp.dev/schemas/shopping/checkout.json",
            "name": "dev.ucp.shopping.checkout",
            "version": "2026-01-11",
            "requires": { "protocol": { "min": "2026-01-11" } },
            "embedded": { "methods": {} },
            "type": "object",
            "ucp_shared_request": true,
            "x-ucp-schema-transition": { "from": "optional", "to": "omit" },
            "required": ["id", "omitted_field"],
            "properties": {
                "id": {
                    "type": "string",
                    "ucp_request": "omit",
                    "ucp_response": "required",
                    "default": {
                        "ucp_request": "keep_inside_default",
                        "$schema": "keep_inside_default"
                    }
                },
                "name": {
                    "type": "string"
                },
                "title": {
                    "type": "string"
                }
            },
            "allOf": [
                {
                    "title": "EC keys carry crv, x, y",
                    "if": { "properties": { "name": { "const": "EC" } } },
                    "then": { "required": ["id"] }
                }
            ]
        });

        strip_ucp_keywords(&mut schema);

        assert!(schema.get("$schema").is_none());
        assert!(schema.get("$id").is_none());
        assert!(schema.get("name").is_none());
        assert!(schema.get("version").is_none());
        assert!(schema.get("requires").is_none());
        assert!(schema.get("embedded").is_none());
        assert!(schema.get("ucp_shared_request").is_none());
        assert!(schema.get("x-ucp-schema-transition").is_none());
        assert_eq!(schema["required"], json!(["id"]));
        assert!(schema["properties"]["id"].get("ucp_request").is_none());
        assert!(schema["properties"]["id"].get("ucp_response").is_none());
        assert_eq!(
            schema["properties"]["id"]["default"]["ucp_request"],
            "keep_inside_default"
        );
        assert!(schema["properties"].get("name").is_some());
        assert!(schema["properties"].get("title").is_some());
        assert!(schema["allOf"][0].get("title").is_none());
    }

    #[test]
    fn strip_ucp_keywords_preserves_required_when_ref_present_and_removes_empty_required() {
        let mut with_ref = json!({
            "type": "object",
            "allOf": [{ "allOf": [{ "$ref": "#/$defs/Base" }] }],
            "properties": {
                "own_field": { "type": "string" }
            },
            "required": ["own_field", "inherited_field"]
        });
        strip_ucp_keywords(&mut with_ref);
        assert_eq!(
            with_ref["required"],
            json!(["own_field", "inherited_field"])
        );

        let mut with_nested_allof_props = json!({
            "allOf": [
                {
                    "allOf": [
                        { "properties": { "nested_prop": { "type": "string" } } }
                    ]
                }
            ],
            "required": ["nested_prop", "omitted_prop"],
            "anyOf": [
                { "required": ["eq"] },
                { "required": ["in"] }
            ]
        });
        strip_ucp_keywords(&mut with_nested_allof_props);
        assert_eq!(with_nested_allof_props["required"], json!(["nested_prop"]));
        assert_eq!(
            with_nested_allof_props["anyOf"][0]["required"],
            json!(["eq"])
        );

        let mut empty_req = json!({
            "type": "object",
            "properties": {},
            "required": ["omitted_field"]
        });
        strip_ucp_keywords(&mut empty_req);
        assert!(empty_req.get("required").is_none());

        // Nested object schemas (inside properties and items) prune dangling required entries
        let mut nested_dangling = json!({
            "type": "object",
            "properties": {
                "child": {
                    "type": "object",
                    "properties": {
                        "kept": { "type": "string" }
                    },
                    "required": ["kept", "omitted_child_prop"]
                },
                "list": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {},
                        "required": ["all_omitted"]
                    }
                }
            }
        });
        strip_ucp_keywords(&mut nested_dangling);
        assert_eq!(
            nested_dangling["properties"]["child"]["required"],
            json!(["kept"])
        );
        assert!(nested_dangling["properties"]["list"]["items"]
            .get("required")
            .is_none());

        // Nested anyOf inside allOf (ServicePlatformSchema pattern) preserves inherited required fields
        let mut service_platform = json!({
            "allOf": [
                { "$ref": "#/$defs/ServiceBase" },
                {
                    "anyOf": [
                        {
                            "properties": { "transport": { "const": "rest" } },
                            "required": ["schema"]
                        }
                    ]
                }
            ]
        });
        strip_ucp_keywords(&mut service_platform);
        assert_eq!(
            service_platform["allOf"][1]["anyOf"][0]["required"],
            json!(["schema"])
        );

        // Redundant "type" alongside "$ref" (MembershipReward pattern) is removed
        let mut ref_with_type = json!({
            "type": "object",
            "properties": {
                "currency": {
                    "type": "object",
                    "$ref": "#/$defs/RewardCurrency",
                    "description": "Currency."
                }
            }
        });
        strip_ucp_keywords(&mut ref_with_type);
        assert!(ref_with_type["properties"]["currency"]
            .get("type")
            .is_none());

        // Vacuous {} property stubs in anyOf (Stay response & StayCreateRequest patterns) are pruned
        let mut stay_resp = json!({
            "type": "object",
            "required": ["id", "accommodation_type", "rate_plan"],
            "anyOf": [
                { "properties": { "id": { "ucp_request": { "create": "required" } } } },
                {
                    "required": ["accommodation_type", "rate_plan"],
                    "properties": {
                        "accommodation_type": {
                            "properties": { "id": { "ucp_request": { "create": "required" } } }
                        }
                    }
                }
            ]
        });
        strip_ucp_keywords(&mut stay_resp);
        assert!(stay_resp.get("anyOf").is_none());

        let mut stay_req = json!({
            "type": "object",
            "required": ["stay_dates"],
            "anyOf": [
                {
                    "properties": { "id": {} },
                    "required": ["id"]
                },
                {
                    "properties": {
                        "accommodation_type": {
                            "properties": { "id": {} },
                            "required": ["id"]
                        }
                    },
                    "required": ["accommodation_type"]
                }
            ]
        });
        strip_ucp_keywords(&mut stay_req);
        assert_eq!(
            stay_req["anyOf"],
            json!([
                { "required": ["id"] },
                {
                    "properties": {
                        "accommodation_type": { "required": ["id"] }
                    },
                    "required": ["accommodation_type"]
                }
            ])
        );
    }

    #[test]
    fn rewrite_refs_to_defs_rewrites_schema_refs_and_skips_instance_data() {
        let mut schema = json!({
            "type": "object",
            "allOf": [
                { "$ref": "../capability.json#/$defs/platform_schema" },
                { "$ref": "#/$defs/base" },
                { "$ref": "#/$defs/jwk_public_key" }
            ],
            "properties": {
                "parent": { "$ref": "#" },
                "root_ptr": { "$ref": "#/" },
                "item": { "$ref": "types/line_item.json" },
                "qty": { "$ref": "types/quantity.json" },
                "example_payload": {
                    "type": "object",
                    "examples": [
                        { "$ref": "should/not/be/rewritten.json" }
                    ],
                    "const": { "$ref": "#" }
                }
            }
        });

        rewrite_refs_to_defs(&mut schema, "Profile", Some("Profile"));

        assert_eq!(
            schema["allOf"][0]["$ref"],
            "#/$defs/CapabilityPlatformSchema"
        );
        assert_eq!(schema["allOf"][1]["$ref"], "#/$defs/ProfileBase");
        assert_eq!(schema["allOf"][2]["$ref"], "#/$defs/JwkPublicKey");
        assert_eq!(schema["properties"]["parent"]["$ref"], "#/$defs/Profile");
        assert_eq!(schema["properties"]["root_ptr"]["$ref"], "#/$defs/Profile");
        assert_eq!(schema["properties"]["item"]["$ref"], "#/$defs/LineItem");
        assert_eq!(schema["properties"]["qty"]["$ref"], "#/$defs/Quantity");
        assert_eq!(
            schema["properties"]["example_payload"]["examples"][0]["$ref"],
            "should/not/be/rewritten.json"
        );
        assert_eq!(
            schema["properties"]["example_payload"]["const"]["$ref"],
            "#"
        );

        // Idempotence check: running a second time preserves already-rewritten refs (including #/$defs/Quantity)
        let once = schema.clone();
        rewrite_refs_to_defs(&mut schema, "Profile", Some("Profile"));
        assert_eq!(schema, once);
    }

    #[test]
    fn normalize_def_schema_strips_rewrites_and_defaults_additional_properties() {
        let raw_obj = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "Buyer Object",
            "name": "dev.ucp.shopping.buyer",
            "version": "2026-01-11",
            "type": "object",
            "properties": {
                "id": { "type": "string", "ucp_request": "omit" },
                "address": { "$ref": "types/postal_address.json" }
            }
        });

        let normalized = normalize_def_schema(&raw_obj, "Buyer", Some("Buyer"));
        assert!(normalized.get("$schema").is_none());
        assert!(normalized.get("name").is_none());
        assert!(normalized.get("version").is_none());
        assert_eq!(normalized["title"], "Buyer");
        assert!(normalized["properties"]["id"].get("ucp_request").is_none());
        assert_eq!(
            normalized["properties"]["address"]["$ref"],
            "#/$defs/PostalAddress"
        );
        assert_eq!(normalized["additionalProperties"], Value::Bool(true));

        // Explicit additionalProperties is preserved
        let closed_obj = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "code": { "type": "string" } }
        });
        let normalized_closed = normalize_def_schema(&closed_obj, "Closed", None);
        assert_eq!(normalized_closed["title"], "Closed");
        assert_eq!(
            normalized_closed["additionalProperties"],
            Value::Bool(false)
        );

        // Schema with properties but no explicit "type": "object" receives additionalProperties: true
        let implicit_obj = json!({
            "properties": { "name": { "type": "string" } }
        });
        let normalized_implicit = normalize_def_schema(&implicit_obj, "Implicit", None);
        assert_eq!(normalized_implicit["title"], "Implicit");
        assert_eq!(
            normalized_implicit["additionalProperties"],
            Value::Bool(true)
        );

        // Non-object schemas do not receive additionalProperties, but still synchronize title
        let string_schema = json!({
            "type": "string",
            "enum": ["pending", "completed"]
        });
        let normalized_str = normalize_def_schema(&string_schema, "Status", None);
        assert_eq!(normalized_str["title"], "Status");
        assert!(normalized_str.get("additionalProperties").is_none());
    }

    #[test]
    fn has_directional_annotations_and_schema_supports_complete_skip_instance_data() {
        let plain = json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "default": {
                        "ucp_request": { "complete": "required" },
                        "x-ucp-lifecycle": ["complete"]
                    }
                }
            }
        });
        assert!(!has_directional_annotations(&plain));
        assert!(!schema_supports_complete(&plain));

        let with_req = json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "ucp_request": { "create": "omit", "update": "required" }
                }
            }
        });
        assert!(has_directional_annotations(&with_req));
        assert!(!schema_supports_complete(&with_req));

        let with_complete_req = json!({
            "type": "object",
            "properties": {
                "payment": {
                    "type": "string",
                    "ucp_request": { "complete": "required" }
                }
            }
        });
        assert!(has_directional_annotations(&with_complete_req));
        assert!(schema_supports_complete(&with_complete_req));

        let with_lifecycle = json!({
            "type": "object",
            "x-ucp-lifecycle": ["create", "update", "complete"],
            "properties": {}
        });
        assert!(schema_supports_complete(&with_lifecycle));

        let with_defs_complete = json!({
            "type": "object",
            "$defs": {
                "checkout.complete_request": { "type": "object" }
            }
        });
        assert!(schema_supports_complete(&with_defs_complete));
    }

    #[test]
    fn slice_directional_schemas_emits_request_and_response_slices_and_omits_empty_requests() {
        let checkout_schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "Checkout",
            "description": "Checkout session.",
            "type": "object",
            "required": ["id", "line_items"],
            "properties": {
                "id": {
                    "type": "string",
                    "ucp_request": {
                        "create": "omit",
                        "update": "required",
                        "complete": "omit"
                    }
                },
                "line_items": {
                    "type": "array",
                    "items": { "$ref": "types/line_item.json" },
                    "ucp_request": {
                        "create": "required",
                        "update": "optional",
                        "complete": "omit"
                    }
                },
                "payment_token": {
                    "type": "string",
                    "ucp_request": {
                        "create": "omit",
                        "update": "omit",
                        "complete": "required"
                    },
                    "ucp_response": "omit"
                }
            }
        });

        let slices = slice_directional_schemas(&checkout_schema, "Checkout").unwrap();
        let names: Vec<&str> = slices.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "CheckoutCreateRequest",
                "CheckoutUpdateRequest",
                "CheckoutCompleteRequest",
                "Checkout"
            ]
        );

        let create_slice = &slices[0].1;
        assert_eq!(create_slice["title"], "CheckoutCreateRequest");
        assert_eq!(
            create_slice["description"],
            "Request payload to create a new Checkout. Checkout session."
        );
        assert!(create_slice["properties"].get("id").is_none());
        assert_eq!(
            create_slice["properties"]["line_items"]["items"]["$ref"],
            "#/$defs/LineItem"
        );
        assert_eq!(create_slice["required"], json!(["line_items"]));
        assert_eq!(create_slice["additionalProperties"], Value::Bool(true));

        let update_slice = &slices[1].1;
        assert_eq!(update_slice["title"], "CheckoutUpdateRequest");
        assert_eq!(
            update_slice["description"],
            "Request payload to update an existing Checkout. Checkout session."
        );
        assert_eq!(update_slice["required"], json!(["id"]));

        let complete_slice = &slices[2].1;
        assert_eq!(complete_slice["title"], "CheckoutCompleteRequest");
        assert_eq!(
            complete_slice["description"],
            "Request payload to complete a Checkout. Checkout session."
        );
        assert_eq!(complete_slice["required"], json!(["payment_token"]));

        let resp_slice = &slices[3].1;
        assert_eq!(resp_slice["title"], "Checkout");
        assert_eq!(resp_slice["description"], "Checkout session.");
        assert!(resp_slice["properties"].get("payment_token").is_none());
        assert_eq!(resp_slice["required"], json!(["id", "line_items"]));

        // Schema where all properties are omitted on requests omits request slices
        let response_only = json!({
            "title": "Adjustment",
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {
                    "type": "string",
                    "ucp_request": "omit"
                }
            }
        });
        let ro_slices = slice_directional_schemas(&response_only, "Adjustment").unwrap();
        assert_eq!(ro_slices.len(), 1);
        assert_eq!(ro_slices[0].0, "Adjustment");

        // Schema without description receives default request description without trailing space,
        // and internal #/$defs/base is qualified with base_name ("ItemBase"), not slice_name
        let no_desc = json!({
            "type": "object",
            "properties": {
                "base_info": {
                    "$ref": "#/$defs/base",
                    "ucp_request": { "create": "required", "update": "omit" }
                }
            }
        });
        let nd_slices = slice_directional_schemas(&no_desc, "Item").unwrap();
        assert_eq!(
            nd_slices[0].1["description"],
            "Request payload to create a new Item."
        );
        assert_eq!(
            nd_slices[0].1["properties"]["base_info"]["$ref"],
            "#/$defs/ItemBase"
        );
    }

    #[test]
    fn align_directional_refs_rewrites_known_targets_and_falls_back_for_complete_request() {
        let known_defs: BTreeSet<String> = [
            "LineItem",
            "LineItemCreateRequest",
            "LineItemUpdateRequest",
            "Fulfillment",
            "FulfillmentCreateRequest",
            "FulfillmentUpdateRequest",
            "FulfillmentCompleteRequest",
            "Money",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        let base_slice = json!({
            "type": "object",
            "properties": {
                "line_items": {
                    "type": "array",
                    "items": { "$ref": "#/$defs/LineItem" }
                },
                "fulfillment": { "$ref": "#/$defs/Fulfillment" },
                "total": { "$ref": "#/$defs/Money" },
                "example": {
                    "type": "object",
                    "default": { "$ref": "#/$defs/LineItem" }
                }
            }
        });

        let mut create_slice = base_slice.clone();
        align_directional_refs(&mut create_slice, "CreateRequest", &known_defs);
        assert_eq!(
            create_slice["properties"]["line_items"]["items"]["$ref"],
            "#/$defs/LineItemCreateRequest"
        );
        assert_eq!(
            create_slice["properties"]["fulfillment"]["$ref"],
            "#/$defs/FulfillmentCreateRequest"
        );
        assert_eq!(create_slice["properties"]["total"]["$ref"], "#/$defs/Money");
        assert_eq!(
            create_slice["properties"]["example"]["default"]["$ref"],
            "#/$defs/LineItem"
        );

        let mut complete_slice = base_slice;
        align_directional_refs(&mut complete_slice, "CompleteRequest", &known_defs);
        // LineItem has no CompleteRequest, so it falls back to LineItemUpdateRequest
        assert_eq!(
            complete_slice["properties"]["line_items"]["items"]["$ref"],
            "#/$defs/LineItemUpdateRequest"
        );
        // Fulfillment has FulfillmentCompleteRequest, so it uses the direct match
        assert_eq!(
            complete_slice["properties"]["fulfillment"]["$ref"],
            "#/$defs/FulfillmentCompleteRequest"
        );
        assert_eq!(
            complete_slice["properties"]["total"]["$ref"],
            "#/$defs/Money"
        );
    }

    #[test]
    fn distribute_properties_in_bare_anyof_preserves_ref_isolation_and_parent_refs() {
        let mut stay_req = json!({
            "type": "object",
            "required": ["stay_dates"],
            "allOf": [{ "$ref": "#/$defs/AccommodationContext" }],
            "properties": {
                "id": { "type": "string" },
                "stay_dates": { "$ref": "#/$defs/DateInterval" },
                "accommodation_type": { "$ref": "#/$defs/AccommodationTypeCreateRequest" },
                "rate_plan": { "$ref": "#/$defs/RatePlanCreateRequest" }
            },
            "anyOf": [
                { "required": ["id"] },
                {
                    "required": ["accommodation_type", "rate_plan"],
                    "properties": {
                        "accommodation_type": { "required": ["id"] },
                        "rate_plan": { "required": ["id"] }
                    }
                }
            ]
        });

        distribute_properties_in_bare_anyof(&mut stay_req);

        assert!(stay_req.get("properties").is_none());
        assert!(stay_req.get("type").is_none());
        assert!(stay_req.get("allOf").is_none());

        let b0 = &stay_req["anyOf"][0];
        assert_eq!(b0["type"], "object");
        assert_eq!(b0["required"], json!(["stay_dates", "id"]));
        assert_eq!(
            b0["allOf"],
            json!([{ "$ref": "#/$defs/AccommodationContext" }])
        );

        let b1 = &stay_req["anyOf"][1];
        assert_eq!(
            b1["required"],
            json!(["stay_dates", "accommodation_type", "rate_plan"])
        );
        // Base $ref property is kept isolated without sibling "required" keys (invariant C6)
        assert_eq!(
            b1["properties"]["accommodation_type"],
            json!({ "$ref": "#/$defs/AccommodationTypeCreateRequest" })
        );
    }

    #[test]
    fn normalize_scalar_or_array_unions_and_attach_const_defaults_handle_guards() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "extends": {
                    "oneOf": [
                        { "$ref": "#/$defs/ReverseDomainName" },
                        {
                            "type": "array",
                            "items": { "$ref": "#/$defs/ReverseDomainName" },
                            "minItems": 1
                        }
                    ],
                    "description": "Parent capabilities."
                },
                "status": { "const": "success" },
                "const": {},
                "enum": { "type": "array" },
                "open_type": {
                    "type": "string",
                    "not": { "enum": ["only_one"] }
                }
            },
            "allOf": [
                {
                    "if": { "properties": { "status": { "const": "error" } } },
                    "then": { "properties": { "code": { "const": 400 } } }
                }
            ]
        });

        normalize_scalar_or_array_unions(&mut schema);
        attach_const_defaults(&mut schema);

        assert!(schema["properties"]["extends"].get("oneOf").is_none());
        assert_eq!(schema["properties"]["extends"]["type"], "array");
        assert_eq!(schema["properties"]["extends"]["minItems"], 1);
        assert_eq!(
            schema["properties"]["extends"]["items"]["$ref"],
            "#/$defs/ReverseDomainName"
        );

        assert_eq!(
            schema["properties"]["status"],
            json!({ "type": "string", "const": "success", "default": "success" })
        );
        assert_eq!(schema["properties"]["const"], json!({}));
        assert!(schema["properties"]["open_type"]["not"]
            .get("default")
            .is_none());
        assert!(schema["allOf"][0]["if"]["properties"]["status"]
            .get("default")
            .is_none());
        assert!(schema["allOf"][0]["then"]["properties"]["code"]
            .get("default")
            .is_none());
    }

    #[test]
    fn flatten_object_allof_flattens_inheritance_and_array_items_while_preserving_if_then() {
        let mut defs = BTreeMap::from([
            (
                "Total".to_string(),
                json!({
                    "title": "Total",
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
                    ],
                    "additionalProperties": true
                }),
            ),
            (
                "Totals".to_string(),
                json!({
                    "title": "Totals",
                    "type": "array",
                    "items": {
                        "allOf": [
                            { "$ref": "#/$defs/Total" },
                            {
                                "type": "object",
                                "properties": { "lines": { "type": "array" } }
                            }
                        ]
                    },
                    "allOf": [
                        { "contains": { "properties": { "type": { "const": "total" } } } }
                    ]
                }),
            ),
            (
                "UnionParent".to_string(),
                json!({
                    "title": "UnionParent",
                    "type": "object",
                    "properties": { "type": { "type": "string" } },
                    "allOf": [
                        {
                            "if": { "properties": { "type": { "const": "a" } } },
                            "then": { "$ref": "#/$defs/VariantA" }
                        }
                    ]
                }),
            ),
            (
                "SubtypeChild".to_string(),
                json!({
                    "title": "SubtypeChild",
                    "allOf": [
                        { "$ref": "#/$defs/UnionParent" },
                        {
                            "type": "object",
                            "required": ["code"],
                            "properties": { "code": { "type": "string" } }
                        }
                    ]
                }),
            ),
        ]);

        flatten_object_allof(&mut defs);

        assert!(defs["Totals"].get("allOf").is_none());
        assert_eq!(defs["Totals"]["items"]["type"], "object");
        assert!(defs["Totals"]["items"]["properties"]
            .get("amount")
            .is_some());
        assert!(defs["Totals"]["items"]["properties"].get("lines").is_some());
        assert_eq!(
            defs["Totals"]["items"]["allOf"].as_array().unwrap().len(),
            1
        );

        // UnionParent has then.$ref, so SubtypeChild keeps $ref to UnionParent in allOf while inline branch flattens
        assert_eq!(
            defs["SubtypeChild"]["allOf"],
            json!([{ "$ref": "#/$defs/UnionParent" }])
        );
        assert_eq!(
            defs["SubtypeChild"]["properties"]["code"],
            json!({ "type": "string" })
        );
    }
}
