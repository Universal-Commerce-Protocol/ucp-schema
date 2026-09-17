//! Ambient UCP protocol-namespace materialization.
//!
//! Makes implicit `ucp` namespaces explicit before operation resolution.

use serde_json::{Map, Value};
use url::Url;

use crate::error::ResolveError;
use crate::resolver;
use crate::types::ResolveOptions;

const AMBIENT_UCP_MEMBERS_DEF_KEY: &str = "__ucp_ambient_members";

/// Resolve a schema with ambient UCP protocol-namespace materialization.
///
/// `members` is the central `ucp` namespace member registry. It must already be
/// self-contained/bundled: the helper is detached from its original resource and
/// installed under the operation schema root `$defs`, so local `#/$defs/...`
/// references inside `members` would resolve against the operation root.
///
/// The operation schema root must carry an absolute `$id`. Injected helper refs
/// are absolute URI references derived from that root resource, with a
/// collision-free helper `$defs` key.
///
/// # Errors
///
/// Returns `ResolveError` for non-object or non-absolute roots, non-object root
/// `$defs`, unbundled members, missing helpers, or invalid UCP annotations.
pub fn resolve_with_ucp_members(
    schema: &Value,
    members: &Value,
    options: &ResolveOptions,
) -> Result<Value, ResolveError> {
    let context = AmbientContext::new(schema)?;
    reject_local_member_refs(members)?;
    let registered_members = registered_member_names(members)?;
    let mut materialized = schema.clone();
    materialize_schema(&mut materialized, Scope::Normal, &context);
    let mut helper = members.clone();
    materialize_schema(&mut helper, Scope::DirectNamespace, &context);
    install_members_helper(&mut materialized, &context.helper_key, helper)?;
    let mut resolved = resolver::resolve(&materialized, options)?;
    mark_omitted_registered_members_false(&mut resolved, &registered_members, &context.helper_key)?;
    Ok(resolved)
}

struct AmbientContext {
    helper_key: String,
    helper_ref: String,
}

