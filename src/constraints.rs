//! Context-aware checking and compilation of UCP runtime constraints.
//!
//! The generic compiler understands Object, Value, and Type Constraints. Ordinary
//! properties declared by a concrete constraint schema are validated but remain
//! opaque: their domain semantics belong to the capability, extension, or payment
//! handler that defines them.

use std::collections::BTreeSet;

use serde_json::{Map, Value};
use thiserror::Error;

use crate::validator::validate_against_schema;

const OBJECT_CONSTRAINT_ID: &str = "object_constraint.json";
const VALUE_CONSTRAINT_ID: &str = "value_constraint.json";
const TYPE_CONSTRAINT_ID: &str = "type_constraint.json";

/// The generic role of a property in a concrete constraint schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintKind {
    /// Recursively constrains an object.
    Object,
    /// Applies a bounded JSON Schema fragment (`enum` or `const`) to a value.
    Value,
    /// Selects one branch of a typed family.
    Type,
    /// A domain-specific value. Its shape is checked, but its semantics are opaque.
    Custom,
}

/// A declared constraint whose semantics are owned by its defining domain.
#[derive(Debug, Clone, PartialEq)]
pub struct OpaqueConstraint {
    /// JSON Pointer to the constraint value.
    pub path: String,
    /// Generic schema classification, when one was declared.
    pub kind: ConstraintKind,
    /// The validated wire value.
    pub value: Value,
}

/// Generic output from compiling one Object Constraint.
#[derive(Debug, Clone, PartialEq)]
pub struct ConstraintPlan {
    /// Standard JSON Schema overlay compiled from `required` and same-name
    /// Object/Value Constraints.
    pub schema_overlay: Value,
    /// Correctly typed constraints that cannot be generically applied to the
    /// supplied target schema.
    pub opaque_constraints: Vec<OpaqueConstraint>,
}

/// Errors found while checking a concrete constraint declaration.
#[derive(Debug, Error, PartialEq)]
pub enum ConstraintError {
    /// The declaration does not validate against its concrete constraint schema.
    #[error("constraint declaration is invalid: {message}")]
    InvalidDeclaration { message: String },

    /// An Object Constraint key was not declared by the concrete schema.
    #[error("constraint key '{key}' at {path} is not declared by the concrete constraint schema")]
    UndeclaredKey { path: String, key: String },

    /// `required` named a property absent from the resolved target schema.
    #[error("required property '{property}' at {path} is not declared by the target schema")]
    UnknownRequiredProperty { path: String, property: String },

    /// A Value Constraint contains a value incompatible with its target schema.
    #[error("value constraint at {path} is incompatible with the target schema: {message}")]
    IncompatibleValue { path: String, message: String },
}

/// Check and compile a concrete Object Constraint.
///
/// Inputs must be bundled: all external `$ref` values needed by the target and
/// concrete constraint schemas must already be resolved. The function:
///
/// 1. validates the declaration against the concrete constraint schema;
/// 2. rejects undeclared keys at every Object Constraint node;
/// 3. checks `required` names against the resolved target;
/// 4. compiles same-name Object and Value Constraints into a JSON Schema overlay;
/// 5. preserves non-property and domain-specific constraints as opaque values.
pub fn compile_constraint(
    target_schema: &Value,
    concrete_constraint_schema: &Value,
    constraint: &Value,
) -> Result<ConstraintPlan, ConstraintError> {
    validate_against_schema(concrete_constraint_schema, constraint).map_err(|error| {
        ConstraintError::InvalidDeclaration {
            message: error.to_string(),
        }
    })?;

    check_declared_keys(concrete_constraint_schema, constraint, "")?;

    let mut opaque_constraints = Vec::new();
    let schema_overlay = compile_object_constraint(
        target_schema,
        concrete_constraint_schema,
        constraint,
        "",
        &mut opaque_constraints,
    )?;

    Ok(ConstraintPlan {
        schema_overlay,
        opaque_constraints,
    })
}

