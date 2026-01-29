//! UCP Schema Resolver
//!
//! Runtime resolution of `ucp_request` and `ucp_response` annotations.
//!
//! This library transforms JSON Schemas with UCP annotations into standard JSON Schemas
//! based on direction (request/response) and operation (create, read, update, etc.).
//!
//! # Example
//!
//! ```
//! use ucp_schema::{resolve, Direction, ResolveOptions};
//! use serde_json::json;
//!
//! let schema = json!({
//!     "type": "object",
//!     "properties": {
//!         "id": {
//!             "type": "string",
//!             "ucp_request": {
//!                 "create": "omit",
//!                 "update": "required"
//!             }
//!         },
//!         "name": { "type": "string" }
//!     }
//! });
//!
//! let options = ResolveOptions::new(Direction::Request, "create");
//! let resolved = resolve(&schema, &options).unwrap();
//!
//! // In the resolved schema, "id" is omitted for create requests
//! assert!(resolved["properties"].get("id").is_none());
//! assert!(resolved["properties"].get("name").is_some());
//! ```
//!
//! # Visibility Rules
//!
//! | Visibility | Effect on `properties` | Effect on `required` |
//! |------------|------------------------|----------------------|
//! | `"omit"` | Remove field | Remove from required |
//! | `"required"` | Keep field | Add to required |
//! | `"optional"` | Keep field | Remove from required |
//! | (none) | Keep field | Preserve original |
//!
//! # Annotation Format
//!
//! Annotations can be shorthand (applies to all operations):
//! ```json
//! { "ucp_request": "omit" }
//! ```
//!
//! Or per-operation:
//! ```json
//! { "ucp_request": { "create": "omit", "update": "required" } }
//! ```

mod compose;
mod error;
mod linter;
mod loader;
mod resolver;
mod types;
mod validator;

pub use compose::{
    capability_short_name, compose_from_payload, compose_schema, detect_direction,
    extract_capabilities, extract_capabilities_from_profile, extract_jsonrpc_payload, Capability,
    DetectedDirection, SchemaBaseConfig,
};
pub use error::{ComposeError, ResolveError, SchemaError, ValidateError};
pub use linter::{lint, lint_file, Diagnostic, FileResult, FileStatus, LintResult, Severity};
pub use loader::{
    bundle_refs, bundle_refs_with_url_mapping, is_url, load_schema, load_schema_auto,
    load_schema_str, navigate_fragment,
};
pub use resolver::{resolve, strip_annotations};
pub use types::{Direction, ResolveOptions, Visibility};
pub use validator::{validate, validate_against_schema};

#[cfg(feature = "remote")]
pub use loader::{bundle_refs_remote, load_schema_url};
