//! Schema composition from UCP capability metadata.
//!
//! UCP payloads are self-describing: they embed capability metadata that declares
//! which schemas apply. This module extracts that metadata and composes the
//! appropriate schema for validation.
//!
//! # Response Pattern
//!
//! Responses have `ucp.capabilities` inline:
//! ```json
//! {
//!   "ucp": {
//!     "capabilities": {
//!       "dev.ucp.shopping.checkout": [{ "version": "...", "schema": "..." }]
//!     }
//!   }
//! }
//! ```
//!
//! # Request Pattern (JSONRPC)
//!
//! JSONRPC requests have `meta.profile` at root, with payload nested under
//! the capability short name (last segment of the dotted capability name):
//! ```json
//! {
//!   "meta": {
//!     "profile": "https://agent.example.com/.well-known/ucp"
//!   },
//!   "checkout": {
//!     "line_items": [...]
//!   }
//! }
//! ```
//!
//! # Request Pattern (REST)
//!
//! REST requests pass the profile URL via HTTP header (`UCP-Agent`), with
//! the payload being the raw checkout object. Use `--profile` CLI flag to
//! simulate this pattern.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::{json, Value};

use crate::error::ComposeError;
use crate::loader::{bundle_refs, bundle_refs_with_url_mapping, is_url, load_schema};
use crate::types::Direction;

#[cfg(feature = "remote")]
use crate::loader::{bundle_refs_remote, load_schema_url};

/// Configuration for mapping schema URLs to local paths.
///
/// When both `local_base` and `remote_base` are set, URLs starting with
/// `remote_base` have that prefix stripped before joining with `local_base`.
///
/// Example:
/// - `remote_base`: `https://ucp.dev/draft`
/// - `local_base`: `source`
/// - URL: `https://ucp.dev/draft/schemas/checkout.json`
/// - Result: `source/schemas/checkout.json`
#[derive(Debug, Clone, Default)]
pub struct SchemaBaseConfig<'a> {
    /// Local directory containing schema files.
    pub local_base: Option<&'a Path>,
    /// URL prefix to strip when mapping to local paths.
    pub remote_base: Option<&'a str>,
}

/// Capability declaration extracted from UCP metadata.
#[derive(Debug, Clone)]
pub struct Capability {
    /// Reverse-domain capability name (e.g., "dev.ucp.shopping.checkout").
    pub name: String,
    /// Version string (e.g., "2026-01-11").
    pub version: String,
    /// URL to the JSON Schema for this capability.
    pub schema_url: String,
    /// Parent capability names this extends. None for root capabilities.
    pub extends: Option<Vec<String>>,
}

/// Detected payload direction based on UCP metadata structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectedDirection {
    /// Payload has `ucp.capabilities` inline (response pattern).
    Response,
    /// Payload has `meta.profile` at root (JSONRPC request pattern).
    Request,
}

impl From<DetectedDirection> for Direction {
    fn from(d: DetectedDirection) -> Self {
        match d {
            DetectedDirection::Response => Direction::Response,
            DetectedDirection::Request => Direction::Request,
        }
    }
}

/// Detect direction from payload structure.
///
/// Returns `Some(Response)` if `ucp.capabilities` exists,
/// `Some(Request)` if `meta.profile` exists at root (JSONRPC pattern),
/// `None` if neither is present.
pub fn detect_direction(payload: &Value) -> Option<DetectedDirection> {
    // Response pattern: ucp.capabilities
    if let Some(ucp) = payload.get("ucp") {
        if ucp.get("capabilities").is_some() {
            return Some(DetectedDirection::Response);
        }
    }

    // JSONRPC request pattern: meta.profile at root (NOT ucp.meta.profile)
    if payload.get("meta").and_then(|m| m.get("profile")).is_some() {
        return Some(DetectedDirection::Request);
    }

    None
}

