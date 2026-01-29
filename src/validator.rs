//! Payload validation against resolved schemas.

use serde_json::Value;

use crate::error::{ResolveError, SchemaError, ValidateError};
use crate::resolver::resolve;
use crate::types::ResolveOptions;

/// Validate a payload against a UCP schema.
///
/// Resolves the schema for the given direction and operation, then validates
/// the payload against the resolved schema.
///
/// # Errors
///
/// Returns `ValidateError::Resolve` if schema resolution fails, or
/// `ValidateError::Invalid` if the payload doesn't match the schema.
pub fn validate(
    schema: &Value,
    payload: &Value,
    options: &ResolveOptions,
) -> Result<(), ValidateError> {
    // First resolve the schema
    let resolved = resolve(schema, options)?;

    // Then validate against resolved schema
    validate_against_schema(&resolved, payload)
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
