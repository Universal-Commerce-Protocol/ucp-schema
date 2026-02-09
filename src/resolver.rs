//! Schema resolution - transforms UCP annotated schemas into standard JSON Schema.

use serde_json::{Map, Value};

use crate::error::ResolveError;
use crate::types::{
    is_valid_schema_transition, json_type_name, Direction, ResolveOptions, SchemaTransitionInfo,
    Visibility, UCP_ANNOTATIONS, VALID_OPERATIONS,
};

/// Resolve a schema for a specific direction and operation.
///
/// Returns a standard JSON Schema with UCP annotations removed.
/// When `options.strict` is true, sets `additionalProperties: false`
/// on all object schemas to reject unknown fields. Default is false
/// to respect UCP's extensibility model.
///
/// # Errors
///
/// Returns `ResolveError` if the schema contains invalid annotations.
pub fn resolve(schema: &Value, options: &ResolveOptions) -> Result<Value, ResolveError> {
    let mut resolved = resolve_value(schema, options, "")?;

    if options.strict {
        close_additional_properties(&mut resolved);
    }

    Ok(resolved)
}

/// Recursively close object schemas to reject unknown properties.
///
/// For simple object schemas: sets `additionalProperties: false`
/// For schemas with composition (allOf/anyOf/oneOf): sets `unevaluatedProperties: false`
///
/// The distinction matters because `additionalProperties` is evaluated per-schema,
/// while `unevaluatedProperties` (JSON Schema 2020-12) looks across all subschemas.
/// This allows $ref inheritance patterns to work correctly in strict mode.
fn close_additional_properties(value: &mut Value) {
    close_additional_properties_inner(value, false);
}