/// Extract capabilities from a self-describing payload.
///
/// - Response: extracts from `ucp.capabilities` directly
/// - JSONRPC Request: fetches `meta.profile` URL, extracts from profile
///
/// # Arguments
/// * `payload` - The UCP payload to extract capabilities from
/// * `schema_base` - Configuration for mapping schema URLs to local paths
pub fn extract_capabilities(
    payload: &Value,
    schema_base: &SchemaBaseConfig,
) -> Result<Vec<Capability>, ComposeError> {
    // Try response pattern first: ucp.capabilities
    if let Some(ucp) = payload.get("ucp") {
        if let Some(caps) = ucp.get("capabilities") {
            return parse_capabilities_object(caps);
        }
    }

    // Try JSONRPC request pattern: meta.profile at root
    if let Some(profile_url) = payload
        .get("meta")
        .and_then(|m| m.get("profile"))
        .and_then(|p| p.as_str())
    {
        return extract_capabilities_from_profile(profile_url, schema_base);
    }

    Err(ComposeError::NotSelfDescribing)
}

/// Extract capabilities from a profile URL.
///
/// Used for both JSONRPC requests (meta.profile) and REST requests (--profile flag).
pub fn extract_capabilities_from_profile(
    profile_url: &str,
    schema_base: &SchemaBaseConfig,
) -> Result<Vec<Capability>, ComposeError> {
    let profile = fetch_profile(profile_url, schema_base)?;
    let caps = profile
        .get("ucp")
        .and_then(|u| u.get("capabilities"))
        .ok_or_else(|| ComposeError::ProfileFetch {
            url: profile_url.to_string(),
            message: "profile missing ucp.capabilities".to_string(),
        })?;
    parse_capabilities_object(caps)
}

/// Extract the actual payload from a JSONRPC request envelope.
///
/// JSONRPC requests have the structure: `{meta: {...}, <capability_key>: <payload>}`
/// The capability key is the short name (last segment) of the root capability.
///
/// # Arguments
/// * `envelope` - The full JSONRPC request envelope
/// * `capabilities` - Capabilities extracted from the profile
///
/// # Returns
/// The payload value and the capability key name used
pub fn extract_jsonrpc_payload<'a>(
    envelope: &'a Value,
    capabilities: &[Capability],
) -> Result<(&'a Value, String), ComposeError> {
    // Find root capability (no extends)
    let root = capabilities
        .iter()
        .find(|c| c.extends.is_none())
        .ok_or(ComposeError::NoRootCapability)?;

    // Derive short name from capability name (last segment of dotted name)
    let short_name = capability_short_name(&root.name);

    // Extract payload from envelope using short name as key
    let payload = envelope
        .get(&short_name)
        .ok_or_else(|| ComposeError::InvalidEnvelope {
            message: format!(
                "JSONRPC envelope missing '{}' key (derived from capability '{}')",
                short_name, root.name
            ),
        })?;

    Ok((payload, short_name))
}

/// Derive short name from a capability name.
///
/// Takes the last segment of a dotted capability name.
/// E.g., "dev.ucp.shopping.checkout" -> "checkout"
pub fn capability_short_name(name: &str) -> String {
    name.rsplit('.').next().unwrap_or(name).to_string()
}

/// Parse a capabilities object into a list of Capability structs.
fn parse_capabilities_object(caps: &Value) -> Result<Vec<Capability>, ComposeError> {
    let obj = caps.as_object().ok_or(ComposeError::EmptyCapabilities)?;

    if obj.is_empty() {
        return Err(ComposeError::EmptyCapabilities);
    }

    let mut capabilities = Vec::new();

    for (name, versions) in obj {
        // Each capability is an array of version entries
        let entries = versions
            .as_array()
            .ok_or_else(|| ComposeError::InvalidCapability {
                name: name.clone(),
                message: "expected array of capability entries".to_string(),
            })?;

        // Take the first entry (version negotiation already happened)
        let entry = entries
            .first()
            .ok_or_else(|| ComposeError::InvalidCapability {
                name: name.clone(),
                message: "empty capability array".to_string(),
            })?;

        let version = entry
            .get("version")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ComposeError::InvalidCapability {
                name: name.clone(),
                message: "missing version field".to_string(),
            })?
            .to_string();

        let schema_url = entry
            .get("schema")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ComposeError::InvalidCapability {
                name: name.clone(),
                message: "missing schema field".to_string(),
            })?
            .to_string();

        // extends can be string or array of strings
        let extends = match entry.get("extends") {
            None => None,
            Some(Value::String(s)) => Some(vec![s.clone()]),
            Some(Value::Array(arr)) => {
                let parents: Result<Vec<String>, _> = arr
                    .iter()
                    .map(|v| {
                        v.as_str().map(|s| s.to_string()).ok_or_else(|| {
                            ComposeError::InvalidCapability {
                                name: name.clone(),
                                message: "extends array must contain strings".to_string(),
                            }
                        })
                    })
                    .collect();
                Some(parents?)
            }
            Some(_) => {
                return Err(ComposeError::InvalidCapability {
                    name: name.clone(),
                    message: "extends must be string or array of strings".to_string(),
                });
            }
        };

        capabilities.push(Capability {
            name: name.clone(),
            version,
            schema_url,
            extends,
        });
    }

    Ok(capabilities)
}

