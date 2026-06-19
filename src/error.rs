//! Error types for UCP schema resolution and validation.

use std::path::PathBuf;
use thiserror::Error;

/// Errors during schema composition from UCP capability metadata.
#[derive(Debug, Error)]
pub enum ComposeError {
    #[error("payload is not self-describing: missing ucp.capabilities (response) or meta.profile (request)")]
    NotSelfDescribing,

    #[error("no capabilities declared in ucp.capabilities")]
    EmptyCapabilities,

    #[error("invalid JSONRPC envelope: {message}")]
    InvalidEnvelope { message: String },

    #[error("no root capability found (all capabilities have 'extends')")]
    NoRootCapability,

    #[error("multiple root capabilities found: {}", names.join(", "))]
    MultipleRootCapabilities { names: Vec<String> },

    #[error("extension '{extension}' references unknown parent '{parent}'")]
    UnknownParent { extension: String, parent: String },

    #[error("extension '{extension}' does not connect to root '{root}'")]
    OrphanExtension { extension: String, root: String },

    #[error("extension '{extension}' missing $defs entry for '{expected_key}'")]
    MissingDefEntry {
        extension: String,
        expected_key: String,
    },

    /// A container-shaped capability (request/response shapes live under
    /// `$defs/{op}_{direction}`) is extended by a schema whose `$defs[<capability>]`
    /// is not itself a container of operation shapes. Container extensions MUST
    /// mirror the base's operation keys (e.g. `{ "$defs": { "search_response": ... } }`).
    #[error(
        "extension '{extension}' does not mirror container capability '{capability}': \
         its $defs['{capability}'] must contain a nested $defs of operation shapes"
    )]
    ContainerExtensionShape {
        extension: String,
        capability: String,
    },

    #[error("failed to fetch schema from {url}: {message}")]
    SchemaFetch { url: String, message: String },

    #[error("failed to fetch profile from {url}: {message}")]
    ProfileFetch { url: String, message: String },

    #[error("invalid capability '{name}': {message}")]
    InvalidCapability { name: String, message: String },

    #[error("invalid URL '{url}': {message}")]
    InvalidUrl { url: String, message: String },

    #[error("extension '{extension}' requires {target} {range} but found {actual}")]
    VersionConstraintViolation {
        extension: String,
        target: String,
        range: String,
        actual: String,
    },

    #[error("capability '{capability}' fails namespace authority binding: {message}")]
    NamespaceBindingViolation { capability: String, message: String },
}

impl ComposeError {
    /// Returns the exit code for this error type.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::SchemaFetch { .. } | Self::ProfileFetch { .. } => 3, // IO
            _ => 2,                                                    // Schema/composition error
        }
    }
}

/// Errors during schema resolution.
#[derive(Debug, Error)]
pub enum ResolveError {
    // IO errors (exit code 3)
    #[error("file not found: {path}")]
    FileNotFound { path: PathBuf },

    #[error("cannot read {path}: {source}")]
    ReadError {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[cfg(feature = "remote")]
    #[error("failed to fetch {url}: {source}")]
    NetworkError {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    // Parse errors (exit code 2)
    #[error("invalid JSON: {source}")]
    InvalidJson {
        #[source]
        source: serde_json::Error,
    },

    // Schema errors (exit code 2)
    #[error("invalid annotation at {path}: expected string or object, got {actual}")]
    InvalidAnnotationType { path: String, actual: String },

    #[error("unknown visibility \"{value}\" at {path}: expected omit, required, or optional")]
    UnknownVisibility { path: String, value: String },

    #[error("invalid schema transition at {path}: {message}")]
    InvalidSchemaTransition { path: String, message: String },

    /// allOf extension tries to weaken a field that base declares as required.
    /// Monotonicity rule: extensions can narrow (optional→omit) or strengthen
    /// (optional→required) but never weaken required fields.
    #[error(
        "monotonicity violation at {path}: field \"{field}\" is {base_status} in base schema \
         but extension sets it to \"{attempted}\""
    )]
    MonotonicityViolation {
        path: String,
        field: String,
        base_status: String,
        attempted: String,
    },

    /// allOf branches declare contradictory types on the same property.
    #[error(
        "type conflict at {path}: base declares \"{base_type}\" but extension declares \"{ext_type}\""
    )]
    TypeConflict {
        path: String,
        base_type: String,
        ext_type: String,
    },

    #[error("invalid schema: {message}")]
    InvalidSchema { message: String },

    /// A container-shaped capability schema has no message body for the
    /// requested `(op, direction)`. The body lives at `$defs/{op}_{direction}`;
    /// because a container root has no body of its own, an absent key is a hard
    /// error rather than a fall-through to an unconstrained root.
    #[error(
        "container schema has no operation shape '{key}' for this (op, direction); \
         available operation shapes: [{available}]"
    )]
    OperationShapeNotFound { key: String, available: String },

    /// An explicit `--def` / `def_name` selector names a `$defs` entry that the
    /// resolved schema does not contain. Used for non-derivable shapes (transport
    /// message types, host views, sub-types) where the name is authored, not
    /// computed from `(op, direction)`.
    #[error("schema has no $defs entry '{def}'; available: [{available}]")]
    DefNotFound { def: String, available: String },

    #[error("failed to bundle schema: {message}")]
    BundleError { message: String },
}

/// Errors during validation.
#[derive(Debug, Error)]
pub enum ValidateError {
    #[error(transparent)]
    Resolve(#[from] ResolveError),

    #[error("validation failed with {} error(s)", errors.len())]
    Invalid { errors: Vec<SchemaError> },
}

/// Single validation error with path context.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SchemaError {
    /// JSON Pointer (RFC 6901) to the invalid field.
    pub path: String,
    /// Human-readable error message.
    pub message: String,
}

impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

impl ResolveError {
    /// Returns the exit code for this error type.
    pub fn exit_code(&self) -> i32 {
        match self {
            ResolveError::FileNotFound { .. } | ResolveError::ReadError { .. } => 3,
            #[cfg(feature = "remote")]
            ResolveError::NetworkError { .. } => 3,
            _ => 2,
        }
    }
}

impl ValidateError {
    /// Returns the exit code for this error type.
    pub fn exit_code(&self) -> i32 {
        match self {
            ValidateError::Resolve(e) => e.exit_code(),
            ValidateError::Invalid { .. } => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_error_exit_codes() {
        let err = ResolveError::FileNotFound {
            path: PathBuf::from("test.json"),
        };
        assert_eq!(err.exit_code(), 3);

        let err = ResolveError::InvalidAnnotationType {
            path: "/properties/id".into(),
            actual: "number".into(),
        };
        assert_eq!(err.exit_code(), 2);

        let err = ResolveError::UnknownVisibility {
            path: "/properties/id".into(),
            value: "readonly".into(),
        };
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn validate_error_exit_codes() {
        let err = ValidateError::Invalid {
            errors: vec![SchemaError {
                path: "/id".into(),
                message: "missing required field".into(),
            }],
        };
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn schema_error_display() {
        let err = SchemaError {
            path: "/buyer/email".into(),
            message: "expected string, got number".into(),
        };
        assert_eq!(err.to_string(), "/buyer/email: expected string, got number");
    }
}
