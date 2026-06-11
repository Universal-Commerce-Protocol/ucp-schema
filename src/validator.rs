//! Payload validation against resolved schemas.

use serde_json::{Map, Value};

use crate::compose::is_container_schema;
use crate::error::{ResolveError, SchemaError, ValidateError};
use crate::resolver::resolve;
use crate::types::ResolveOptions;

/// Validate a payload against a UCP schema.
///
/// Resolves the schema for the given direction and operation, selects the
/// operation shape for container-shaped capabilities, then validates the
/// payload against the resulting schema.
///
/// # Errors
///
/// Returns `ValidateError::Resolve` if schema resolution or operation-shape
/// selection fails, or `ValidateError::Invalid` if the payload doesn't match.
pub fn validate(
    schema: &Value,
    payload: &Value,
    options: &ResolveOptions,
) -> Result<(), ValidateError> {
    let resolved = resolve(schema, options)?;

    // The message body to validate depends on the capability's shape:
    // single-object capabilities validate at the root; container capabilities
    // validate at the selected operation shape.
    let target = select_operation_schema(&resolved, options)?;

    validate_against_schema(&target, payload)
}

/// Resolve a (possibly container-shaped) schema to its validation target.
///
/// For a single-object capability the target is the root, returned unchanged.
/// For a container capability (see [`crate::is_container_schema`]) the target is
/// the message body for this `(op, direction)`, held at `$defs/{op}_{direction}`;
/// this roots validation there via a `$ref` while keeping the sibling `$defs`
/// and `$schema` in scope so internal refs and the dialect still resolve.
///
/// A container root carries no body of its own, so an absent `{op}_{direction}`
/// is reported as `OperationShapeNotFound` rather than falling through to an
/// unconstrained root.
pub fn select_operation_schema(
    schema: &Value,
    options: &ResolveOptions,
) -> Result<Value, ResolveError> {
    if !is_container_schema(schema) {
        return Ok(schema.clone());
    }

    let key = format!("{}_{}", options.operation, options.direction.dir_str());
    let defs = schema.get("$defs").and_then(|d| d.as_object());

    let has_key = defs.map(|d| d.contains_key(&key)).unwrap_or(false);
    if !has_key {
        let available = defs
            .map(|d| {
                d.keys()
                    .filter(|k| k.ends_with("_request") || k.ends_with("_response"))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        return Err(ResolveError::OperationShapeNotFound { key, available });
    }

    // Thin wrapper rooted at the operation shape. `$ref` + sibling `$defs` is
    // valid JSON Schema 2020-12 and keeps every sibling def resolvable.
    let mut wrapper = Map::new();
    if let Some(s) = schema.get("$schema") {
        wrapper.insert("$schema".to_string(), s.clone());
    }
    wrapper.insert(
        "$ref".to_string(),
        Value::String(format!("#/$defs/{}", key)),
    );
    if let Some(defs) = schema.get("$defs") {
        wrapper.insert("$defs".to_string(), defs.clone());
    }
    Ok(Value::Object(wrapper))
}

/// Validate a payload against an already-resolved schema.
///
/// Use this when you've already resolved the schema and want to validate
/// multiple payloads against it.
pub fn validate_against_schema(schema: &Value, payload: &Value) -> Result<(), ValidateError> {
    let validator = jsonschema::validator_for(schema).map_err(|e| {
        ValidateError::Resolve(ResolveError::InvalidSchema {
            message: e.to_string(),
        })
    })?;

    let errors: Vec<SchemaError> = validator
        .iter_errors(payload)
        .map(|e| SchemaError {
            path: e.instance_path.to_string(),
            message: e.to_string(),
        })
        .collect();

    if errors.is_empty() {
        Ok(())
    } else {
        Err(ValidateError::Invalid { errors })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Direction;
    use serde_json::json;

    #[test]
    fn validate_valid_payload() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            },
            "required": ["name"]
        });
        let payload = json!({ "name": "test" });
        let options = ResolveOptions::new(Direction::Request, "create");

        let result = validate(&schema, &payload, &options);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_missing_required_field() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "ucp_request": "required" }
            }
        });
        let payload = json!({});
        let options = ResolveOptions::new(Direction::Request, "create");

        let result = validate(&schema, &payload, &options);
        assert!(matches!(result, Err(ValidateError::Invalid { .. })));
    }

    #[test]
    fn validate_wrong_type() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            }
        });
        let payload = json!({ "name": 123 });
        let options = ResolveOptions::new(Direction::Request, "create");

        let result = validate(&schema, &payload, &options);
        assert!(matches!(result, Err(ValidateError::Invalid { .. })));
    }

    #[test]
    fn validate_omitted_field_rejected() {
        // When additionalProperties is false and a field is omitted,
        // sending that field should fail validation
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "id": { "type": "string", "ucp_request": "omit" },
                "name": { "type": "string" }
            }
        });
        let payload = json!({ "name": "test", "id": "123" });
        let options = ResolveOptions::new(Direction::Request, "create");

        let result = validate(&schema, &payload, &options);
        assert!(matches!(result, Err(ValidateError::Invalid { .. })));
    }

    #[test]
    fn validate_collects_multiple_errors() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "ucp_request": "required" },
                "age": { "type": "number", "ucp_request": "required" }
            }
        });
        let payload = json!({});
        let options = ResolveOptions::new(Direction::Request, "create");

        let result = validate(&schema, &payload, &options);
        match result {
            Err(ValidateError::Invalid { errors }) => {
                assert_eq!(errors.len(), 2);
            }
            _ => panic!("expected validation error with 2 errors"),
        }
    }

    #[test]
    fn validate_allof_strict_accepts_properties_from_all_branches() {
        // allOf with strict mode should accept properties defined in ANY branch
        // This tests that unevaluatedProperties correctly sees all branch properties
        let schema = json!({
            "allOf": [
                {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" }
                    }
                },
                {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" }
                    }
                }
            ]
        });
        // Payload uses properties from BOTH branches
        let payload = json!({ "id": "123", "name": "test" });
        let options = ResolveOptions::new(Direction::Request, "create").strict(true);

        let result = validate(&schema, &payload, &options);
        assert!(
            result.is_ok(),
            "should accept properties from all allOf branches"
        );
    }

    #[test]
    fn validate_allof_strict_rejects_unknown_properties() {
        // allOf with strict mode should reject properties not in ANY branch
        let schema = json!({
            "allOf": [
                {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" }
                    }
                },
                {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" }
                    }
                }
            ]
        });
        // Payload has unknown property
        let payload = json!({ "id": "123", "name": "test", "unknown": "bad" });
        let options = ResolveOptions::new(Direction::Request, "create").strict(true);

        let result = validate(&schema, &payload, &options);
        assert!(
            matches!(result, Err(ValidateError::Invalid { .. })),
            "should reject unknown properties in strict mode"
        );
    }

    #[test]
    fn validate_allof_non_strict_allows_unknown_properties() {
        // allOf without strict mode should allow unknown properties (extensibility)
        let schema = json!({
            "allOf": [
                {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" }
                    }
                },
                {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" }
                    }
                }
            ]
        });
        // Payload has unknown property
        let payload = json!({ "id": "123", "name": "test", "unknown": "allowed" });
        let options = ResolveOptions::new(Direction::Request, "create").strict(false);

        let result = validate(&schema, &payload, &options);
        assert!(
            result.is_ok(),
            "should allow unknown properties in non-strict mode"
        );
    }
}