/// Fetch a profile from a URL or local path.
fn fetch_profile(url: &str, schema_base: &SchemaBaseConfig) -> Result<Value, ComposeError> {
    resolve_schema_url(url, schema_base).map_err(|e| ComposeError::ProfileFetch {
        url: url.to_string(),
        message: e.to_string(),
    })
}

/// Compose schema from capability declarations.
///
/// 1. Finds root capability (no extends)
/// 2. Validates graph connectivity
/// 3. Fetches schemas and extracts $defs[root] entries
/// 4. Composes using allOf
pub fn compose_schema(
    capabilities: &[Capability],
    schema_base: &SchemaBaseConfig,
) -> Result<Value, ComposeError> {
    if capabilities.is_empty() {
        return Err(ComposeError::EmptyCapabilities);
    }

    // Build name -> capability map for lookups
    let cap_map: HashMap<&str, &Capability> =
        capabilities.iter().map(|c| (c.name.as_str(), c)).collect();

    // Find root capability (no extends)
    let roots: Vec<&Capability> = capabilities
        .iter()
        .filter(|c| c.extends.is_none())
        .collect();

    let root = match roots.len() {
        0 => return Err(ComposeError::NoRootCapability),
        1 => roots[0],
        _ => {
            return Err(ComposeError::MultipleRootCapabilities {
                names: roots.iter().map(|c| c.name.clone()).collect(),
            })
        }
    };

    // Validate graph: all extends references must exist in capabilities
    for cap in capabilities {
        if let Some(parents) = &cap.extends {
            for parent in parents {
                if !cap_map.contains_key(parent.as_str()) {
                    return Err(ComposeError::UnknownParent {
                        extension: cap.name.clone(),
                        parent: parent.clone(),
                    });
                }
            }
        }
    }

    // Validate graph connectivity: all extensions must reach root
    for cap in capabilities {
        if cap.extends.is_some() && !reaches_root(cap, &cap_map, &root.name) {
            return Err(ComposeError::OrphanExtension {
                extension: cap.name.clone(),
                root: root.name.clone(),
            });
        }
    }

    // Get extensions (all non-root capabilities)
    let extensions: Vec<&Capability> = capabilities
        .iter()
        .filter(|c| c.extends.is_some())
        .collect();

    // If no extensions, just return the root schema
    if extensions.is_empty() {
        return resolve_schema_url(&root.schema_url, schema_base).map_err(|e| {
            ComposeError::SchemaFetch {
                url: root.schema_url.clone(),
                message: e.to_string(),
            }
        });
    }

    // Compose: for each extension, extract $defs[root.name]
    let mut all_of_schemas = Vec::new();

    for ext in &extensions {
        let ext_schema = resolve_schema_url(&ext.schema_url, schema_base).map_err(|e| {
            ComposeError::SchemaFetch {
                url: ext.schema_url.clone(),
                message: e.to_string(),
            }
        })?;

        // Extract $defs[root.name] and inline any internal refs
        let defs = ext_schema
            .get("$defs")
            .ok_or_else(|| ComposeError::MissingDefEntry {
                extension: ext.name.clone(),
                expected_key: root.name.clone(),
            })?;

        let ext_def = defs
            .get(&root.name)
            .ok_or_else(|| ComposeError::MissingDefEntry {
                extension: ext.name.clone(),
                expected_key: root.name.clone(),
            })?;

        // Inline internal #/$defs/... refs so the extracted def is self-contained
        let mut inlined = ext_def.clone();
        inline_internal_refs(&mut inlined, defs);

        all_of_schemas.push(inlined);
    }

    // Compose into single schema with allOf
    Ok(json!({ "allOf": all_of_schemas }))
}

