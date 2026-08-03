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

/// True if any `$ref: "#"` occurs anywhere in the value.
pub(crate) fn contains_self_root_ref(value: &Value) -> bool {
    match value {
        Value::Object(obj) => {
            if obj.get("$ref").and_then(|v| v.as_str()) == Some("#") {
                return true;
            }
            obj.values().any(contains_self_root_ref)
        }
        Value::Array(arr) => arr.iter().any(contains_self_root_ref),
        _ => false,
    }
}

/// Copy of a file's root schema suitable for inlining at a `$ref: "#"` site:
/// `$defs` is dropped (internal refs are resolved against the original file),
/// and `$id`/`$schema` are dropped so the copy doesn't open a new resource
/// scope at the inline site.
pub(crate) fn root_schema_copy(file_root: &Value) -> Value {
    let mut copy = file_root.clone();
    if let Value::Object(obj) = &mut copy {
        obj.remove("$defs");
        obj.remove("$id");
        obj.remove("$schema");
    }
    copy
}

/// Stable (FNV-1a) hash so synthesized resource ids are deterministic across
/// builds, unlike `DefaultHasher`.
pub(crate) fn stable_hash(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in s.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Identity of a source file for cycle bookkeeping: its `$id` when present,
/// otherwise a content hash.
fn self_root_identity(file_root: &Value) -> String {
    match file_root.get("$id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => format!("{:016x}", stable_hash(&file_root.to_string())),
    }
}

/// Deterministic resource id for a file that has no `$id` of its own,
/// derived from the file's content so distinct files never collide.
pub(crate) fn synthesized_root_id(file_root: &Value) -> String {
    format!(
        "urn:ucp-schema:bundled-root:{:016x}",
        stable_hash(&file_root.to_string())
    )
}

/// Attach a schema resolved from a `$ref: "#"` site to that site's object
/// (whose `$ref` has already been removed). With no sibling keywords the
/// schema is spliced in directly; with siblings, 2020-12 requires the ref'd
/// schema to apply in CONJUNCTION with them, so it is pushed into `allOf`.
/// Returns false when the site has a malformed (non-array) `allOf`, in which
/// case the caller should use the embedded-resource form instead.
pub(crate) fn attach_resolved_self_root(
    obj: &mut serde_json::Map<String, Value>,
    resolved: Value,
) -> bool {
    if obj.is_empty() {
        if let Value::Object(root_obj) = resolved {
            for (k, v) in root_obj {
                obj.insert(k, v);
            }
        }
        return true;
    }
    match obj.get_mut("allOf") {
        None => {
            obj.insert("allOf".to_string(), Value::Array(vec![resolved]));
            true
        }
        Some(Value::Array(arr)) => {
            arr.push(resolved);
            true
        }
        Some(_) => false,
    }
}

/// Resolve a `$ref: "#"` site inside content extracted from source file
/// `src_root` so it keeps meaning that file's root after bundling (issue #43).
///
/// Sibling keywords at the site are bundled first — they are ordinary
/// subschemas written in the same source file. Preferred strategy for the ref
/// itself: inline a copy of the source file's root schema at the site (in
/// conjunction with any siblings). When that cannot terminate (the root itself
/// contains `$ref: "#"`, or resolving it cycles back into this site), fall
/// back to embedding the whole source file as a `$defs` resource with its
/// `$id` intact (synthesized when missing) and rewriting the ref to that id —
/// under 2020-12 embedded-resource rules, `#` inside the embedded copy then
/// resolves against its own `$id`, and the rewritten `$ref` applies in
/// conjunction with the remaining siblings.
fn inline_self_root_ref(
    obj: &mut serde_json::Map<String, Value>,
    src_root: &Value,
    base_dir: &Path,
    url_local_base: Option<&Path>,
    url_remote_base: Option<&str>,
    visited: &mut std::collections::HashSet<String>,
) -> Result<(), ResolveError> {
    obj.remove("$ref");
    for value in obj.values_mut() {
        bundle_refs_inner(
            value,
            base_dir,
            Some(src_root),
            Some(src_root),
            url_local_base,
            url_remote_base,
            visited,
        )?;
    }

    let visit_key = format!("self-root|{}", self_root_identity(src_root));
    let root_copy = root_schema_copy(src_root);
    if !visited.contains(&visit_key) && !contains_self_root_ref(&root_copy) {
        // Attempt the inline on cloned state so a cycle detected mid-way can
        // fall back cleanly without corrupting the caller's bookkeeping.
        let mut attempt = root_copy;
        let mut attempt_visited = visited.clone();
        attempt_visited.insert(visit_key.clone());
        if bundle_refs_inner(
            &mut attempt,
            base_dir,
            Some(src_root),
            Some(src_root),
            url_local_base,
            url_remote_base,
            &mut attempt_visited,
        )
        .is_ok()
        {
            attempt_visited.remove(&visit_key);
            let committed = attach_resolved_self_root(obj, attempt);
            if committed {
                *visited = attempt_visited;
                return Ok(());
            }
        }
    }
    embed_self_root_resource(
        obj,
        src_root,
        base_dir,
        url_local_base,
        url_remote_base,
        visited,
    )
}

/// Fallback for [`inline_self_root_ref`]: embed the source file as a `$defs`
/// resource (with `$id`) and point the ref at it. The embedded copy is only
/// materialized once per source file; later sites just reuse the id.
fn embed_self_root_resource(
    obj: &mut serde_json::Map<String, Value>,
    src_root: &Value,
    base_dir: &Path,
    url_local_base: Option<&Path>,
    url_remote_base: Option<&str>,
    visited: &mut std::collections::HashSet<String>,
) -> Result<(), ResolveError> {
    let id = match src_root.get("$id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => synthesized_root_id(src_root),
    };
    let embed_marker = format!("embedded|{}", id);
    if !visited.contains(&embed_marker) {
        visited.insert(embed_marker);
        let mut resource = src_root.clone();
        if let Value::Object(res_obj) = &mut resource {
            res_obj
                .entry("$id")
                .or_insert_with(|| Value::String(id.clone()));
        }
        // Bundle only the embedded copy's external refs. Its internal refs
        // ("#" and "#/...") now sit under the resource's own $id and already
        // mean the right thing, so no file_root/self context is passed.
        bundle_refs_inner(
            &mut resource,
            base_dir,
            None,
            None,
            url_local_base,
            url_remote_base,
            visited,
        )?;
        let defs = obj
            .entry("$defs")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if let Value::Object(defs_obj) = defs {
            let mut key = "bundled_self_root".to_string();
            let mut n = 1;
            while defs_obj.contains_key(&key) {
                n += 1;
                key = format!("bundled_self_root_{}", n);
            }
            defs_obj.insert(key, resource);
        }
    }
    obj.insert("$ref".to_string(), Value::String(id));
    Ok(())
}

/// Recursively resolve and inline external $ref pointers.
///
/// Walks the schema tree, finds `$ref` values pointing to external files,
/// loads them, and replaces the $ref with the loaded content.
/// Internal refs (`#/...`) in the root schema are left for the validator.
/// Internal refs in loaded external files are resolved against that file.
/// A `$ref: "#"` at document level is left as-is (it keeps meaning the
/// document root); inside inlined fragments it is resolved against the file
/// it was written in (see [`inline_self_root_ref`]).
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
        None,
        Some(local_base),
        Some(remote_base),
        &mut std::collections::HashSet::new(),
    )
}

