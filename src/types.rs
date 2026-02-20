//! Core types for UCP schema resolution.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Schema transition: from/to are visibility values (omit, optional, required).
/// During the transition period the field is always the `from` visibility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaTransitionInfo {
    pub from: String,
    pub to: String,
    pub description: String,
}

/// Valid UCP operations for annotation object form.
pub const VALID_OPERATIONS: &[&str] = &["create", "update", "complete", "read"];

/// UCP annotation keys.
pub const UCP_ANNOTATIONS: &[&str] = &["ucp_request", "ucp_response"];

/// Returns the JSON type name for error messages.
pub fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Direction of the schema transformation.
///
/// Determines whether to use `ucp_request` or `ucp_response` annotations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Request,
    Response,
}

impl Direction {
    /// Returns the annotation key for this direction.
    pub fn annotation_key(&self) -> &'static str {
        match self {
            Direction::Request => "ucp_request",
            Direction::Response => "ucp_response",
        }
    }

    /// Create direction from a request flag (true = Request, false = Response).
    pub fn from_request_flag(is_request: bool) -> Self {
        if is_request {
            Direction::Request
        } else {
            Direction::Response
        }
    }
}

/// Visibility of a field after resolution.
///
/// Determines how a field is transformed in the output schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Visibility {
    /// No transformation - keep field as-is with original required status.
    #[default]
    Include,
    /// Remove field from properties and required array.
    Omit,
    /// Keep field and ensure it's in the required array.
    Required,
    /// Keep field but remove from required array.
    Optional,
}

impl Visibility {
    /// Parse a visibility value from a string.
    ///
    /// Returns `None` for unknown values (caller should error).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "omit" => Some(Visibility::Omit),
            "required" => Some(Visibility::Required),
            "optional" => Some(Visibility::Optional),
            _ => None,
        }
    }
}

/// Returns true if (from, to) is a valid schema transition: both are visibility
/// values (omit, optional, required) and from != to.
pub fn is_valid_schema_transition(from: &str, to: &str) -> bool {
    from != to && Visibility::parse(from).is_some() && Visibility::parse(to).is_some()
}

/// Options for schema resolution.
#[derive(Debug, Clone)]
pub struct ResolveOptions {
    /// Whether resolving for request or response.
    pub direction: Direction,
    /// The operation to resolve for (e.g., "create", "update").
    /// Will be normalized to lowercase.
    pub operation: String,
    /// When true, sets `additionalProperties: false` on all object schemas
    /// to reject unknown fields. Defaults to false to respect schema extensibility.
    pub strict: bool,
}

impl ResolveOptions {
    /// Create new resolve options with strict mode disabled (default).
    ///
    /// Operation is normalized to lowercase for case-insensitive matching.
    /// Strict mode is off by default to respect UCP's extensibility model:
    /// schemas validate known fields but allow additional properties.
    pub fn new(direction: Direction, operation: impl Into<String>) -> Self {
        Self {
            direction,
            operation: operation.into().to_lowercase(),
            strict: false,
        }
    }

    /// Set strict mode (additionalProperties: false on all objects).
    pub fn strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_annotation_key() {
        assert_eq!(Direction::Request.annotation_key(), "ucp_request");
        assert_eq!(Direction::Response.annotation_key(), "ucp_response");
    }

    #[test]
    fn visibility_parse_valid() {
        assert_eq!(Visibility::parse("omit"), Some(Visibility::Omit));
        assert_eq!(Visibility::parse("required"), Some(Visibility::Required));
        assert_eq!(Visibility::parse("optional"), Some(Visibility::Optional));
    }

    #[test]
    fn visibility_parse_invalid() {
        assert_eq!(Visibility::parse("include"), None);
        assert_eq!(Visibility::parse("readonly"), None);
        assert_eq!(Visibility::parse(""), None);
    }

    #[test]
    fn valid_schema_transitions() {
        // Any distinct pair of visibility values is valid
        for (from, to) in [
            ("required", "optional"),
            ("required", "omit"),
            ("optional", "omit"),
            ("optional", "required"),
            ("omit", "required"),
            ("omit", "optional"),
        ] {
            assert!(super::is_valid_schema_transition(from, to));
        }
        // Disbarred: same value for both
        assert!(!super::is_valid_schema_transition("required", "required"));
        assert!(!super::is_valid_schema_transition("omit", "omit"));
        assert!(!super::is_valid_schema_transition("optional", "optional"));
        // Disbarred: invalid visibility value
        assert!(!super::is_valid_schema_transition("readonly", "omit"));
        assert!(!super::is_valid_schema_transition("required", "invalid"));
    }

    #[test]
    fn resolve_options_normalizes_operation() {
        let opts = ResolveOptions::new(Direction::Request, "Create");
        assert_eq!(opts.operation, "create");

        let opts = ResolveOptions::new(Direction::Request, "UPDATE");
        assert_eq!(opts.operation, "update");
    }
}