/// Inline internal `#/$defs/...` refs from the parent schema.
///
/// When extracting a single definition from a schema, that definition may have
/// internal refs to other definitions in the same schema. This function
/// recursively inlines those refs so the extracted definition is self-contained.
///
/// # Arguments
/// * `value` - The value to process (modified in place)
/// * `defs` - The `$defs` object to resolve refs against
fn inline_internal_refs(value: &mut Value, defs: &Value) {
    inline_internal_refs_inner(value, defs, &mut HashSet::new());
}

fn inline_internal_refs_inner(value: &mut Value, defs: &Value, visited: &mut HashSet<String>) {
    match value {
        Value::Object(obj) => {
            // Check if this object has an internal $ref
            if let Some(ref_val) = obj.get("$ref").and_then(|v| v.as_str()) {
                // Only handle internal refs to $defs (not self-root "#" refs)
                if let Some(def_name) = ref_val.strip_prefix("#/$defs/") {
                    // Guard against circular refs
                    if visited.contains(def_name) {
                        return;
                    }

                    // Look up the definition
                    if let Some(def) = defs.get(def_name) {
                        visited.insert(def_name.to_string());

                        // Clone and recursively inline
                        let mut inlined = def.clone();
                        inline_internal_refs_inner(&mut inlined, defs, visited);

                        visited.remove(def_name);

                        // Replace the $ref object with the inlined definition
                        obj.remove("$ref");
                        if let Value::Object(def_obj) = inlined {
                            for (k, v) in def_obj {
                                obj.entry(k).or_insert(v);
                            }
                        }
                        return;
                    }
                }
            }

            // Recurse into all values
            for v in obj.values_mut() {
                inline_internal_refs_inner(v, defs, visited);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                inline_internal_refs_inner(item, defs, visited);
            }
        }
        _ => {}
    }
}

/// Check if a capability transitively reaches the root via extends chain.
fn reaches_root(cap: &Capability, cap_map: &HashMap<&str, &Capability>, root_name: &str) -> bool {
    let mut visited = HashSet::new();
    let mut queue = vec![cap];

    while let Some(current) = queue.pop() {
        if visited.contains(&current.name.as_str()) {
            continue;
        }
        visited.insert(current.name.as_str());

        if let Some(parents) = &current.extends {
            for parent_name in parents {
                if parent_name == root_name {
                    return true;
                }
                if let Some(parent) = cap_map.get(parent_name.as_str()) {
                    queue.push(parent);
                }
            }
        }
    }

    false
}

/// Convenience: extract capabilities and compose schema in one call.
pub fn compose_from_payload(
    payload: &Value,
    schema_base: &SchemaBaseConfig,
) -> Result<Value, ComposeError> {
    let capabilities = extract_capabilities(payload, schema_base)?;
    compose_schema(&capabilities, schema_base)
}