fn compile_object_constraint(
    target_schema: &Value,
    constraint_schema: &Value,
    constraint: &Value,
    path: &str,
    opaque: &mut Vec<OpaqueConstraint>,
) -> Result<Value, ConstraintError> {
    let Some(constraint_object) = constraint.as_object() else {
        return Ok(Value::Object(Map::new()));
    };

    let target_properties = collect_properties(target_schema);
    let constraint_properties = collect_properties(constraint_schema);
    let mut overlay = Map::new();
    let mut overlay_properties = Map::new();

    if let Some(required) = constraint_object.get("required").and_then(Value::as_array) {
        let mut compiled_required = Vec::with_capacity(required.len());
        for property in required.iter().filter_map(Value::as_str) {
            if !target_properties.contains_key(property) {
                return Err(ConstraintError::UnknownRequiredProperty {
                    path: display_path(path),
                    property: property.to_string(),
                });
            }
            compiled_required.push(Value::String(property.to_string()));
        }
        if !compiled_required.is_empty() {
            overlay.insert("required".to_string(), Value::Array(compiled_required));
        }
    }

    for (key, value) in constraint_object {
        if key == "required" {
            continue;
        }

        let property_path = push_path(path, key);
        let property_constraint_schema = constraint_properties
            .get(key)
            .expect("check_declared_keys ensures every key is declared");
        let kind = constraint_kind(property_constraint_schema);

        let Some(target_property_schema) = target_properties.get(key) else {
            opaque.push(OpaqueConstraint {
                path: property_path,
                kind,
                value: value.clone(),
            });
            continue;
        };

        match kind {
            ConstraintKind::Object => {
                let child_overlay = compile_object_constraint(
                    target_property_schema,
                    property_constraint_schema,
                    value,
                    &property_path,
                    opaque,
                )?;
                overlay_properties.insert(key.clone(), child_overlay);
            }
            ConstraintKind::Value => {
                check_value_compatibility(target_property_schema, value, &property_path)?;
                overlay_properties.insert(key.clone(), value.clone());
            }
            ConstraintKind::Type | ConstraintKind::Custom => {
                opaque.push(OpaqueConstraint {
                    path: property_path,
                    kind,
                    value: value.clone(),
                });
            }
        }
    }

    if !overlay_properties.is_empty() {
        overlay.insert("properties".to_string(), Value::Object(overlay_properties));
    }

    Ok(Value::Object(overlay))
}

fn check_declared_keys(
    constraint_schema: &Value,
    constraint: &Value,
    path: &str,
) -> Result<(), ConstraintError> {
    let Some(object) = constraint.as_object() else {
        return Ok(());
    };
    let properties = collect_properties(constraint_schema);

    for (key, value) in object {
        let Some(property_schema) = properties.get(key) else {
            return Err(ConstraintError::UndeclaredKey {
                path: display_path(path),
                key: key.clone(),
            });
        };

        match constraint_kind(property_schema) {
            ConstraintKind::Object => {
                check_declared_keys(property_schema, value, &push_path(path, key))?;
            }
            ConstraintKind::Type => {
                check_type_constraint_keys(property_schema, value, &push_path(path, key))?;
            }
            ConstraintKind::Value | ConstraintKind::Custom => {}
        }
    }

    Ok(())
}

fn check_type_constraint_keys(
    type_schema: &Value,
    value: &Value,
    path: &str,
) -> Result<(), ConstraintError> {
    if value.is_array() {
        let Some(items_schema) = type_schema.get("items") else {
            return Ok(());
        };
        for (index, entry) in value.as_array().into_iter().flatten().enumerate() {
            check_type_constraint_keys(items_schema, entry, &push_path(path, &index.to_string()))?;
        }
        return Ok(());
    }

    let Some(object) = value.as_object() else {
        return Ok(());
    };
    let properties = collect_properties(type_schema);
    if let (Some(constraints), Some(constraints_schema)) =
        (object.get("constraints"), properties.get("constraints"))
    {
        check_declared_keys(
            constraints_schema,
            constraints,
            &push_path(path, "constraints"),
        )?;
    }
    Ok(())
}