/// Inner implementation with context tracking.
///
/// `in_composition_branch` is true when processing direct children of allOf/anyOf/oneOf.
/// We skip setting additionalProperties on these because each branch is validated
/// independently and doesn't see properties from sibling branches.
fn close_additional_properties_inner(value: &mut Value, in_composition_branch: bool) {
    if let Value::Object(map) = value {
        // Check if this schema uses composition keywords
        let has_composition =
            map.contains_key("allOf") || map.contains_key("anyOf") || map.contains_key("oneOf");

        // Check if this is an object schema (has "type": "object" or has "properties")
        let is_object_schema = map
            .get("type")
            .and_then(|t| t.as_str())
            .map(|t| t == "object")
            .unwrap_or(false)
            || map.contains_key("properties");

        // Close the schema if we're not inside a composition branch
        if !in_composition_branch && (is_object_schema || has_composition) {
            if has_composition {
                // Use unevaluatedProperties for composition - it looks across all subschemas
                // so $ref inheritance works correctly
                match map.get("unevaluatedProperties") {
                    None => {
                        map.insert("unevaluatedProperties".to_string(), Value::Bool(false));
                    }
                    Some(Value::Bool(true)) => {
                        map.insert("unevaluatedProperties".to_string(), Value::Bool(false));
                    }
                    _ => {}
                }
            } else {
                // Simple object schema - use additionalProperties
                match map.get("additionalProperties") {
                    None => {
                        map.insert("additionalProperties".to_string(), Value::Bool(false));
                    }
                    Some(Value::Bool(true)) => {
                        map.insert("additionalProperties".to_string(), Value::Bool(false));
                    }
                    _ => {}
                }
            }
        }

        // Recurse into all values
        for (key, child) in map.iter_mut() {
            match key.as_str() {
                "properties" => {
                    // Recurse into each property definition
                    if let Value::Object(props) = child {
                        for prop_value in props.values_mut() {
                            close_additional_properties_inner(prop_value, false);
                        }
                    }
                }
                "items" | "additionalProperties" | "unevaluatedProperties" => {
                    // Schema values - recurse
                    close_additional_properties_inner(child, false);
                }
                "$defs" | "definitions" => {
                    // Definitions - recurse into each
                    if let Value::Object(defs) = child {
                        for def_value in defs.values_mut() {
                            close_additional_properties_inner(def_value, false);
                        }
                    }
                }
                "allOf" | "anyOf" | "oneOf" => {
                    // Composition branches - recurse but mark as in_composition
                    // so we don't set additionalProperties on them directly
                    if let Value::Array(arr) = child {
                        for item in arr {
                            close_additional_properties_inner(item, true);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// Result of resolving a property's visibility: visibility and optional schema-transition info
/// to emit in the resolved schema so devs see optional fields and commentary.
pub type VisibilityResult = (Visibility, Option<SchemaTransitionInfo>);

/// Get visibility for a single property.
///
/// Looks up the appropriate annotation (`ucp_request` or `ucp_response`) and
/// determines the visibility for the given operation.
///
/// # Errors
///
/// Returns `ResolveError` if the annotation has invalid type or unknown visibility value.
pub fn get_visibility(
    prop: &Value,
    direction: Direction,
    operation: &str,
    path: &str,
) -> Result<VisibilityResult, ResolveError> {
    let key = direction.annotation_key();
    let Some(annotation) = prop.get(key) else {
        return Ok((Visibility::Include, None));
    };

    match annotation {
        // Shorthand: "ucp_request": "omit" - applies to all operations
        Value::String(s) => parse_visibility_value(s, path).map(|v| (v, None)),

        Value::Object(map) => {
            let op_path = format!("{}/{}", path, operation);
            // Shorthand schema transition: { from, to, description } applies to all operations
            if map.contains_key("from") && map.contains_key("to") && map.contains_key("description")
                && !map.keys().any(|k| VALID_OPERATIONS.contains(&k.as_str()))
            {
                return parse_schema_transition_object(map, path);
            }
            match map.get(operation) {
                Some(Value::String(s)) => parse_visibility_value(s, &op_path).map(|v| (v, None)),
                Some(Value::Object(obj)) => parse_schema_transition_object(obj, &op_path),
                Some(other) => Err(ResolveError::InvalidAnnotationType {
                    path: op_path,
                    actual: json_type_name(other).to_string(),
                }),
                // Operation not specified → default to include
                None => Ok((Visibility::Include, None)),
            }
        }

        // Invalid type
        other => Err(ResolveError::InvalidAnnotationType {
            path: path.to_string(),
            actual: json_type_name(other).to_string(),
        }),
    }
}

/// Strip all UCP annotations from a schema.
///
/// Recursively removes `ucp_request` and `ucp_response`.
pub fn strip_annotations(schema: &Value) -> Value {
    strip_annotations_recursive(schema)
}

// --- Internal implementation ---

fn resolve_value(
    value: &Value,
    options: &ResolveOptions,
    path: &str,
) -> Result<Value, ResolveError> {
    match value {
        Value::Object(map) => resolve_object(map, options, path),
        Value::Array(arr) => resolve_array(arr, options, path),
        // Primitives pass through unchanged
        other => Ok(other.clone()),
    }
}

fn resolve_object(
    map: &Map<String, Value>,
    options: &ResolveOptions,
    path: &str,
) -> Result<Value, ResolveError> {
    let mut result = Map::new();

    // Track required array modifications
    let original_required: Vec<String> = map
        .get("required")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let mut new_required: Vec<String> = original_required.clone();

    for (key, value) in map {
        // Skip UCP annotations in output
        if UCP_ANNOTATIONS.contains(&key.as_str()) {
            continue;
        }

        let child_path = format!("{}/{}", path, key);

        match key.as_str() {
            "properties" => {
                let resolved = resolve_properties(value, options, &child_path, &mut new_required)?;
                result.insert(key.clone(), resolved);
            }
            "items" => {
                // Array items - recurse
                let resolved = resolve_value(value, options, &child_path)?;
                result.insert(key.clone(), resolved);
            }
            "$defs" | "definitions" => {
                // Definitions - recurse into each definition
                let resolved = resolve_defs(value, options, &child_path)?;
                result.insert(key.clone(), resolved);
            }
            "allOf" | "anyOf" | "oneOf" => {
                // Composition - transform each branch
                let resolved = resolve_composition(value, options, &child_path)?;
                result.insert(key.clone(), resolved);
            }
            "additionalProperties" => {
                // If it's a schema (object), recurse; otherwise keep as-is
                if value.is_object() {
                    let resolved = resolve_value(value, options, &child_path)?;
                    result.insert(key.clone(), resolved);
                } else {
                    result.insert(key.clone(), value.clone());
                }
            }
            "required" => {
                // Will be handled at the end after processing properties
                continue;
            }
            _ => {
                // Other keys - recurse if object/array, otherwise copy
                let resolved = resolve_value(value, options, &child_path)?;
                result.insert(key.clone(), resolved);
            }
        }
    }

    // Add updated required array if non-empty or if original existed
    if !new_required.is_empty() || map.contains_key("required") {
        result.insert(
            "required".to_string(),
            Value::Array(new_required.into_iter().map(Value::String).collect()),
        );
    }

    Ok(Value::Object(result))
}

fn resolve_properties(
    value: &Value,
    options: &ResolveOptions,
    path: &str,
    required: &mut Vec<String>,
) -> Result<Value, ResolveError> {
    let Some(props) = value.as_object() else {
        return Ok(value.clone());
    };

    let mut result = Map::new();

    for (prop_name, prop_value) in props {
        let prop_path = format!("{}/{}", path, prop_name);

        // Get visibility for this property
        let (visibility, schema_transition_info) = get_visibility(
            prop_value,
            options.direction,
            &options.operation,
            &prop_path,
        )?;

        match visibility {
            Visibility::Omit => {
                // Remove from properties and required
                required.retain(|r| r != prop_name);
            }
            Visibility::Required => {
                // Keep property, ensure in required
                let resolved = resolve_value(prop_value, options, &prop_path)?;
                let stripped = strip_annotations(&resolved);
                result.insert(prop_name.clone(), stripped);
                if !required.contains(prop_name) {
                    required.push(prop_name.clone());
                }
            }
            Visibility::Optional => {
                // Keep property, remove from required
                let resolved = resolve_value(prop_value, options, &prop_path)?;
                let mut stripped = strip_annotations(&resolved);
                if let Some(ref info) = schema_transition_info {
                    if let Value::Object(ref mut obj) = stripped {
                        obj.insert(
                            "x-ucp-schema-transition".to_string(),
                            serde_json::to_value(info).unwrap(),
                        );
                        // Only set deprecated: true when the field is being removed (to "omit").
                        // Required→optional or optional→required are contract changes, not removal.
                        if info.to == "omit" {
                            obj.insert("deprecated".to_string(), Value::Bool(true));
                        }
                    }
                }
                result.insert(prop_name.clone(), stripped);
                required.retain(|r| r != prop_name);
            }
            Visibility::Include => {
                // Keep as-is (preserve original required status)
                let resolved = resolve_value(prop_value, options, &prop_path)?;
                let stripped = strip_annotations(&resolved);
                result.insert(prop_name.clone(), stripped);
            }
        }
    }

    Ok(Value::Object(result))
}

fn resolve_defs(
    value: &Value,
    options: &ResolveOptions,
    path: &str,
) -> Result<Value, ResolveError> {
    let Some(defs) = value.as_object() else {
        return Ok(value.clone());
    };

    let mut result = Map::new();
    for (name, def) in defs {
        let def_path = format!("{}/{}", path, name);
        let resolved = resolve_value(def, options, &def_path)?;
        result.insert(name.clone(), resolved);
    }

    Ok(Value::Object(result))
}

fn resolve_array(
    arr: &[Value],
    options: &ResolveOptions,
    path: &str,
) -> Result<Value, ResolveError> {
    let mut result = Vec::new();
    for (i, item) in arr.iter().enumerate() {
        let item_path = format!("{}/{}", path, i);
        let resolved = resolve_value(item, options, &item_path)?;
        result.push(resolved);
    }
    Ok(Value::Array(result))
}

fn resolve_composition(
    value: &Value,
    options: &ResolveOptions,
    path: &str,
) -> Result<Value, ResolveError> {
    let Some(arr) = value.as_array() else {
        return Ok(value.clone());
    };

    let mut result = Vec::new();
    for (i, item) in arr.iter().enumerate() {
        let item_path = format!("{}/{}", path, i);
        let resolved = resolve_value(item, options, &item_path)?;
        result.push(resolved);
    }

    Ok(Value::Array(result))
}

fn strip_annotations_recursive(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut result = Map::new();
            for (k, v) in map {
                if !UCP_ANNOTATIONS.contains(&k.as_str()) {
                    result.insert(k.clone(), strip_annotations_recursive(v));
                }
            }
            Value::Object(result)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(strip_annotations_recursive).collect()),
        other => other.clone(),
    }
}

fn parse_visibility_value(s: &str, path: &str) -> Result<Visibility, ResolveError> {
    Visibility::parse(s).ok_or_else(|| ResolveError::UnknownVisibility {
        path: path.to_string(),
        value: s.to_string(),
    })
}

/// Parse a schema-transition object { from, to, description } and return (Visibility, Some(SchemaTransitionInfo)).
fn parse_schema_transition_object(
    map: &serde_json::Map<String, Value>,
    path: &str,
) -> Result<VisibilityResult, ResolveError> {
    let from = map
        .get("from")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ResolveError::InvalidSchemaTransition {
            path: path.to_string(),
            message: "missing \"from\" (must be omit, optional, or required)".to_string(),
        })?;
    let to = map
        .get("to")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ResolveError::InvalidSchemaTransition {
            path: path.to_string(),
            message: "missing \"to\" (must be omit, optional, or required)".to_string(),
        })?;
    let description = map
        .get("description")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ResolveError::InvalidSchemaTransition {
            path: path.to_string(),
            message: "missing \"description\"".to_string(),
        })?;

    if Visibility::parse(from).is_none() {
        return Err(ResolveError::InvalidSchemaTransition {
            path: path.to_string(),
            message: format!(
                "\"from\" must be omit, optional, or required, got \"{}\"",
                from
            ),
        });
    }
    if Visibility::parse(to).is_none() {
        return Err(ResolveError::InvalidSchemaTransition {
            path: path.to_string(),
            message: format!(
                "\"to\" must be omit, optional, or required, got \"{}\"",
                to
            ),
        });
    }
    if !is_valid_schema_transition(from, to) {
        return Err(ResolveError::InvalidSchemaTransition {
            path: path.to_string(),
            message: format!(
                "from and to must be distinct visibility values (omit, optional, required), got from=\"{}\" to=\"{}\"",
                from, to
            ),
        });
    }

    let info = SchemaTransitionInfo {
        from: from.to_string(),
        to: to.to_string(),
        description: description.to_string(),
    };
    Ok((Visibility::Optional, Some(info)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // === Visibility Parsing Tests ===

    #[test]
    fn get_visibility_shorthand_omit() {
        let prop = json!({
            "type": "string",
            "ucp_request": "omit"
        });
        let (vis, _) = get_visibility(&prop, Direction::Request, "create", "/test").unwrap();
        assert_eq!(vis, Visibility::Omit);
    }

    #[test]
    fn get_visibility_shorthand_required() {
        let prop = json!({
            "type": "string",
            "ucp_request": "required"
        });
        let (vis, _) = get_visibility(&prop, Direction::Request, "create", "/test").unwrap();
        assert_eq!(vis, Visibility::Required);
    }

    #[test]
    fn get_visibility_object_form() {
        let prop = json!({
            "type": "string",
            "ucp_request": {
                "create": "omit",
                "update": "required"
            }
        });
        let (vis, _) = get_visibility(&prop, Direction::Request, "create", "/test").unwrap();
        assert_eq!(vis, Visibility::Omit);

        let (vis, _) = get_visibility(&prop, Direction::Request, "update", "/test").unwrap();
        assert_eq!(vis, Visibility::Required);
    }

    #[test]
    fn get_visibility_schema_transition_object() {
        let prop = json!({
            "type": "string",
            "ucp_request": {
                "update": {
                    "from": "required",
                    "to": "omit",
                    "description": "Legacy id will be removed in v2."
                }
            }
        });
        let (vis, dep) = get_visibility(&prop, Direction::Request, "update", "/test").unwrap();
        assert_eq!(vis, Visibility::Optional);
        let info = dep.unwrap();
        assert_eq!(info.from, "required");
        assert_eq!(info.to, "omit");
        assert_eq!(info.description, "Legacy id will be removed in v2.");
    }

    #[test]
    fn get_visibility_missing_annotation() {
        let prop = json!({
            "type": "string"
        });
        let (vis, _) = get_visibility(&prop, Direction::Request, "create", "/test").unwrap();
        assert_eq!(vis, Visibility::Include);
    }

    #[test]
    fn get_visibility_missing_operation_in_dict() {
        let prop = json!({
            "type": "string",
            "ucp_request": {
                "create": "omit"
            }
        });
        let (vis, _) = get_visibility(&prop, Direction::Request, "update", "/test").unwrap();
        assert_eq!(vis, Visibility::Include);
    }

    #[test]
    fn get_visibility_response_direction() {
        let prop = json!({
            "type": "string",
            "ucp_response": "omit"
        });
        let (vis, _) = get_visibility(&prop, Direction::Response, "create", "/test").unwrap();
        assert_eq!(vis, Visibility::Omit);

        let (vis, _) = get_visibility(&prop, Direction::Request, "create", "/test").unwrap();
        assert_eq!(vis, Visibility::Include);
    }

    #[test]
    fn get_visibility_invalid_type_errors() {
        let prop = json!({
            "type": "string",
            "ucp_request": 123
        });
        let result = get_visibility(&prop, Direction::Request, "create", "/test");
        assert!(matches!(
            result,
            Err(ResolveError::InvalidAnnotationType { .. })
        ));
    }

    #[test]
    fn get_visibility_unknown_visibility_errors() {
        let prop = json!({
            "type": "string",
            "ucp_request": "readonly"
        });
        let result = get_visibility(&prop, Direction::Request, "create", "/test");
        assert!(matches!(
            result,
            Err(ResolveError::UnknownVisibility { value, .. }) if value == "readonly"
        ));
    }

    #[test]
    fn get_visibility_unknown_in_dict_errors() {
        let prop = json!({
            "type": "string",
            "ucp_request": {
                "create": "maybe"
            }
        });
        let result = get_visibility(&prop, Direction::Request, "create", "/test");
        assert!(matches!(
            result,
            Err(ResolveError::UnknownVisibility { value, .. }) if value == "maybe"
        ));
    }

    #[test]
    fn get_visibility_invalid_schema_transition_errors() {
        let prop = json!({
            "type": "string",
            "ucp_request": {
                "update": {
                    "from": "required",
                    "to": "omit"
                }
            }
        });
        let result = get_visibility(&prop, Direction::Request, "update", "/test");
        assert!(matches!(
            result,
            Err(ResolveError::InvalidSchemaTransition { .. })
        ));
    }

    // === Transformation Tests ===

    #[test]
    fn resolve_omit_removes_field() {
        let schema = json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "ucp_request": "omit" },
                "name": { "type": "string" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        assert!(result["properties"].get("id").is_none());
        assert!(result["properties"].get("name").is_some());
    }

    #[test]
    fn resolve_omit_removes_from_required() {
        let schema = json!({
            "type": "object",
            "required": ["id", "name"],
            "properties": {
                "id": { "type": "string", "ucp_request": "omit" },
                "name": { "type": "string" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        let required = result["required"].as_array().unwrap();
        assert!(!required.contains(&json!("id")));
        assert!(required.contains(&json!("name")));
    }

    #[test]
    fn resolve_required_adds_to_required() {
        let schema = json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "ucp_request": "required" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        let required = result["required"].as_array().unwrap();
        assert!(required.contains(&json!("id")));
    }

    #[test]
    fn resolve_optional_removes_from_required() {
        let schema = json!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": { "type": "string", "ucp_request": "optional" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        let required = result["required"].as_array().unwrap();
        assert!(!required.contains(&json!("id")));
    }

    #[test]
    fn resolve_schema_transition_emits_transition_info() {
        let schema = json!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {
                    "type": "string",
                    "ucp_request": {
                        "from": "required",
                        "to": "optional",
                        "description": "Will become optional in v2."
                    }
                }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        assert!(result["properties"].get("id").is_some());
        let required = result["required"].as_array().unwrap();
        assert!(!required.contains(&json!("id")));
        let transition = result["properties"]["id"].get("x-ucp-schema-transition").unwrap();
        assert_eq!(transition["from"], "required");
        assert_eq!(transition["to"], "optional");
        assert_eq!(transition["description"], "Will become optional in v2.");
        assert!(result["properties"]["id"].get("deprecated").is_none());
    }

    #[test]
    fn resolve_schema_transition_sets_deprecated_when_to_omit() {
        let schema = json!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {
                    "type": "string",
                    "ucp_request": {
                        "from": "required",
                        "to": "omit",
                        "description": "Will be removed in v2."
                    }
                }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        assert!(result["properties"].get("id").is_some());
        let required = result["required"].as_array().unwrap();
        assert!(!required.contains(&json!("id")));
        assert!(result["properties"]["id"].get("x-ucp-schema-transition").is_some());
        assert_eq!(result["properties"]["id"]["deprecated"], true);
    }

    #[test]
    fn resolve_schema_transition_per_operation() {
        let schema = json!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {
                    "type": "string",
                    "ucp_request": {
                        "create": "omit",
                        "update": {
                            "from": "required",
                            "to": "omit",
                            "description": "Removing in v2."
                        }
                    }
                }
            }
        });

        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();
        assert!(result["properties"].get("id").is_none());

        let options = ResolveOptions::new(Direction::Request, "update");
        let result = resolve(&schema, &options).unwrap();
        assert!(result["properties"].get("id").is_some());
        let required = result["required"].as_array().unwrap();
        assert!(!required.contains(&json!("id")));
        assert_eq!(result["properties"]["id"]["x-ucp-schema-transition"]["description"], "Removing in v2.");
    }

    #[test]
    fn resolve_include_preserves_original() {
        let schema = json!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": { "type": "string" },
                "name": { "type": "string" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        // Both fields should be present
        assert!(result["properties"].get("id").is_some());
        assert!(result["properties"].get("name").is_some());

        // Required should be preserved
        let required = result["required"].as_array().unwrap();
        assert!(required.contains(&json!("id")));
        assert!(!required.contains(&json!("name")));
    }

    #[test]
    fn resolve_strips_annotations() {
        let schema = json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "ucp_request": "required",
                    "ucp_response": "omit"
                }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        // Annotations should be stripped
        assert!(result["properties"]["id"].get("ucp_request").is_none());
        assert!(result["properties"]["id"].get("ucp_response").is_none());
    }

    #[test]
    fn resolve_empty_schema_after_filtering() {
        let schema = json!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": { "type": "string", "ucp_request": "omit" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create");
        let result = resolve(&schema, &options).unwrap();

        // Properties should be empty object
        assert_eq!(result["properties"], json!({}));
        // Required should be empty array
        assert_eq!(result["required"], json!([]));
    }

    // === Strip Annotations Tests ===

    #[test]
    fn strip_annotations_removes_all_ucp() {
        let schema = json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "ucp_request": "omit",
                    "ucp_response": "required"
                }
            }
        });
        let result = strip_annotations(&schema);

        assert!(result["properties"]["id"].get("ucp_request").is_none());
        assert!(result["properties"]["id"].get("ucp_response").is_none());
    }
}
