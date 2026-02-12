//! Schema loading from various sources.
//!
//! Handles loading schemas from files, strings, and HTTP URLs.

use std::path::Path;

use serde_json::Value;

use crate::error::ResolveError;

#[cfg(feature = "remote")]
use std::time::Duration;

/// Default timeout for HTTP requests (10 seconds).
#[cfg(feature = "remote")]
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Load a schema from a file path.
///
/// # Errors
///
/// Returns `ResolveError::FileNotFound` if the file doesn't exist,
/// or `ResolveError::InvalidJson` if the file isn't valid JSON.
pub fn load_schema(path: &Path) -> Result<Value, ResolveError> {
    if !path.exists() {
        return Err(ResolveError::FileNotFound {
            path: path.to_path_buf(),
        });
    }

    let content = std::fs::read_to_string(path).map_err(|source| ResolveError::ReadError {
        path: path.to_path_buf(),
        source,
    })?;

    serde_json::from_str(&content).map_err(|source| ResolveError::InvalidJson { source })
}

/// Load a schema from a JSON string.
///
/// # Errors
///
/// Returns `ResolveError::InvalidJson` if the string isn't valid JSON.
pub fn load_schema_str(content: &str) -> Result<Value, ResolveError> {
    serde_json::from_str(content).map_err(|source| ResolveError::InvalidJson { source })
}

/// Load a schema from an HTTP/HTTPS URL.
///
/// Requires the `remote` feature (enabled by default).
///
/// # Errors
///
/// Returns `ResolveError::NetworkError` if the request fails,
/// or `ResolveError::InvalidJson` if the response isn't valid JSON.
#[cfg(feature = "remote")]
pub fn load_schema_url(url: &str) -> Result<Value, ResolveError> {
    let client = reqwest::blocking::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|source| ResolveError::NetworkError {
            url: url.to_string(),
            source,
        })?;

    let response = client
        .get(url)
        .send()
        .map_err(|source| ResolveError::NetworkError {
            url: url.to_string(),
            source,
        })?;

    // Check for HTTP errors before parsing
    let response = response
        .error_for_status()
        .map_err(|source| ResolveError::NetworkError {
            url: url.to_string(),
            source,
        })?;

    response
        .json()
        .map_err(|source| ResolveError::NetworkError {
            url: url.to_string(),
            source,
        })
}

/// Check if a string looks like a URL (starts with http:// or https://).
pub fn is_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// Navigate a JSON Pointer fragment (e.g., "#/$defs/foo" or "#/properties/bar").
///
/// Returns the value at the given JSON Pointer path within the schema.
/// The fragment should start with '#' (e.g., "#/$defs/foo").
pub fn navigate_fragment(schema: &Value, fragment: &str) -> Result<Value, ResolveError> {
    // Remove leading # and split by /
    let path = fragment.trim_start_matches('#').trim_start_matches('/');
    if path.is_empty() {
        return Ok(schema.clone());
    }

    let mut current = schema;
    for part in path.split('/') {
        // Unescape JSON Pointer encoding (~1 = /, ~0 = ~)
        let key = part.replace("~1", "/").replace("~0", "~");
        current = current.get(&key).ok_or_else(|| ResolveError::BundleError {
            message: format!("fragment not found: {}", fragment),
        })?;
    }
    Ok(current.clone())
}

/// Recursively resolve and inline external $ref pointers.
///
/// Walks the schema tree, finds `$ref` values pointing to external files,
/// loads them, and replaces the $ref with the loaded content.
/// Internal refs (`#/...`) in the root schema are left for the validator.
/// Internal refs in loaded external files are resolved against that file.
/// Self-root refs (`$ref: "#"`) are left as-is (recursive type definitions).
///
/// # Arguments
/// * `schema` - The schema to process (modified in place)
/// * `base_dir` - Base directory for resolving relative file paths
pub fn bundle_refs(schema: &mut Value, base_dir: &Path) -> Result<(), ResolveError> {
    // Snapshot root schema so internal #/$defs/ refs can resolve against it.
    let root_snapshot = schema.clone();
    bundle_refs_inner(
        schema,
        base_dir,
        Some(&root_snapshot),
        None,
        None,
        &mut std::collections::HashSet::new(),
    )
}