fn check_value_compatibility(
    target_schema: &Value,
    value_constraint: &Value,
    path: &str,
) -> Result<(), ConstraintError> {
    let Some(object) = value_constraint.as_object() else {
        return Ok(());
    };

    if let Some(values) = object.get("enum").and_then(Value::as_array) {
        for value in values {
            validate_constraint_value(target_schema, value, path)?;
        }
    }
    if let Some(value) = object.get("const") {
        validate_constraint_value(target_schema, value, path)?;
    }
    Ok(())
}

fn validate_constraint_value(
    target_schema: &Value,
    value: &Value,
    path: &str,
) -> Result<(), ConstraintError> {
    validate_against_schema(target_schema, value).map_err(|error| {
        ConstraintError::IncompatibleValue {
            path: display_path(path),
            message: error.to_string(),
        }
    })
}

fn constraint_kind(schema: &Value) -> ConstraintKind {
    let mut kinds = BTreeSet::new();
    collect_constraint_kinds(schema, &mut kinds);

    if kinds.contains(OBJECT_CONSTRAINT_ID) {
        ConstraintKind::Object
    } else if kinds.contains(VALUE_CONSTRAINT_ID) {
        ConstraintKind::Value
    } else if kinds.contains(TYPE_CONSTRAINT_ID)
        || (schema.get("type").and_then(Value::as_str) == Some("array")
            && schema
                .get("items")
                .map(constraint_kind)
                .is_some_and(|kind| kind == ConstraintKind::Type))
    {
        ConstraintKind::Type
    } else {
        ConstraintKind::Custom
    }
}

fn collect_constraint_kinds(schema: &Value, kinds: &mut BTreeSet<&'static str>) {
    for key in ["$id", "$ref"] {
        if let Some(reference) = schema.get(key).and_then(Value::as_str) {
            for candidate in [
                OBJECT_CONSTRAINT_ID,
                VALUE_CONSTRAINT_ID,
                TYPE_CONSTRAINT_ID,
            ] {
                if reference.contains(candidate) {
                    kinds.insert(candidate);
                }
            }
        }
    }

    for keyword in ["allOf", "anyOf", "oneOf"] {
        if let Some(branches) = schema.get(keyword).and_then(Value::as_array) {
            for branch in branches {
                collect_constraint_kinds(branch, kinds);
            }
        }
    }
}

fn collect_properties(schema: &Value) -> Map<String, Value> {
    let mut properties = Map::new();
    collect_properties_into(schema, &mut properties);
    properties
}

fn collect_properties_into(schema: &Value, properties: &mut Map<String, Value>) {
    if let Some(schema_properties) = schema.get("properties").and_then(Value::as_object) {
        for (name, property) in schema_properties {
            if let Some(existing) = properties.remove(name) {
                properties.insert(
                    name.clone(),
                    serde_json::json!({ "allOf": [existing, property.clone()] }),
                );
            } else {
                properties.insert(name.clone(), property.clone());
            }
        }
    }

    for keyword in ["allOf", "anyOf", "oneOf"] {
        if let Some(branches) = schema.get(keyword).and_then(Value::as_array) {
            for branch in branches {
                collect_properties_into(branch, properties);
            }
        }
    }
}

fn push_path(path: &str, key: &str) -> String {
    let escaped = key.replace('~', "~0").replace('/', "~1");
    if path.is_empty() {
        format!("/{escaped}")
    } else {
        format!("{path}/{escaped}")
    }
}