/// Resolve a schema URL to a Value, bundling any $ref pointers.
///
/// If `schema_base.local_base` is provided, maps URL paths to local files.
/// If `schema_base.remote_base` is also provided, strips that prefix from URLs
/// before mapping (enables versioned URL to unversioned local path mapping).
/// Otherwise, fetches via HTTP.
///
/// After loading, bundles external $ref pointers so the schema is self-contained.
/// This is necessary because extension schemas often have relative refs like
/// `$ref: "checkout.json"` that need resolution before composition.
fn resolve_schema_url(url: &str, schema_base: &SchemaBaseConfig) -> Result<Value, ComposeError> {
    if let Some(base) = schema_base.local_base {
        // Map URL to local path
        let path = if let Some(remote_base) = schema_base.remote_base {
            // Strip remote_base prefix if URL starts with it
            if let Some(remainder) = url.strip_prefix(remote_base) {
                // remainder is like "/schemas/checkout.json"
                remainder.to_string()
            } else {
                // URL doesn't match remote_base, fall back to extracting path
                extract_url_path(url)?
            }
        } else {
            // No remote_base, extract path portion of URL
            extract_url_path(url)?
        };

        let local_path = base.join(path.trim_start_matches('/'));
        let mut schema = load_schema(&local_path).map_err(|_| ComposeError::SchemaFetch {
            url: url.to_string(),
            message: format!("file not found: {}", local_path.display()),
        })?;

        // Bundle refs - use URL-aware version if remote mapping is configured
        let schema_dir = local_path.parent().unwrap_or(base);
        if let Some(remote_base) = schema_base.remote_base {
            // URL mapping configured - internal refs may also be absolute URLs
            bundle_refs_with_url_mapping(&mut schema, schema_dir, base, remote_base).map_err(
                |e| ComposeError::SchemaFetch {
                    url: url.to_string(),
                    message: format!("bundling refs: {}", e),
                },
            )?;
        } else {
            // No URL mapping - use simple relative path bundling
            bundle_refs(&mut schema, schema_dir).map_err(|e| ComposeError::SchemaFetch {
                url: url.to_string(),
                message: format!("bundling refs: {}", e),
            })?;
        }

        Ok(schema)
    } else if is_url(url) {
        // HTTP fetch with remote bundling
        #[cfg(feature = "remote")]
        {
            let mut schema = load_schema_url(url).map_err(|e| ComposeError::SchemaFetch {
                url: url.to_string(),
                message: e.to_string(),
            })?;

            // Bundle refs using the URL as base for resolving relative refs
            bundle_refs_remote(&mut schema, url).map_err(|e| ComposeError::SchemaFetch {
                url: url.to_string(),
                message: format!("bundling refs: {}", e),
            })?;

            Ok(schema)
        }
        #[cfg(not(feature = "remote"))]
        {
            Err(ComposeError::SchemaFetch {
                url: url.to_string(),
                message: "HTTP fetching requires 'remote' feature".to_string(),
            })
        }
    } else {
        // Treat as local file path
        let local_path = Path::new(url);
        let mut schema = load_schema(local_path).map_err(|e| ComposeError::SchemaFetch {
            url: url.to_string(),
            message: e.to_string(),
        })?;

        // Bundle refs using the schema's directory as base
        if let Some(schema_dir) = local_path.parent() {
            bundle_refs(&mut schema, schema_dir).map_err(|e| ComposeError::SchemaFetch {
                url: url.to_string(),
                message: format!("bundling refs: {}", e),
            })?;
        }

        Ok(schema)
    }
}