/// Bundle external $ref pointers with URL-to-local-path mapping.
///
/// Like `bundle_refs`, but handles absolute URL refs by mapping them to local paths.
/// When a ref starts with `remote_base`, that prefix is stripped and the remainder
/// is joined to `local_base` to form the local file path.
///
/// # Example
/// ```text
/// remote_base = "https://ucp.dev/draft"
/// local_base = Path::new("site")
/// $ref = "https://ucp.dev/draft/schemas/ucp.json" -> "site/schemas/ucp.json"
/// ```
pub fn bundle_refs_with_url_mapping(
    schema: &mut Value,
    base_dir: &Path,
    local_base: &Path,
    remote_base: &str,
) -> Result<(), ResolveError> {
    let root_snapshot = schema.clone();
    bundle_refs_inner(
        schema,
        base_dir,
        Some(&root_snapshot),
        Some(local_base),
        Some(remote_base),
        &mut std::collections::HashSet::new(),
    )
}

fn bundle_refs_inner(
    schema: &mut Value,
    base_dir: &Path,
    file_root: Option<&Value>, // Root of external file for resolving internal refs
    url_local_base: Option<&Path>,
    url_remote_base: Option<&str>,
    visited: &mut std::collections::HashSet<String>,
) -> Result<(), ResolveError> {
    match schema {
        Value::Object(obj) => {
            // Check if this object has a $ref
            if let Some(ref_val) = obj.get("$ref").and_then(|v| v.as_str()) {
                if ref_val.starts_with('#') {
                    // Internal ref - only resolve if we have a file_root context
                    // Skip self-root refs ($ref: "#") - these are recursive type defs
                    if ref_val == "#" {
                        // Leave as-is - can't inline recursive self-reference
                    } else if let Some(root) = file_root {
                        let mut target = navigate_fragment(root, ref_val)?;
                        // Recursively process (may have nested refs)
                        bundle_refs_inner(
                            &mut target,
                            base_dir,
                            file_root,
                            url_local_base,
                            url_remote_base,
                            visited,
                        )?;
                        // Inline the resolved definition
                        obj.remove("$ref");
                        if let Value::Object(ref_obj) = target {
                            for (k, v) in ref_obj {
                                obj.entry(k).or_insert(v);
                            }
                        }
                        return Ok(());
                    }
                    // No file_root context — leave as-is
                } else {
                    // External ref - may be relative path or absolute URL
                    let (file_part, fragment) = match ref_val.find('#') {
                        Some(idx) => (&ref_val[..idx], Some(&ref_val[idx..])),
                        None => (ref_val, None),
                    };

                    // Resolve ref to local path, handling URL mapping if configured
                    let ref_path =
                        resolve_ref_to_path(file_part, base_dir, url_local_base, url_remote_base);

                    let canonical = ref_path.canonicalize().unwrap_or(ref_path.clone());
                    let visit_key = format!("{}|{}", canonical.display(), fragment.unwrap_or(""));

                    if visited.contains(&visit_key) {
                        return Err(ResolveError::BundleError {
                            message: format!("circular reference detected: {}", ref_val),
                        });
                    }

                    // Load file - this becomes the new file_root for internal refs
                    let loaded = load_schema(&ref_path)?;
                    let mut target = if let Some(frag) = fragment {
                        navigate_fragment(&loaded, frag)?
                    } else {
                        loaded.clone()
                    };

                    visited.insert(visit_key.clone());
                    let ref_dir = ref_path.parent().unwrap_or(base_dir);
                    // Pass loaded file as file_root so internal refs resolve against it
                    bundle_refs_inner(
                        &mut target,
                        ref_dir,
                        Some(&loaded),
                        url_local_base,
                        url_remote_base,
                        visited,
                    )?;
                    visited.remove(&visit_key);

                    obj.remove("$ref");
                    if let Value::Object(ref_obj) = target {
                        for (k, v) in ref_obj {
                            obj.entry(k).or_insert(v);
                        }
                    }
                    return Ok(());
                }
            }

            // Recurse into all values
            for value in obj.values_mut() {
                bundle_refs_inner(
                    value,
                    base_dir,
                    file_root,
                    url_local_base,
                    url_remote_base,
                    visited,
                )?;
            }
        }
        Value::Array(arr) => {
            for item in arr {
                bundle_refs_inner(
                    item,
                    base_dir,
                    file_root,
                    url_local_base,
                    url_remote_base,
                    visited,
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Resolve a $ref value to a local file path.
///
/// If URL mapping is configured and the ref matches the remote base,
/// strips the prefix and joins to local_base. Otherwise uses base_dir
/// for relative path resolution.
fn resolve_ref_to_path(
    ref_val: &str,
    base_dir: &Path,
    url_local_base: Option<&Path>,
    url_remote_base: Option<&str>,
) -> std::path::PathBuf {
    // Check if this is an absolute URL that matches our remote base
    if let (Some(local_base), Some(remote_base)) = (url_local_base, url_remote_base) {
        if let Some(remainder) = ref_val.strip_prefix(remote_base) {
            // URL matches remote base - map to local path
            return local_base.join(remainder.trim_start_matches('/'));
        }
    }

    // Default: treat as relative path from base_dir
    base_dir.join(ref_val)
}

/// Bundle external $ref pointers by fetching from remote URLs.
///
/// Like `bundle_refs`, but fetches external refs via HTTP instead of local files.
/// This allows remote-only validation by inlining all refs before passing to
/// the JSON Schema validator.
///
/// # Arguments
/// * `schema` - The schema to process (modified in place)
/// * `base_url` - Base URL for resolving relative refs (typically the schema's $id)
#[cfg(feature = "remote")]
pub fn bundle_refs_remote(schema: &mut Value, base_url: &str) -> Result<(), ResolveError> {
    // Snapshot root schema so internal #/$defs/ refs can resolve against it.
    let root_snapshot = schema.clone();
    bundle_refs_remote_inner(
        schema,
        base_url,
        Some(&root_snapshot),
        &mut std::collections::HashSet::new(),
    )
}

#[cfg(feature = "remote")]
fn bundle_refs_remote_inner(
    schema: &mut Value,
    base_url: &str,
    file_root: Option<&Value>,
    visited: &mut std::collections::HashSet<String>,
) -> Result<(), ResolveError> {
    match schema {
        Value::Object(obj) => {
            if let Some(ref_val) = obj.get("$ref").and_then(|v| v.as_str()) {
                if ref_val.starts_with('#') {
                    // Internal ref
                    if ref_val == "#" {
                        // Self-reference, leave as-is
                    } else if let Some(root) = file_root {
                        let mut target = navigate_fragment(root, ref_val)?;
                        bundle_refs_remote_inner(&mut target, base_url, file_root, visited)?;
                        obj.remove("$ref");
                        if let Value::Object(ref_obj) = target {
                            for (k, v) in ref_obj {
                                obj.entry(k).or_insert(v);
                            }
                        }
                        return Ok(());
                    }
                    // No file_root context — leave as-is
                } else {
                    // External ref - resolve URL
                    let (file_part, fragment) = match ref_val.find('#') {
                        Some(idx) => (&ref_val[..idx], Some(&ref_val[idx..])),
                        None => (ref_val, None),
                    };

                    // Resolve to absolute URL
                    let resolved_url = resolve_url(file_part, base_url);
                    let visit_key = format!("{}|{}", resolved_url, fragment.unwrap_or(""));

                    if visited.contains(&visit_key) {
                        return Err(ResolveError::BundleError {
                            message: format!("circular reference detected: {}", ref_val),
                        });
                    }

                    // Fetch the referenced schema
                    let loaded = load_schema_url(&resolved_url)?;
                    let mut target = if let Some(frag) = fragment {
                        navigate_fragment(&loaded, frag)?
                    } else {
                        loaded.clone()
                    };

                    visited.insert(visit_key.clone());
                    // Recursively bundle with new base URL
                    bundle_refs_remote_inner(&mut target, &resolved_url, Some(&loaded), visited)?;
                    visited.remove(&visit_key);

                    obj.remove("$ref");
                    if let Value::Object(ref_obj) = target {
                        for (k, v) in ref_obj {
                            obj.entry(k).or_insert(v);
                        }
                    }
                    return Ok(());
                }
            }

            // Recurse into all values
            for value in obj.values_mut() {
                bundle_refs_remote_inner(value, base_url, file_root, visited)?;
            }
        }
        Value::Array(arr) => {
            for item in arr {
                bundle_refs_remote_inner(item, base_url, file_root, visited)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Resolve a potentially relative URL against a base URL.
#[cfg(feature = "remote")]
fn resolve_url(url: &str, base: &str) -> String {
    if is_url(url) {
        // Already absolute
        url.to_string()
    } else {
        // Relative - resolve against base
        // Find the directory part of base URL
        if let Some(idx) = base.rfind('/') {
            format!("{}/{}", &base[..idx], url)
        } else {
            url.to_string()
        }
    }
}

/// Load a schema from a file path or URL.
///
/// Automatically detects whether the source is a URL or file path.
/// URL loading requires the `remote` feature.
///
/// # Errors
///
/// Returns appropriate errors based on the source type.
pub fn load_schema_auto(source: &str) -> Result<Value, ResolveError> {
    if is_url(source) {
        #[cfg(feature = "remote")]
        {
            load_schema_url(source)
        }
        #[cfg(not(feature = "remote"))]
        {
            Err(ResolveError::FileNotFound {
                path: std::path::PathBuf::from(source),
            })
        }
    } else {
        load_schema(Path::new(source))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn load_schema_valid_file() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"type": "object"}}"#).unwrap();

        let schema = load_schema(file.path()).unwrap();
        assert_eq!(schema["type"], "object");
    }

    #[test]
    fn load_schema_file_not_found() {
        let result = load_schema(Path::new("/nonexistent/path.json"));
        assert!(matches!(result, Err(ResolveError::FileNotFound { .. })));
    }

    #[test]
    fn load_schema_invalid_json() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "not valid json").unwrap();

        let result = load_schema(file.path());
        assert!(matches!(result, Err(ResolveError::InvalidJson { .. })));
    }

    #[test]
    fn load_schema_str_valid() {
        let schema = load_schema_str(r#"{"type": "object"}"#).unwrap();
        assert_eq!(schema["type"], "object");
    }

    #[test]
    fn load_schema_str_invalid() {
        let result = load_schema_str("not json");
        assert!(matches!(result, Err(ResolveError::InvalidJson { .. })));
    }

    #[test]
    fn is_url_https() {
        assert!(is_url("https://example.com/schema.json"));
    }

    #[test]
    fn is_url_http() {
        assert!(is_url("http://example.com/schema.json"));
    }

    #[test]
    fn is_url_file_path() {
        assert!(!is_url("/path/to/schema.json"));
        assert!(!is_url("./schema.json"));
        assert!(!is_url("schema.json"));
    }

    #[test]
    fn load_schema_auto_file() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"type": "string"}}"#).unwrap();

        let schema = load_schema_auto(file.path().to_str().unwrap()).unwrap();
        assert_eq!(schema["type"], "string");
    }

    #[test]
    fn resolve_ref_to_path_with_url_mapping() {
        let base_dir = Path::new("/some/dir");
        let local_base = Path::new("/local/schemas");
        let remote_base = "https://ucp.dev/draft";

        // URL matching remote base gets mapped to local
        let path = resolve_ref_to_path(
            "https://ucp.dev/draft/schemas/ucp.json",
            base_dir,
            Some(local_base),
            Some(remote_base),
        );
        assert_eq!(path, Path::new("/local/schemas/schemas/ucp.json"));
    }

    #[test]
    fn resolve_ref_to_path_url_not_matching_remote() {
        let base_dir = Path::new("/some/dir");
        let local_base = Path::new("/local/schemas");
        let remote_base = "https://ucp.dev/draft";

        // URL not matching remote base falls back to base_dir join
        let path = resolve_ref_to_path(
            "https://other.com/schemas/foo.json",
            base_dir,
            Some(local_base),
            Some(remote_base),
        );
        assert_eq!(
            path,
            Path::new("/some/dir/https://other.com/schemas/foo.json")
        );
    }

    #[test]
    fn resolve_ref_to_path_relative_ref() {
        let base_dir = Path::new("/some/dir");

        // Relative ref without URL mapping
        let path = resolve_ref_to_path("types/buyer.json", base_dir, None, None);
        assert_eq!(path, Path::new("/some/dir/types/buyer.json"));
    }

    #[test]
    fn resolve_ref_to_path_strips_leading_slash() {
        let base_dir = Path::new("/some/dir");
        let local_base = Path::new("/local");
        let remote_base = "https://ucp.dev/draft";

        // Stripping remote base leaves "/schemas/..." - leading slash should be trimmed
        let path = resolve_ref_to_path(
            "https://ucp.dev/draft/schemas/foo.json",
            base_dir,
            Some(local_base),
            Some(remote_base),
        );
        assert_eq!(path, Path::new("/local/schemas/foo.json"));
    }

    // Remote tests - require network, use httpbin.org for reliable testing
    #[cfg(feature = "remote")]
    mod remote {
        use super::*;

        #[test]
        fn load_schema_url_valid() {
            // httpbin.org/json returns a well-known JSON response
            let result = load_schema_url("https://httpbin.org/json");
            assert!(result.is_ok());
            let schema = result.unwrap();
            // httpbin returns {"slideshow": {...}}
            assert!(schema.get("slideshow").is_some());
        }

        #[test]
        fn load_schema_url_404() {
            let result = load_schema_url("https://httpbin.org/status/404");
            assert!(matches!(result, Err(ResolveError::NetworkError { .. })));
        }

        #[test]
        fn load_schema_url_invalid_host() {
            let result =
                load_schema_url("https://this-domain-does-not-exist-12345.invalid/schema.json");
            assert!(matches!(result, Err(ResolveError::NetworkError { .. })));
        }

        #[test]
        fn load_schema_auto_url() {
            let result = load_schema_auto("https://httpbin.org/json");
            assert!(result.is_ok());
        }
    }
}