fn display_path(path: &str) -> String {
    if path.is_empty() {
        "/".to_string()
    } else {
        path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::bundle_refs;
    use serde_json::json;
    use std::fs;

    fn object_constraint_schema() -> Value {
        json!({
            "$id": "https://ucp.dev/schemas/shopping/types/object_constraint.json",
            "type": "object",
            "properties": {
                "required": {
                    "type": "array",
                    "items": { "type": "string" },
                    "uniqueItems": true
                }
            }
        })
    }

    fn value_constraint_schema() -> Value {
        json!({
            "$id": "https://ucp.dev/schemas/shopping/types/value_constraint.json",
            "type": "object",
            "properties": {
                "enum": { "type": "array", "minItems": 1 },
                "const": {}
            },
            "anyOf": [
                { "required": ["enum"] },
                { "required": ["const"] }
            ],
            "additionalProperties": false
        })
    }

    fn type_constraint_schema() -> Value {
        json!({
            "$id": "https://ucp.dev/schemas/shopping/types/type_constraint.json",
            "type": "object",
            "required": ["type"],
            "properties": {
                "type": { "type": "string" },
                "constraints": object_constraint_schema()
            }
        })
    }

    fn concrete_constraint_schema() -> Value {
        json!({
            "allOf": [
                object_constraint_schema(),
                {
                    "type": "object",
                    "properties": {
                        "billing_address": {
                            "allOf": [
                                object_constraint_schema(),
                                {
                                    "properties": {
                                        "country": value_constraint_schema()
                                    }
                                }
                            ]
                        },
                        "country": value_constraint_schema(),
                        "derived_score": value_constraint_schema(),
                        "derived_context": {
                            "allOf": [
                                object_constraint_schema(),
                                {
                                    "properties": {
                                        "required": object_constraint_schema()["properties"]["required"].clone()
                                    }
                                }
                            ]
                        },
                        "alternatives": type_constraint_schema(),
                        "credential_options": {
                            "type": "array",
                            "items": type_constraint_schema()
                        },
                        "brands": {
                            "type": "array",
                            "items": { "type": "string" }
                        }
                    }
                }
            ]
        })
    }

    fn target_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "billing_address": {
                    "type": "object",
                    "properties": {
                        "country": { "type": "string" }
                    }
                },
                "country": { "type": "string" }
            }
        })
    }

    #[test]
    fn compiles_required_nested_object_and_value_constraints() {
        let constraint = json!({
            "required": ["billing_address"],
            "billing_address": {
                "required": ["country"],
                "country": { "enum": ["US", "CA"] }
            },
            "country": { "const": "US" }
        });

        let plan = compile_constraint(&target_schema(), &concrete_constraint_schema(), &constraint)
            .unwrap();

        assert_eq!(
            plan.schema_overlay,
            json!({
                "required": ["billing_address"],
                "properties": {
                    "billing_address": {
                        "required": ["country"],
                        "properties": {
                            "country": { "enum": ["US", "CA"] }
                        }
                    },
                    "country": { "const": "US" }
                }
            })
        );
        assert!(plan.opaque_constraints.is_empty());
    }

    #[test]
    fn rejects_unknown_required_property() {
        let error = compile_constraint(
            &target_schema(),
            &concrete_constraint_schema(),
            &json!({ "required": ["missing"] }),
        )
        .unwrap_err();

        assert_eq!(
            error,
            ConstraintError::UnknownRequiredProperty {
                path: "/".to_string(),
                property: "missing".to_string()
            }
        );
    }

    #[test]
    fn rejects_undeclared_constraint_key() {
        let error = compile_constraint(
            &target_schema(),
            &concrete_constraint_schema(),
            &json!({ "not_declared": true }),
        )
        .unwrap_err();

        assert_eq!(
            error,
            ConstraintError::UndeclaredKey {
                path: "/".to_string(),
                key: "not_declared".to_string()
            }
        );
    }

    #[test]
    fn accepts_non_property_constraints_as_opaque() {
        let constraint = json!({
            "derived_score": { "enum": ["low", "high"] },
            "derived_context": { "required": ["anything"] },
            "alternatives": { "type": "special" },
            "credential_options": [{ "type": "token" }],
            "brands": ["visa", "mastercard"]
        });

        let plan = compile_constraint(&target_schema(), &concrete_constraint_schema(), &constraint)
            .unwrap();

        assert_eq!(plan.schema_overlay, json!({}));
        assert_eq!(plan.opaque_constraints.len(), 5);
        assert_eq!(plan.opaque_constraints[0].path, "/derived_score");
        assert_eq!(plan.opaque_constraints[0].kind, ConstraintKind::Value);
        assert_eq!(plan.opaque_constraints[1].kind, ConstraintKind::Object);
        assert_eq!(plan.opaque_constraints[2].kind, ConstraintKind::Type);
        assert_eq!(plan.opaque_constraints[3].kind, ConstraintKind::Type);
        assert_eq!(plan.opaque_constraints[4].kind, ConstraintKind::Custom);
    }

    #[test]
    fn validates_custom_constraint_shape_without_evaluating_semantics() {
        let valid = compile_constraint(
            &target_schema(),
            &concrete_constraint_schema(),
            &json!({ "brands": ["visa", "mastercard"] }),
        );
        assert!(valid.is_ok());

        let invalid = compile_constraint(
            &target_schema(),
            &concrete_constraint_schema(),
            &json!({ "brands": "visa" }),
        );
        assert!(matches!(
            invalid,
            Err(ConstraintError::InvalidDeclaration { .. })
        ));
    }

    #[test]
    fn rejects_value_constraint_incompatible_with_target_type() {
        let error = compile_constraint(
            &target_schema(),
            &concrete_constraint_schema(),
            &json!({ "country": { "enum": [1, 2] } }),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            ConstraintError::IncompatibleValue { path, .. } if path == "/country"
        ));
    }

    #[test]
    fn rejects_unsupported_value_constraint_keyword() {
        let error = compile_constraint(
            &target_schema(),
            &concrete_constraint_schema(),
            &json!({ "country": { "minLength": 2 } }),
        )
        .unwrap_err();

        assert!(matches!(error, ConstraintError::InvalidDeclaration { .. }));
    }

    #[test]
    fn merges_repeated_property_definitions_across_allof() {
        let schema = json!({
            "allOf": [
                {
                    "properties": {
                        "constraints": {
                            "allOf": [object_constraint_schema()],
                            "properties": {
                                "base_key": { "type": "boolean" }
                            }
                        }
                    }
                },
                {
                    "properties": {
                        "constraints": {
                            "properties": {
                                "extension_key": { "type": "string" }
                            }
                        }
                    }
                }
            ]
        });

        let constraints = collect_properties(&schema).remove("constraints").unwrap();
        let properties = collect_properties(&constraints);
        assert!(properties.contains_key("required"));
        assert!(properties.contains_key("base_key"));
        assert!(properties.contains_key("extension_key"));
        assert_eq!(constraint_kind(&constraints), ConstraintKind::Object);
    }

    #[test]
    fn discovers_constraint_kinds_after_external_refs_are_bundled() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("object_constraint.json"),
            serde_json::to_vec(&object_constraint_schema()).unwrap(),
        )
        .unwrap();
        fs::write(
            directory.path().join("value_constraint.json"),
            serde_json::to_vec(&value_constraint_schema()).unwrap(),
        )
        .unwrap();

        let mut concrete = json!({
            "allOf": [
                { "$ref": "object_constraint.json" },
                {
                    "properties": {
                        "country": { "$ref": "value_constraint.json" },
                        "derived": { "$ref": "value_constraint.json" }
                    }
                }
            ]
        });
        bundle_refs(&mut concrete, directory.path()).unwrap();

        let plan = compile_constraint(
            &target_schema(),
            &concrete,
            &json!({
                "country": { "enum": ["US", "CA"] },
                "derived": { "const": "custom" }
            }),
        )
        .unwrap();

        assert_eq!(
            plan.schema_overlay,
            json!({
                "properties": {
                    "country": { "enum": ["US", "CA"] }
                }
            })
        );
        assert_eq!(plan.opaque_constraints.len(), 1);
        assert_eq!(plan.opaque_constraints[0].path, "/derived");
        assert_eq!(plan.opaque_constraints[0].kind, ConstraintKind::Value);
    }
}
