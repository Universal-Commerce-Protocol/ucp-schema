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

    #[error("failed to fetch schema from {url}: {message}")]
    SchemaFetch { url: String, message: String },

    #[error("failed to fetch profile from {url}: {message}")]
    ProfileFetch { url: String, message: String },

    #[error("invalid capability '{name}': {message}")]
    InvalidCapability { name: String, message: String },

    #[error("invalid URL '{url}': {message}")]
    InvalidUrl { url: String, message: String },
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

    #[error("invalid schema: {message}")]
    InvalidSchema { message: String },

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