/// Extract the path portion from a URL.
///
/// E.g., "https://ucp.dev/schemas/shopping/checkout.json" -> "/schemas/shopping/checkout.json"
fn extract_url_path(url: &str) -> Result<String, ComposeError> {
    // Try stripping http:// or https:// prefix
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));

    match rest {
        Some(after_scheme) => {
            // URL with scheme - extract path after host
            after_scheme
                .find('/')
                .map(|idx| after_scheme[idx..].to_string())
                .ok_or_else(|| ComposeError::InvalidUrl {
                    url: url.to_string(),
                    message: "could not extract path from URL".to_string(),
                })
        }
        None => {
            // Not a URL, treat the whole thing as a path
            Ok(url.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detect_direction_response() {
        let payload = json!({
            "ucp": {
                "capabilities": {
                    "dev.ucp.shopping.checkout": [{"version": "2026-01-11", "schema": "..."}]
                }
            }
        });
        assert_eq!(
            detect_direction(&payload),
            Some(DetectedDirection::Response)
        );
    }

    #[test]
    fn detect_direction_request() {
        // JSONRPC request: meta.profile at root (NOT ucp.meta.profile)
        let payload = json!({
            "meta": {
                "profile": "https://example.com/.well-known/ucp"
            },
            "checkout": {
                "line_items": []
            }
        });
        assert_eq!(detect_direction(&payload), Some(DetectedDirection::Request));
    }

    #[test]
    fn detect_direction_old_request_format_not_detected() {
        // Old invalid format should NOT be detected as request
        let payload = json!({
            "ucp": {
                "meta": {
                    "profile": "https://example.com/.well-known/ucp"
                }
            }
        });
        assert_eq!(detect_direction(&payload), None);
    }

    #[test]
    fn detect_direction_neither() {
        let payload = json!({
            "ucp": {
                "version": "2026-01-11"
            }
        });
        assert_eq!(detect_direction(&payload), None);
    }

    #[test]
    fn detect_direction_no_ucp() {
        let payload = json!({
            "id": "123",
            "status": "incomplete"
        });
        assert_eq!(detect_direction(&payload), None);
    }

    #[test]
    fn parse_capabilities_single_root() {
        let caps = json!({
            "dev.ucp.shopping.checkout": [{
                "version": "2026-01-11",
                "schema": "https://ucp.dev/schemas/shopping/checkout.json"
            }]
        });
        let result = parse_capabilities_object(&caps).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "dev.ucp.shopping.checkout");
        assert_eq!(result[0].version, "2026-01-11");
        assert!(result[0].extends.is_none());
    }

    #[test]
    fn parse_capabilities_with_extension() {
        let caps = json!({
            "dev.ucp.shopping.checkout": [{
                "version": "2026-01-11",
                "schema": "https://ucp.dev/schemas/shopping/checkout.json"
            }],
            "dev.ucp.shopping.discount": [{
                "version": "2026-01-11",
                "schema": "https://ucp.dev/schemas/shopping/discount.json",
                "extends": "dev.ucp.shopping.checkout"
            }]
        });
        let result = parse_capabilities_object(&caps).unwrap();
        assert_eq!(result.len(), 2);

        let discount = result
            .iter()
            .find(|c| c.name == "dev.ucp.shopping.discount")
            .unwrap();
        assert_eq!(
            discount.extends,
            Some(vec!["dev.ucp.shopping.checkout".to_string()])
        );
    }

    #[test]
    fn parse_capabilities_multi_parent() {
        // Tests diamond pattern: combo extends both discount and fulfillment
        let caps = json!({
            "dev.ucp.shopping.checkout": [{
                "version": "2026-01-11",
                "schema": "https://ucp.dev/schemas/shopping/checkout.json"
            }],
            "dev.ucp.shopping.discount": [{
                "version": "2026-01-11",
                "schema": "https://ucp.dev/schemas/shopping/discount.json",
                "extends": "dev.ucp.shopping.checkout"
            }],
            "dev.ucp.shopping.fulfillment": [{
                "version": "2026-01-11",
                "schema": "https://ucp.dev/schemas/shopping/fulfillment.json",
                "extends": "dev.ucp.shopping.checkout"
            }],
            "dev.ucp.shopping.combo": [{
                "version": "2026-01-11",
                "schema": "https://ucp.dev/schemas/shopping/combo.json",
                "extends": ["dev.ucp.shopping.discount", "dev.ucp.shopping.fulfillment"]
            }]
        });
        let result = parse_capabilities_object(&caps).unwrap();
        assert_eq!(result.len(), 4);

        let combo = result
            .iter()
            .find(|c| c.name == "dev.ucp.shopping.combo")
            .unwrap();
        assert_eq!(
            combo.extends,
            Some(vec![
                "dev.ucp.shopping.discount".to_string(),
                "dev.ucp.shopping.fulfillment".to_string()
            ])
        );
    }

    #[test]
    fn parse_capabilities_empty() {
        let caps = json!({});
        let result = parse_capabilities_object(&caps);
        assert!(matches!(result, Err(ComposeError::EmptyCapabilities)));
    }

    #[test]
    fn extract_url_path_https() {
        let path = extract_url_path("https://ucp.dev/schemas/shopping/checkout.json").unwrap();
        assert_eq!(path, "/schemas/shopping/checkout.json");
    }

    #[test]
    fn extract_url_path_http() {
        let path = extract_url_path("http://localhost:8080/schemas/test.json").unwrap();
        assert_eq!(path, "/schemas/test.json");
    }

    #[test]
    fn extract_url_path_local() {
        let path = extract_url_path("./schemas/checkout.json").unwrap();
        assert_eq!(path, "./schemas/checkout.json");
    }

    #[test]
    fn compose_no_extensions() {
        // Setup: single root capability
        let checkout = Capability {
            name: "dev.ucp.shopping.checkout".to_string(),
            version: "2026-01-11".to_string(),
            schema_url: "checkout.json".to_string(),
            extends: None,
        };

        // This will fail because checkout.json doesn't exist, but tests the logic path
        let config = SchemaBaseConfig {
            local_base: Some(Path::new("/nonexistent")),
            remote_base: None,
        };
        let result = compose_schema(&[checkout], &config);
        assert!(matches!(result, Err(ComposeError::SchemaFetch { .. })));
    }

    #[test]
    fn compose_no_root_error() {
        let discount = Capability {
            name: "dev.ucp.shopping.discount".to_string(),
            version: "2026-01-11".to_string(),
            schema_url: "discount.json".to_string(),
            extends: Some(vec!["dev.ucp.shopping.checkout".to_string()]),
        };

        let config = SchemaBaseConfig::default();
        let result = compose_schema(&[discount], &config);
        assert!(matches!(result, Err(ComposeError::NoRootCapability)));
    }

    #[test]
    fn compose_multiple_roots_error() {
        // Error case: fulfillment is missing its "extends" field, creating two roots
        let checkout = Capability {
            name: "dev.ucp.shopping.checkout".to_string(),
            version: "2026-01-11".to_string(),
            schema_url: "checkout.json".to_string(),
            extends: None,
        };
        let fulfillment = Capability {
            name: "dev.ucp.shopping.fulfillment".to_string(),
            version: "2026-01-11".to_string(),
            schema_url: "fulfillment.json".to_string(),
            extends: None, // Bug: should extend checkout
        };

        let config = SchemaBaseConfig::default();
        let result = compose_schema(&[checkout, fulfillment], &config);
        assert!(matches!(
            result,
            Err(ComposeError::MultipleRootCapabilities { .. })
        ));
    }

    #[test]
    fn compose_unknown_parent_error() {
        let checkout = Capability {
            name: "dev.ucp.shopping.checkout".to_string(),
            version: "2026-01-11".to_string(),
            schema_url: "checkout.json".to_string(),
            extends: None,
        };
        let discount = Capability {
            name: "dev.ucp.shopping.discount".to_string(),
            version: "2026-01-11".to_string(),
            schema_url: "discount.json".to_string(),
            extends: Some(vec!["dev.ucp.shopping.nonexistent".to_string()]),
        };

        let config = SchemaBaseConfig::default();
        let result = compose_schema(&[checkout, discount], &config);
        assert!(matches!(result, Err(ComposeError::UnknownParent { .. })));
    }

    #[test]
    fn reaches_root_direct() {
        let checkout = Capability {
            name: "dev.ucp.shopping.checkout".to_string(),
            version: "2026-01-11".to_string(),
            schema_url: "checkout.json".to_string(),
            extends: None,
        };
        let discount = Capability {
            name: "dev.ucp.shopping.discount".to_string(),
            version: "2026-01-11".to_string(),
            schema_url: "discount.json".to_string(),
            extends: Some(vec!["dev.ucp.shopping.checkout".to_string()]),
        };

        let cap_map: HashMap<&str, &Capability> = vec![
            ("dev.ucp.shopping.checkout", &checkout),
            ("dev.ucp.shopping.discount", &discount),
        ]
        .into_iter()
        .collect();

        assert!(reaches_root(
            &discount,
            &cap_map,
            "dev.ucp.shopping.checkout"
        ));
    }

    #[test]
    fn reaches_root_transitive_diamond() {
        // Tests diamond extension pattern: combo extends both discount and fulfillment,
        // both of which extend checkout. This is a realistic UCP scenario.
        let checkout = Capability {
            name: "dev.ucp.shopping.checkout".to_string(),
            version: "2026-01-11".to_string(),
            schema_url: "checkout.json".to_string(),
            extends: None,
        };
        let discount = Capability {
            name: "dev.ucp.shopping.discount".to_string(),
            version: "2026-01-11".to_string(),
            schema_url: "discount.json".to_string(),
            extends: Some(vec!["dev.ucp.shopping.checkout".to_string()]),
        };
        let fulfillment = Capability {
            name: "dev.ucp.shopping.fulfillment".to_string(),
            version: "2026-01-11".to_string(),
            schema_url: "fulfillment.json".to_string(),
            extends: Some(vec!["dev.ucp.shopping.checkout".to_string()]),
        };
        // Combo capability that extends both discount and fulfillment
        let combo = Capability {
            name: "dev.ucp.shopping.combo".to_string(),
            version: "2026-01-11".to_string(),
            schema_url: "combo.json".to_string(),
            extends: Some(vec![
                "dev.ucp.shopping.discount".to_string(),
                "dev.ucp.shopping.fulfillment".to_string(),
            ]),
        };

        let cap_map: HashMap<&str, &Capability> = vec![
            ("dev.ucp.shopping.checkout", &checkout),
            ("dev.ucp.shopping.discount", &discount),
            ("dev.ucp.shopping.fulfillment", &fulfillment),
            ("dev.ucp.shopping.combo", &combo),
        ]
        .into_iter()
        .collect();

        // combo -> discount -> checkout (transitive through discount)
        // combo -> fulfillment -> checkout (transitive through fulfillment)
        assert!(reaches_root(&combo, &cap_map, "dev.ucp.shopping.checkout"));
        // Also verify the direct extensions
        assert!(reaches_root(
            &discount,
            &cap_map,
            "dev.ucp.shopping.checkout"
        ));
        assert!(reaches_root(
            &fulfillment,
            &cap_map,
            "dev.ucp.shopping.checkout"
        ));
    }

    #[test]
    fn reaches_root_orphan() {
        // Tests orphan detection: an extension that doesn't connect to root
        let checkout = Capability {
            name: "dev.ucp.shopping.checkout".to_string(),
            version: "2026-01-11".to_string(),
            schema_url: "checkout.json".to_string(),
            extends: None,
        };
        let discount = Capability {
            name: "dev.ucp.shopping.discount".to_string(),
            version: "2026-01-11".to_string(),
            schema_url: "discount.json".to_string(),
            // Extends something that's not in the map and not root
            extends: Some(vec!["dev.ucp.shopping.nonexistent".to_string()]),
        };

        let cap_map: HashMap<&str, &Capability> = vec![
            ("dev.ucp.shopping.checkout", &checkout),
            ("dev.ucp.shopping.discount", &discount),
        ]
        .into_iter()
        .collect();

        // discount extends nonexistent, which doesn't connect to checkout
        assert!(!reaches_root(
            &discount,
            &cap_map,
            "dev.ucp.shopping.checkout"
        ));
    }

    #[test]
    fn capability_short_name_extracts_last_segment() {
        assert_eq!(
            capability_short_name("dev.ucp.shopping.checkout"),
            "checkout"
        );
        assert_eq!(
            capability_short_name("dev.ucp.shopping.discount"),
            "discount"
        );
        assert_eq!(capability_short_name("checkout"), "checkout");
    }

    #[test]
    fn extract_jsonrpc_payload_finds_checkout_key() {
        let envelope = json!({
            "meta": {"profile": "https://example.com/profile"},
            "checkout": {"line_items": [{"item": {"id": "sku"}, "quantity": 2}]}
        });

        let capabilities = vec![Capability {
            name: "dev.ucp.shopping.checkout".to_string(),
            version: "2026-01-26".to_string(),
            schema_url: "https://example.com/checkout.json".to_string(),
            extends: None,
        }];

        let (payload, key) = extract_jsonrpc_payload(&envelope, &capabilities).unwrap();
        assert_eq!(key, "checkout");
        assert_eq!(payload["line_items"][0]["quantity"], 2);
    }

    #[test]
    fn extract_jsonrpc_payload_missing_key_errors() {
        let envelope = json!({
            "meta": {"profile": "https://example.com/profile"},
            "wrong_key": {"line_items": []}
        });

        let capabilities = vec![Capability {
            name: "dev.ucp.shopping.checkout".to_string(),
            version: "2026-01-26".to_string(),
            schema_url: "https://example.com/checkout.json".to_string(),
            extends: None,
        }];

        let result = extract_jsonrpc_payload(&envelope, &capabilities);
        assert!(matches!(result, Err(ComposeError::InvalidEnvelope { .. })));
    }
}