fn bundle_refs_inner(
    schema: &mut Value,
    base_dir: &Path,
    file_root: Option<&Value>, // Root of external file for resolving internal refs
    self_root: Option<&Value>, // Set when processing a fragment extracted from an
    // external file: the root that `$ref: "#"` refers to
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
                    if ref_val == "#" {
                        if let Some(src) = self_root {
                            // Inside an inlined fragment "#" means the SOURCE
                            // file's root, which is about to disappear from
                            // scope — resolve it now (issue #43).
                            inline_self_root_ref(
                                obj,
                                src,
                                base_dir,
                                url_local_base,
                                url_remote_base,
                                visited,
                            )?;
                            return Ok(());
                        }
                        // Document-level "#" keeps meaning the document root
                        // after bundling — leave as-is.
                    } else if let Some(root) = file_root {
                        let mut target = navigate_fragment(root, ref_val)?;
                        // Recursively process (may have nested refs)
                        bundle_refs_inner(
                            &mut target,
                            base_dir,
                            file_root,
                            self_root,
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

                    // If local resolution fails and the ref is a URL, try HTTP fetch
                    #[cfg(feature = "remote")]
                    let (loaded, ref_dir_owned) = if !ref_path.exists() && is_url(file_part) {
                        let fetched = load_schema_url(file_part)?;
                        // Remote schemas have no local directory; use base_dir for
                        // any relative refs within the fetched schema
                        (fetched, base_dir.to_path_buf())
                    } else {
                        let schema = load_schema(&ref_path)?;
                        let dir = ref_path.parent().unwrap_or(base_dir).to_path_buf();
                        (schema, dir)
                    };

                    #[cfg(not(feature = "remote"))]
                    let (loaded, ref_dir_owned) = {
                        let schema = load_schema(&ref_path)?;
                        let dir = ref_path.parent().unwrap_or(base_dir).to_path_buf();
                        (schema, dir)
                    };

                    let canonical = ref_path.canonicalize().unwrap_or(ref_path.clone());
                    let visit_key = format!("{}|{}", canonical.display(), fragment.unwrap_or(""));

                    if visited.contains(&visit_key) {
                        return Err(ResolveError::BundleError {
                            message: format!("circular reference detected: {}", ref_val),
                        });
                    }

                    let mut target = if let Some(frag) = fragment {
                        navigate_fragment(&loaded, frag)?
                    } else {
                        loaded.clone()
                    };

                    // Self-root ("#") context for the inlined content:
                    // - A fragment loses its file's $id, so "#" inside it must
                    //   be resolved against the source file (issue #43).
                    // - A whole-file inline keeps the file's $id (the merge
                    //   below carries it over), which is exactly what "#"
                    //   binds to — but if the file has no $id, synthesize one
                    //   so "#" doesn't escape to the inlining document root.
                    let sub_self_root = if fragment.is_some() {
                        Some(&loaded)
                    } else {
                        if loaded.get("$id").is_none() && contains_self_root_ref(&target) {
                            if let Value::Object(target_obj) = &mut target {
                                // Content-derived id: two different $id-less
                                // files inlined under the same relative ref
                                // text must not collide.
                                target_obj.insert(
                                    "$id".to_string(),
                                    Value::String(synthesized_root_id(&loaded)),
                                );
                            }
                        }
                        None
                    };

                    visited.insert(visit_key.clone());
                    // Pass loaded file as file_root so internal refs resolve against it
                    bundle_refs_inner(
                        &mut target,
                        &ref_dir_owned,
                        Some(&loaded),
                        sub_self_root,
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
                    self_root,
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
                    self_root,
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
        None,
        &mut std::collections::HashSet::new(),
    )
}

/// Remote twin of [`inline_self_root_ref`]: same strategy, HTTP loading.
#[cfg(feature = "remote")]
fn inline_self_root_ref_remote(
    obj: &mut serde_json::Map<String, Value>,
    src_root: &Value,
    base_url: &str,
    visited: &mut std::collections::HashSet<String>,
) -> Result<(), ResolveError> {
    obj.remove("$ref");
    for value in obj.values_mut() {
        bundle_refs_remote_inner(value, base_url, Some(src_root), Some(src_root), visited)?;
    }

    let visit_key = format!("self-root|{}", self_root_identity(src_root));
    let root_copy = root_schema_copy(src_root);
    if !visited.contains(&visit_key) && !contains_self_root_ref(&root_copy) {
        let mut attempt = root_copy;
        let mut attempt_visited = visited.clone();
        attempt_visited.insert(visit_key.clone());
        if bundle_refs_remote_inner(
            &mut attempt,
            base_url,
            Some(src_root),
            Some(src_root),
            &mut attempt_visited,
        )
        .is_ok()
        {
            attempt_visited.remove(&visit_key);
            let committed = attach_resolved_self_root(obj, attempt);
            if committed {
                *visited = attempt_visited;
                return Ok(());
            }
        }
    }
    embed_self_root_resource_remote(obj, src_root, base_url, visited)
}

/// Remote twin of [`embed_self_root_resource`].
#[cfg(feature = "remote")]
fn embed_self_root_resource_remote(
    obj: &mut serde_json::Map<String, Value>,
    src_root: &Value,
    base_url: &str,
    visited: &mut std::collections::HashSet<String>,
) -> Result<(), ResolveError> {
    let id = match src_root.get("$id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => synthesized_root_id(src_root),
    };
    let embed_marker = format!("embedded|{}", id);
    if !visited.contains(&embed_marker) {
        visited.insert(embed_marker);
        let mut resource = src_root.clone();
        if let Value::Object(res_obj) = &mut resource {
            res_obj
                .entry("$id")
                .or_insert_with(|| Value::String(id.clone()));
        }
        bundle_refs_remote_inner(&mut resource, base_url, None, None, visited)?;
        let defs = obj
            .entry("$defs")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if let Value::Object(defs_obj) = defs {
            let mut key = "bundled_self_root".to_string();
            let mut n = 1;
            while defs_obj.contains_key(&key) {
                n += 1;
                key = format!("bundled_self_root_{}", n);
            }
            defs_obj.insert(key, resource);
        }
    }
    obj.insert("$ref".to_string(), Value::String(id));
    Ok(())
}

#[cfg(feature = "remote")]
fn bundle_refs_remote_inner(
    schema: &mut Value,
    base_url: &str,
    file_root: Option<&Value>,
    self_root: Option<&Value>,
    visited: &mut std::collections::HashSet<String>,
) -> Result<(), ResolveError> {
    match schema {
        Value::Object(obj) => {
            if let Some(ref_val) = obj.get("$ref").and_then(|v| v.as_str()) {
                if ref_val.starts_with('#') {
                    // Internal ref
                    if ref_val == "#" {
                        if let Some(src) = self_root {
                            // "#" inside an inlined fragment means the source
                            // file's root — resolve it now (issue #43).
                            inline_self_root_ref_remote(obj, src, base_url, visited)?;
                            return Ok(());
                        }
                        // Document-level self-reference, leave as-is
                    } else if let Some(root) = file_root {
                        let mut target = navigate_fragment(root, ref_val)?;
                        bundle_refs_remote_inner(
                            &mut target,
                            base_url,
                            file_root,
                            self_root,
                            visited,
                        )?;
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

                    // Same self-root handling as the local bundler: fragments
                    // need "#" resolved against their source file; whole-file
                    // inlines keep (or gain) an $id that "#" binds to.
                    let sub_self_root = if fragment.is_some() {
                        Some(&loaded)
                    } else {
                        if loaded.get("$id").is_none() && contains_self_root_ref(&target) {
                            if let Value::Object(target_obj) = &mut target {
                                target_obj
                                    .insert("$id".to_string(), Value::String(resolved_url.clone()));
                            }
                        }
                        None
                    };

                    visited.insert(visit_key.clone());
                    // Recursively bundle with new base URL
                    bundle_refs_remote_inner(
                        &mut target,
                        &resolved_url,
                        Some(&loaded),
                        sub_self_root,
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
                bundle_refs_remote_inner(value, base_url, file_root, self_root, visited)?;
            }
        }
        Value::Array(arr) => {
            for item in arr {
                bundle_refs_remote_inner(item, base_url, file_root, self_root, visited)?;
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

    // Remote tests run against a local mockito server so they're deterministic
    // and offline — no dependency on a live third party. The connection-error
    // case uses a reserved `.invalid` host (RFC 2606), which fails to resolve
    // locally without touching the network.
    #[cfg(feature = "remote")]
    mod remote {
        use super::*;

        #[test]
        fn load_schema_url_valid() {
            // 200 + JSON body resolves to the parsed value.
            let mut server = mockito::Server::new();
            let mock = server
                .mock("GET", "/schema.json")
                .with_header("content-type", "application/json")
                .with_body(r#"{"type": "object"}"#)
                .create();

            let result = load_schema_url(&format!("{}/schema.json", server.url()));
            assert_eq!(result.unwrap()["type"], "object");
            mock.assert();
        }

        #[test]
        fn load_schema_url_404() {
            // Non-2xx status surfaces as NetworkError (via error_for_status).
            let mut server = mockito::Server::new();
            server
                .mock("GET", "/missing.json")
                .with_status(404)
                .create();

            let result = load_schema_url(&format!("{}/missing.json", server.url()));
            assert!(matches!(result, Err(ResolveError::NetworkError { .. })));
        }

        #[test]
        fn load_schema_url_invalid_host() {
            // Connection/DNS failure surfaces as NetworkError. `.invalid` (RFC
            // 2606) fails to resolve without network access.
            let result =
                load_schema_url("https://this-domain-does-not-exist-12345.invalid/schema.json");
            assert!(matches!(result, Err(ResolveError::NetworkError { .. })));
        }

        #[test]
        fn load_schema_auto_url() {
            // A URL source delegates to load_schema_url.
            let mut server = mockito::Server::new();
            let mock = server
                .mock("GET", "/schema.json")
                .with_header("content-type", "application/json")
                .with_body(r#"{"type": "string"}"#)
                .create();

            let result = load_schema_auto(&format!("{}/schema.json", server.url()));
            assert_eq!(result.unwrap()["type"], "string");
            mock.assert();
        }
    }
}