impl AmbientContext {
    fn new(schema: &Value) -> Result<Self, ResolveError> {
        let root = schema.as_object().ok_or_else(|| {
            invalid("ambient UCP materialization requires object root with absolute $id")
        })?;
        let id = root
            .get("$id")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("ambient UCP materialization requires an absolute root $id"))?;
        let mut root_uri = Url::parse(id).map_err(|source| {
            invalid(format!("ambient UCP root $id must be absolute: {source}"))
        })?;
        let helper_key = allocate_helper_key(root)?;
        root_uri.set_fragment(Some(&format!("/$defs/{helper_key}")));
        Ok(Self {
            helper_ref: root_uri.to_string(),
            helper_key,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Normal,
    /// Direct value of a reserved `ucp` protocol namespace. Same-instance
    /// applicators stay in this scope so `ucp.ucp` remains prohibited through
    /// composition and conditionals. Child-instance schemas return to normal
    /// materialization.
    DirectNamespace,
}

fn materialize_schema(schema: &mut Value, scope: Scope, context: &AmbientContext) {
    match schema {
        Value::Object(map) => materialize_object(map, scope, context),
        Value::Bool(true) if scope == Scope::DirectNamespace => {
            let mut map = Map::new();
            mark_direct_namespace(&mut map);
            *schema = Value::Object(map);
        }
        Value::Array(items) => {
            for item in items {
                materialize_schema(item, Scope::Normal, context);
            }
        }
        _ => {}
    }
}

fn materialize_object(map: &mut Map<String, Value>, scope: Scope, context: &AmbientContext) {
    for (key, value) in map.iter_mut() {
        match key.as_str() {
            "properties" => materialize_properties(value, scope, context),
            "$defs" | "definitions" | "patternProperties" => {
                materialize_schema_map(value, Scope::Normal, context);
            }
            "dependentSchemas" => materialize_schema_map(value, scope, context),
            "allOf" | "anyOf" | "oneOf" => materialize_schema_array(value, scope, context),
            "if" | "then" | "else" | "not" => materialize_schema(value, scope, context),
            "additionalProperties"
            | "unevaluatedProperties"
            | "propertyNames"
            | "items"
            | "contains"
            | "unevaluatedItems"
            | "contentSchema" => materialize_schema(value, Scope::Normal, context),
            "prefixItems" => materialize_schema_array(value, Scope::Normal, context),
            _ => {}
        }
    }
    match scope {
        Scope::Normal => inject_ambient_ref_if_eligible(map, context),
        Scope::DirectNamespace => mark_direct_namespace(map),
    }
}

fn materialize_properties(value: &mut Value, scope: Scope, context: &AmbientContext) {
    let Some(properties) = value.as_object_mut() else {
        return;
    };
    for (name, schema) in properties {
        let child_scope = match (scope, name.as_str()) {
            (Scope::DirectNamespace, "ucp") => {
                *schema = Value::Bool(false);
                continue;
            }
            (Scope::Normal, "ucp") => Scope::DirectNamespace,
            _ => Scope::Normal,
        };
        materialize_schema(schema, child_scope, context);
    }
}

fn materialize_schema_map(value: &mut Value, scope: Scope, context: &AmbientContext) {
    if let Some(map) = value.as_object_mut() {
        for schema in map.values_mut() {
            materialize_schema(schema, scope, context);
        }
    }
}

fn materialize_schema_array(value: &mut Value, scope: Scope, context: &AmbientContext) {
    if let Some(items) = value.as_array_mut() {
        for item in items {
            materialize_schema(item, scope, context);
        }
    }
}

fn inject_ambient_ref_if_eligible(map: &mut Map<String, Value>, context: &AmbientContext) {
    let Some(properties) = map.get_mut("properties").and_then(Value::as_object_mut) else {
        return;
    };
    if !properties.is_empty() && !properties.contains_key("ucp") {
        properties.insert("ucp".to_string(), absolute_ref_schema(&context.helper_ref));
    }
}

fn mark_direct_namespace(map: &mut Map<String, Value>) {
    let properties = map
        .entry("properties".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(properties) = properties {
        properties.insert("ucp".to_string(), Value::Bool(false));
    }
    map.insert("additionalProperties".to_string(), empty_schema());
    if has_composition(map) {
        map.insert("unevaluatedProperties".to_string(), empty_schema());
    }
}

fn allocate_helper_key(root: &Map<String, Value>) -> Result<String, ResolveError> {
    let Some(defs) = root.get("$defs") else {
        return Ok(AMBIENT_UCP_MEMBERS_DEF_KEY.to_string());
    };
    let defs = defs.as_object().ok_or_else(|| {
        invalid("ambient UCP materialization requires root $defs to be an object")
    })?;
    let mut key = AMBIENT_UCP_MEMBERS_DEF_KEY.to_string();
    for suffix in 1.. {
        if !defs.contains_key(&key) {
            return Ok(key);
        }
        key = format!("{AMBIENT_UCP_MEMBERS_DEF_KEY}_{suffix}");
    }
    unreachable!("unbounded suffix search should always find a helper key")
}

fn install_members_helper(
    schema: &mut Value,
    key: &str,
    helper: Value,
) -> Result<(), ResolveError> {
    let root = schema
        .as_object_mut()
        .ok_or_else(|| invalid("ambient UCP materialization requires an object schema root"))?;
    let defs = root
        .entry("$defs".to_string())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| {
            invalid("ambient UCP materialization requires root $defs to be an object")
        })?;
    defs.insert(key.to_string(), helper);
    Ok(())
}

fn reject_local_member_refs(members: &Value) -> Result<(), ResolveError> {
    if let Some(reference) = find_local_ref(members) {
        return Err(invalid(format!(
            "ambient UCP members registry must be bundled: found local $ref '{reference}'"
        )));
    }
    Ok(())
}

fn find_local_ref(value: &Value) -> Option<&str> {
    match value {
        Value::Object(map) => {
            if let Some(reference) = map
                .get("$ref")
                .and_then(Value::as_str)
                .filter(|reference| reference.starts_with('#'))
            {
                return Some(reference);
            }
            map.values().find_map(find_local_ref)
        }
        Value::Array(items) => items.iter().find_map(find_local_ref),
        _ => None,
    }
}

fn registered_member_names(members: &Value) -> Result<Vec<String>, ResolveError> {
    let members = members
        .as_object()
        .ok_or_else(|| invalid("ambient UCP members registry must be an object schema"))?;
    let properties = members
        .get("properties")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("ambient UCP members registry must have object properties"))?;

    Ok(properties.keys().cloned().collect())
}

fn mark_omitted_registered_members_false(
    schema: &mut Value,
    registered_members: &[String],
    helper_key: &str,
) -> Result<(), ResolveError> {
    let helper = schema
        .get_mut("$defs")
        .and_then(Value::as_object_mut)
        .and_then(|defs| defs.get_mut(helper_key))
        .and_then(Value::as_object_mut)
        .ok_or_else(|| {
            invalid(format!(
                "ambient UCP helper '{helper_key}' missing after resolution"
            ))
        })?;
    let properties = helper
        .get_mut("properties")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| {
            invalid(format!(
                "ambient UCP helper '{helper_key}' must have properties"
            ))
        })?;
    for name in registered_members {
        if !properties.contains_key(name) {
            properties.insert(name.clone(), Value::Bool(false));
        }
    }
    Ok(())
}

fn has_composition(map: &Map<String, Value>) -> bool {
    map.contains_key("allOf") || map.contains_key("anyOf") || map.contains_key("oneOf")
}
fn absolute_ref_schema(reference: &str) -> Value {
    serde_json::json!({ "$ref": reference })
}
fn empty_schema() -> Value {
    Value::Object(Map::new())
}
fn invalid(message: impl Into<String>) -> ResolveError {
    ResolveError::InvalidSchema {
        message: message.into(),
    }
}
