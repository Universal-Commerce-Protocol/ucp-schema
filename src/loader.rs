//! Schema loading from various sources.
//!
//! Handles loading schemas from files, strings, and HTTP URLs.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use url::Url;

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

// ---------------------------------------------------------------------------
// Bundling (flatten) — built on the upstream `jsonschema` resolution engine.
//
// UCP resolution (annotations, monotonicity, strict-closing) must see through
// external `$ref`s, so bundling materializes them. Reference *semantics* —
// base URIs, lexical scope, fragments, cycle identity — are delegated to
// `jsonschema::dereference`, which is tested against the official JSON Schema
// suite. Around it run four local passes covering what upstream cannot do:
//
//   1. mask_instance_refs — hide `$ref`-shaped payload inside instance data
//      (`const`/`enum`/`default`/`examples`) from upstream, which walks those
//      positions as if they were schemas; restored byte-identically at the
//      end. Removable if upstream stops traversing instance data;
//   2. hoist_ref_siblings — Draft 2020-12 evaluates `$ref` siblings
//      conjunctively, upstream dereference drops them; hoisting makes the
//      conjunction structural. Removable if upstream preserves siblings;
//   3. crawl_external_refs — upstream resolves lazily-discovered refs only
//      at registry-build time; pre-crawl the transitive document closure so
//      every resource is present up front. Removable if upstream retrieves
//      transitively;
//   4. collapse_and_strip — re-merge the hoisted one-branch conjunctions
//      unless a constraint genuinely conflicts, and shed `$id`/`$schema`
//      from materialized copies no retained ref needs (Draft 2020-12 §8.1.1
//      forbids `$schema` outside a resource root).
//
// Cycles need no local handling: dereference retains them as `$ref`s inside
// an embedded resource copy that keeps its `$id`, so `#` keeps denoting the
// resource it was written in.
// ---------------------------------------------------------------------------

/// Sentinel used to hide `$ref` keys inside instance data from upstream
/// dereference. Reserved everywhere in input documents: the final unmask is
/// a blind whole-tree rename, so any pre-existing key with this name would
/// silently become `$ref` in the output.
const INSTANCE_REF_SENTINEL: &str = "__ucp_instance_ref__";

/// Keywords whose values are instance data, not subschemas.
pub(crate) const INSTANCE_DATA_KEYWORDS: &[&str] = &["const", "enum", "default", "examples"];

/// Turns resolved reference URIs back into fetchable locations.
///
/// Schemas declare `$id` under an HTTP authority (`https://ucp.dev/...`) while
/// living as sibling files on disk, and references resolve against the nearest
/// `$id` per Draft 2020-12. To honor the long-standing contract that relative
/// refs load from the referencing *file's* directory, the retriever learns an
/// `$id`-directory → disk-directory mapping for every document it loads and
/// consults it (longest prefix first) before attempting network access.
struct UcpRetriever {
    /// Learned `$id`-directory → disk-directory prefixes, seeded with the
    /// root document. Mutex because `Retrieve::retrieve` takes `&self`.
    prefixes: std::sync::Mutex<Vec<(String, PathBuf)>>,
    /// Explicit URL→path mapping (`--schema-local-base`/`--schema-remote-base`).
    mapping: Option<(PathBuf, String)>,
}

impl UcpRetriever {
    /// Load from disk and learn the document's own `$id` directory so that
    /// refs *it* declares (resolved against its `$id`) map back to its
    /// directory.
    fn load_and_learn(&self, path: &Path) -> Result<Value, ResolveError> {
        let mut doc = load_schema(path)?;
        prepare_document(&mut doc)?;
        if let (Some(id), Some(dir)) = (doc.get("$id").and_then(Value::as_str), path.parent()) {
            if let Some((id_dir, _)) = id.rsplit_once('/') {
                let mut prefixes = self.prefixes.lock().expect("retriever mutex poisoned");
                let entry = (format!("{id_dir}/"), dir.to_path_buf());
                if !prefixes.contains(&entry) {
                    prefixes.push(entry);
                }
            }
        }
        Ok(doc)
    }
}

impl jsonschema::Retrieve for UcpRetriever {
    fn retrieve(
        &self,
        uri: &jsonschema::Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let uri = uri.as_str();
        // 1. Explicit URL mapping wins.
        if let Some((local, remote)) = &self.mapping {
            if let Some(rest) = uri.strip_prefix(remote.trim_end_matches('/')) {
                let path = local.join(rest.trim_start_matches('/'));
                return Ok(self.load_and_learn(&path)?);
            }
        }
        // 2. file:// URIs load directly. Parse rather than strip the scheme:
        //    `to_file_path` is the inverse of the `Url::from_directory_path`
        //    that built the base URI, so it undoes percent-encoding (`%20`
        //    back to a space) and, on Windows, the slash preceding a drive
        //    letter — `file:///C:/x` is `C:\x`, not `/C:/x`. Stripping the
        //    scheme as text inverts neither.
        if uri.starts_with("file://") {
            let path = Url::parse(uri)
                .ok()
                .and_then(|url| url.to_file_path().ok())
                .ok_or_else(|| format!("not a usable local file URI: {uri}"))?;
            return Ok(self.load_and_learn(&path)?);
        }
        // 3. Learned `$id`-directory anchors: relocate the URI relative to a
        //    known (`$id` dir → disk dir) pair, walking up as needed so that
        //    `../capability.json`-style refs resolve like sibling files.
        let candidate = {
            let prefixes = self.prefixes.lock().expect("retriever mutex poisoned");
            let mut hits: Vec<_> = prefixes
                .iter()
                .filter_map(|(id_dir, disk)| {
                    relocate(id_dir, disk, uri).map(|path| (id_dir.len(), path))
                })
                .collect();
            // Prefer the most specific anchor (longest shared $id directory).
            hits.sort_by_key(|(len, _)| std::cmp::Reverse(*len));
            hits.into_iter()
                .map(|(_, path)| path)
                .find(|path| path.exists())
        };
        if let Some(path) = candidate {
            return Ok(self.load_and_learn(&path)?);
        }
        // 4. Remote fetch, when built with the `remote` feature.
        #[cfg(feature = "remote")]
        if is_url(uri) {
            let mut doc = load_schema_url(uri)?;
            prepare_document(&mut doc)?;
            return Ok(doc);
        }
        Err(format!("cannot retrieve schema resource: {uri}").into())
    }
}

/// Map `uri` onto disk using a known (`$id` directory → disk directory) pair.
///
/// Splits both URI paths on `/`, finds their common ancestor, then applies the
/// divergence to the disk anchor: one `..` per remaining `id_dir` segment plus
/// the URI remainder. Returns `None` when the URIs share no origin.
fn relocate(id_dir: &str, disk: &Path, uri: &str) -> Option<PathBuf> {
    let (id_origin, id_path) = split_origin(id_dir)?;
    let (uri_origin, uri_path) = split_origin(uri)?;
    if id_origin != uri_origin {
        return None;
    }
    let id_segs: Vec<&str> = id_path.split('/').filter(|s| !s.is_empty()).collect();
    let uri_segs: Vec<&str> = uri_path.split('/').filter(|s| !s.is_empty()).collect();
    let common = id_segs
        .iter()
        .zip(uri_segs.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut path = disk.to_path_buf();
    for _ in common..id_segs.len() {
        path = path.join("..");
    }
    for seg in &uri_segs[common..] {
        path = path.join(seg);
    }
    Some(path)
}

/// Split `scheme://authority` from the path portion of a URI.
fn split_origin(uri: &str) -> Option<(&str, &str)> {
    let scheme_end = uri.find("://")? + 3;
    let path_start = uri[scheme_end..]
        .find('/')
        .map_or(uri.len(), |i| scheme_end + i);
    Some((&uri[..path_start], &uri[path_start..]))
}

/// Depth-first crawl of every externally referenced schema document,
/// resolving each `$ref` against its containing document's base URI
/// (`$id` when declared, retrieval URI otherwise), fetching through the
/// UCP retriever. Returns each document keyed by the URI it resolves under.
fn crawl_external_refs(
    root: &Value,
    root_base: &str,
    retriever: &UcpRetriever,
) -> Result<Vec<(String, Value)>, ResolveError> {
    use jsonschema::Retrieve;

    fn refs(value: &Value, out: &mut Vec<String>) {
        // Guided walk: `$ref`-shaped instance data inside `const`/`enum`/
        // unknown keywords is payload, not a reference — chasing it would
        // fetch phantom documents or fail bundling on legal schemas.
        for_each_schema_object(value, &mut |obj| {
            if let Some(Value::String(r)) = obj.get("$ref") {
                if !r.starts_with('#') {
                    out.push(r.clone());
                }
            }
        });
    }

    let base = |doc: &Value, fallback: &str| -> String {
        doc.get("$id")
            .and_then(Value::as_str)
            .map_or_else(|| fallback.to_string(), ToString::to_string)
    };

    let mut resources = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut claimed: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut queue = vec![(root.clone(), base(root, root_base))];
    while let Some((doc, doc_base)) = queue.pop() {
        let mut found = Vec::new();
        refs(&doc, &mut found);
        for reference in found {
            let target = reference.split('#').next().unwrap_or("");
            if target.is_empty() {
                continue;
            }
            let mut joined = Url::parse(&doc_base)
                .and_then(|b| b.join(target))
                .map_err(|e| ResolveError::BundleError {
                    message: format!("invalid $ref {reference:?} against {doc_base}: {e}"),
                })?;
            joined.set_fragment(None);
            let uri = joined.to_string();
            if !seen.insert(uri.clone()) {
                continue;
            }
            let parsed =
                jsonschema::Uri::parse(uri.clone()).map_err(|e| ResolveError::BundleError {
                    message: format!("invalid URI {uri}: {e:?}"),
                })?;
            let fetched = retriever
                .retrieve(&parsed)
                .map_err(|e| ResolveError::BundleError {
                    message: format!("cannot load referenced schema {uri}: {e}"),
                })?;
            let fetched_base = base(&fetched, &uri);
            queue.push((fetched.clone(), fetched_base.clone()));
            resources.push((uri.clone(), fetched.clone()));
            // Register under the declared `$id` too when it differs — and
            // refuse two *different* documents claiming the same canonical
            // `$id`: silent first-wins would bind `$id`-relative refs to
            // whichever file the crawl happened to reach first.
            if fetched_base != uri {
                match claimed.get(&fetched_base) {
                    None => {
                        claimed.insert(fetched_base.clone(), uri.clone());
                        seen.insert(fetched_base.clone());
                        resources.push((fetched_base, fetched));
                    }
                    Some(first) if *first != uri => {
                        return Err(ResolveError::BundleError {
                            message: format!(
                                "distinct schema documents claim the same canonical $id {fetched_base}: {first} and {uri}"
                            ),
                        });
                    }
                    Some(_) => {}
                }
            }
        }
    }
    Ok(resources)
}

/// Bundle references using a local physical base directory.
pub fn bundle_refs(schema: &mut Value, base_dir: &Path) -> Result<(), ResolveError> {
    let base_dir = canonical_dir(base_dir);
    let base_uri = directory_uri(&base_dir)?;
    flatten(schema, Some(base_dir), base_uri, None)
}

/// Bundle references, mapping an absolute URL prefix to a local directory.
pub fn bundle_refs_with_url_mapping(
    schema: &mut Value,
    base_dir: &Path,
    local_base: &Path,
    remote_base: &str,
) -> Result<(), ResolveError> {
    let base_dir = canonical_dir(base_dir);
    let base_uri = directory_uri(&base_dir)?;
    flatten(
        schema,
        Some(base_dir),
        base_uri,
        Some((local_base.to_path_buf(), remote_base.to_string())),
    )
}

/// Bundle references by fetching remote resources.
#[cfg(feature = "remote")]
pub fn bundle_refs_remote(schema: &mut Value, base_url: &str) -> Result<(), ResolveError> {
    flatten(schema, None, base_url.to_string(), None)
}

/// `file://` base URI for a directory, percent-encoded by the `url` crate.
/// `canonical_dir` has already absolutized the path (empty input anchors at
/// the cwd), so conversion only fails on genuinely non-representable paths.
fn directory_uri(dir: &Path) -> Result<String, ResolveError> {
    Url::from_directory_path(dir)
        .map(String::from)
        .map_err(|()| bundle_error(format!("not a usable base directory: {}", dir.display())))
}

fn bundle_error(message: impl Into<String>) -> ResolveError {
    ResolveError::BundleError {
        message: message.into(),
    }
}

/// Empty parent dirs (bare `schema.json` inputs) anchor at the cwd.
fn canonical_dir(dir: &Path) -> PathBuf {
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf())
}

fn flatten(
    schema: &mut Value,
    base_dir: Option<PathBuf>,
    base_uri: String,
    mapping: Option<(PathBuf, String)>,
) -> Result<(), ResolveError> {
    // Work on a copy: the input must stay untouched on any error path.
    let mut work = schema.clone();
    prepare_document(&mut work)?;

    let mut prefixes = Vec::new();
    if let (Some(id), Some(dir)) = (work.get("$id").and_then(Value::as_str), &base_dir) {
        if let Some((id_dir, _)) = id.rsplit_once('/') {
            prefixes.push((format!("{id_dir}/"), dir.clone()));
        }
    }

    let retriever = UcpRetriever {
        prefixes: std::sync::Mutex::new(prefixes),
        mapping,
    };

    // Upstream dereference resolves lazily-discovered external refs only at
    // registry-build time; refs found *inside fetched documents* would hit a
    // closed registry. Pre-crawl the transitive closure so every resource is
    // present up front.
    let resources = crawl_external_refs(&work, &base_uri, &retriever)?;
    let mut builder = jsonschema::Registry::new();
    for (uri, doc) in resources {
        builder = builder
            .add(&uri, jsonschema::Resource::from_contents(doc))
            .map_err(|e| ResolveError::BundleError {
                message: format!("invalid schema resource {uri}: {e}"),
            })?;
    }
    let registry = builder.prepare().map_err(|e| ResolveError::BundleError {
        message: format!("building schema registry: {e}"),
    })?;
    let mut flat = jsonschema::options()
        .with_base_uri(base_uri)
        .with_registry(&registry)
        .dereference(&work)
        .map_err(|e| ResolveError::BundleError {
            message: format!("failed to bundle schema: {e}"),
        })?;

    let _ = collapse_and_strip(&mut flat, true);
    unmask_instance_refs(&mut flat);
    *schema = flat;
    Ok(())
}

/// Prepare a document for the pipeline. Every document — the root and every
/// fetched reference — passes through here exactly once, so the two
/// protections upstream dereference lacks are structural, not per-call-site:
/// instance-data `$ref`s are masked and `$ref` siblings are hoisted. A
/// future document source that skips this function cannot exist without
/// also skipping retrieval.
fn prepare_document(doc: &mut Value) -> Result<(), ResolveError> {
    mask_instance_refs(doc)?;
    hoist_ref_siblings(doc);
    Ok(())
}

/// Rename `$ref` keys inside instance-data subtrees so no resolver touches
/// them. Restored verbatim by [`unmask_instance_refs`] on the final output
/// (fetched documents inline into it, so their masks unwind there too).
///
/// Rejects the sentinel name anywhere in the document first — schema or
/// instance position, root or fetched — because the final unmask is a blind
/// whole-tree rename.
fn mask_instance_refs(schema: &mut Value) -> Result<(), ResolveError> {
    fn reject_sentinels(value: &Value) -> Result<(), ResolveError> {
        match value {
            Value::Object(obj) => {
                if obj.contains_key(INSTANCE_REF_SENTINEL) {
                    return Err(ResolveError::BundleError {
                        message: format!("reserved member name: {INSTANCE_REF_SENTINEL}"),
                    });
                }
                obj.values().try_for_each(reject_sentinels)
            }
            Value::Array(arr) => arr.iter().try_for_each(reject_sentinels),
            _ => Ok(()),
        }
    }

    /// Rebuild objects in place so member order is preserved exactly:
    /// instance data must round-trip byte-equivalent (JSON object equality
    /// is unordered, but emitted artifacts and order-sensitive comparators
    /// must see the original bytes).
    fn rename(value: &mut Value) {
        match value {
            Value::Object(obj) => {
                if obj.contains_key("$ref") {
                    let entries: Vec<(String, Value)> = std::mem::take(obj)
                        .into_iter()
                        .map(|(k, v)| {
                            if k == "$ref" {
                                (INSTANCE_REF_SENTINEL.to_string(), v)
                            } else {
                                (k, v)
                            }
                        })
                        .collect();
                    obj.extend(entries);
                }
                obj.values_mut().for_each(rename);
            }
            Value::Array(arr) => arr.iter_mut().for_each(rename),
            _ => {}
        }
    }

    reject_sentinels(schema)?;
    for_each_schema_object_mut(schema, &mut |obj| {
        for keyword in INSTANCE_DATA_KEYWORDS {
            if let Some(v) = obj.get_mut(*keyword) {
                rename(v);
            }
        }
    });
    Ok(())
}

/// Inverse of [`mask_instance_refs`], applied blindly: sentinels only exist
/// where the mask put them.
fn unmask_instance_refs(value: &mut Value) {
    match value {
        Value::Object(obj) => {
            if obj.contains_key(INSTANCE_REF_SENTINEL) {
                let entries: Vec<(String, Value)> = std::mem::take(obj)
                    .into_iter()
                    .map(|(k, v)| {
                        if k == INSTANCE_REF_SENTINEL {
                            ("$ref".to_string(), v)
                        } else {
                            (k, v)
                        }
                    })
                    .collect();
                obj.extend(entries);
            }
            obj.values_mut().for_each(unmask_instance_refs);
        }
        Value::Array(arr) => arr.iter_mut().for_each(unmask_instance_refs),
        _ => {}
    }
}

/// Visit `schema` and every object reachable from it, *except* subtrees under
/// instance-data keywords (`const`, `enum`, `default`, `examples`).
///
/// Deliberately default-open rather than keyword-guided: UCP nests schemas
/// under domain positions no keyword table can enumerate — capability
/// containers (`$defs/<name>/platform_schema`), embedded-transport method
/// declarations (`embedded.methods.<m>.result.schema`), and future
/// extension carriers. A closed walker silently skips those, leaving their
/// refs uncrawled and their siblings unprotected. The only positions that
/// must never be treated as schema are instance data, which is exactly what
/// the exclusion list names. `$ref`-shaped payload under *non-standard*
/// carriers remains out of reach of any static rule; upstream dereference
/// walks blindly there too, so behavior matches the validator.
pub(crate) fn for_each_schema_object_mut(
    schema: &mut Value,
    f: &mut impl FnMut(&mut Map<String, Value>),
) {
    match schema {
        Value::Object(obj) => {
            f(obj);
            for (key, child) in obj.iter_mut() {
                if !INSTANCE_DATA_KEYWORDS.contains(&key.as_str()) {
                    for_each_schema_object_mut(child, f);
                }
            }
        }
        Value::Array(arr) => {
            for child in arr.iter_mut() {
                for_each_schema_object_mut(child, f);
            }
        }
        _ => {}
    }
}

/// Immutable twin of [`for_each_schema_object_mut`].
pub(crate) fn for_each_schema_object(schema: &Value, f: &mut impl FnMut(&Map<String, Value>)) {
    match schema {
        Value::Object(obj) => {
            f(obj);
            for (key, child) in obj {
                if !INSTANCE_DATA_KEYWORDS.contains(&key.as_str()) {
                    for_each_schema_object(child, f);
                }
            }
        }
        Value::Array(arr) => {
            for child in arr {
                for_each_schema_object(child, f);
            }
        }
        _ => {}
    }
}

/// `{"$ref": R, ...siblings}` → `{"allOf": [{"$ref": R}], ...siblings}`.
///
/// Draft 2020-12 evaluates `$ref` siblings conjunctively; upstream
/// `dereference` replaces the carrying object with the target, dropping
/// siblings (43 `ucp_*` annotations ride as `$ref` siblings in the UCP
/// corpus alone). Hoisting makes the conjunction structural before
/// dereference; `collapse_and_strip` re-merges afterwards. Only schema
/// positions are visited: `$ref`-shaped instance data stays untouched.
fn hoist_ref_siblings(value: &mut Value) {
    for_each_schema_object_mut(value, &mut |obj| {
        let has_ref = matches!(obj.get("$ref"), Some(Value::String(_)));
        if !has_ref || obj.len() == 1 {
            return;
        }
        match obj.get_mut("allOf") {
            None => {
                let r = obj.remove("$ref").expect("checked above");
                obj.insert(
                    "allOf".to_string(),
                    Value::Array(vec![serde_json::json!({ "$ref": r })]),
                );
            }
            // An authored allOf coexisting with `$ref`: the ref joins the
            // conjunction as one more branch. Skipping it here would hand
            // the object to upstream dereference, which replaces it wholesale
            // and silently drops colliding use-site constraints.
            Some(Value::Array(_)) => {
                let r = obj.remove("$ref").expect("checked above");
                let Some(Value::Array(branches)) = obj.get_mut("allOf") else {
                    unreachable!("matched above");
                };
                branches.push(serde_json::json!({ "$ref": r }));
            }
            // Malformed allOf (not an array): leave untouched for the
            // validator to reject.
            Some(_) => {}
        }
    });
}

/// Post-dereference cleanup, one guided walk over schema positions:
/// - merge single-branch `allOf` conjunctions flat when no keyword conflicts
///   (a one-branch `allOf` is semantically identical to inlining it);
/// - drop non-root `$id`/`$schema` from subtrees with no retained `$ref`
///   (nothing needs their base once materialized; §8.1.1 forbids stray
///   `$schema`); keep them where a retained cyclic `$ref` still resolves
///   against that base.
///
/// Returns whether any schema position at or below this node still carries a
/// `$ref`, computed bottom-up during the same walk (`$ref`-shaped instance
/// data does not count).
fn collapse_and_strip(schema: &mut Value, is_root: bool) -> bool {
    let Value::Object(_) = schema else {
        return false;
    };

    // Children first so nested one-branch allOfs collapse before parents.
    // Same default-open traversal as the walkers: recurse everywhere except
    // instance-data subtrees, whose values must survive byte-for-byte.
    let mut has_ref = false;
    {
        let obj = schema.as_object_mut().expect("checked above");
        for (key, child) in obj.iter_mut() {
            if !INSTANCE_DATA_KEYWORDS.contains(&key.as_str()) {
                has_ref |= collapse_children(child);
            }
        }
    }

    let obj = schema.as_object_mut().expect("checked above");
    // Merge {"allOf":[X], ...siblings} flat unless a *constraint* genuinely
    // conflicts. A branch key merges when it is absent from the parent,
    // equal-valued, or annotation-class (title, description, ucp_* …) —
    // where the use site wins, matching how authors annotate `$ref`
    // occurrences. Only conflicting constraints (e.g. two different
    // `maximum`s) keep the `allOf` conjunction, which is the Draft 2020-12
    // sibling semantics. The wrapper's own `allOf` key is being consumed,
    // so a branch-level `allOf` takes its place rather than colliding.
    let mergeable = match obj.get("allOf") {
        Some(Value::Array(branches)) if branches.len() == 1 => match &branches[0] {
            Value::Object(branch) => {
                !branch.is_empty()
                    && branch.iter().all(|(k, v)| {
                        k == "allOf"
                            || !obj.contains_key(k)
                            || obj.get(k) == Some(v)
                            || is_annotation_keyword(k)
                    })
            }
            _ => false,
        },
        _ => false,
    };
    if mergeable {
        let Some(Value::Array(mut branches)) = obj.remove("allOf") else {
            unreachable!("checked above");
        };
        let Value::Object(branch) = branches.remove(0) else {
            unreachable!("checked above");
        };
        for (k, v) in branch {
            obj.entry(k).or_insert(v);
        }
    }
    has_ref |= obj.contains_key("$ref");
    if !is_root && !has_ref {
        obj.remove("$id");
        obj.remove("$schema");
    }
    has_ref
}

/// Recurse [`collapse_and_strip`] through arrays and objects uniformly,
/// propagating the retained-`$ref` flag upward.
fn collapse_children(value: &mut Value) -> bool {
    match value {
        Value::Object(_) => collapse_and_strip(value, false),
        Value::Array(arr) => arr
            .iter_mut()
            .fold(false, |acc, v| acc | collapse_children(v)),
        _ => false,
    }
}

/// Keywords that annotate rather than constrain. When a `$ref` use site and
/// its target both carry one, the use site's copy wins on merge — authors
/// annotate the occurrence, and validation outcomes cannot change.
fn is_annotation_keyword(keyword: &str) -> bool {
    matches!(
        keyword,
        "title"
            | "description"
            | "$comment"
            | "examples"
            | "default"
            | "deprecated"
            | "readOnly"
            | "writeOnly"
            | "ucp_request"
            | "ucp_response"
    )
}

/// Anchor retained root-local refs in a subtree extracted from `source_doc`.
///
/// Bundling retains root-local refs only for recursion; when a subtree is
/// transplanted into a foreign document (composition extraction, `--def`
/// selection), those refs would rebase onto the new root — the same
/// resource-identity bug bundling fixes. For each `$ref` matching
/// `qualifies`, rewrite it to an absolute URI under the source document's
/// `$id` (`fallback_uri` when it declares none), and embed one copy of the
/// source document under `$defs/<embed_key>` with that `$id` intact so the
/// rewritten refs resolve. The embedded copy keeps its own `$defs` even
/// though the host may carry the same entries: refs inside the copy resolve
/// against the copy's base, and pruning them would dangle those. Refs inside
/// the copy are never rewritten — its `$id` anchors them.
///
/// Dormant (returns `Ok(false)`, subtree untouched) when nothing qualifies:
/// every shipped UCP schema today. Errors when `$defs/<embed_key>` is
/// already occupied or not an object.
pub(crate) fn anchor_to_source(
    subtree: &mut Value,
    source_doc: &Value,
    fallback_uri: &str,
    embed_key: &str,
    qualifies: impl Fn(&str) -> bool,
) -> Result<bool, String> {
    let mut needs_anchor = false;
    for_each_schema_object(subtree, &mut |obj| {
        if matches!(obj.get("$ref"), Some(Value::String(r)) if qualifies(r)) {
            needs_anchor = true;
        }
    });
    if !needs_anchor {
        return Ok(false);
    }

    let source_uri = source_doc
        .get("$id")
        .and_then(Value::as_str)
        .unwrap_or(fallback_uri)
        .to_string();

    for_each_schema_object_mut(subtree, &mut |obj| {
        if let Some(Value::String(r)) = obj.get("$ref") {
            if qualifies(r) {
                let fragment = r.strip_prefix('#').expect("qualifying refs are root-local");
                let target = if fragment.is_empty() {
                    source_uri.clone()
                } else {
                    format!("{source_uri}#{fragment}")
                };
                obj.insert("$ref".to_string(), Value::String(target));
            }
        }
    });

    let mut embedded = source_doc.clone();
    if let Value::Object(doc) = &mut embedded {
        doc.entry("$id")
            .or_insert_with(|| Value::String(source_uri.clone()));
    }
    let Value::Object(subtree_obj) = subtree else {
        return Ok(false);
    };
    // The embedded copy is about to claim `source_uri`. A host that claims it
    // too puts two resources behind one URI, and the refs just rewritten bind
    // to the host instead of the copy — silently dropping the source root's
    // constraints. The host is a wrapper we synthesized; the copy is the real
    // resource, so the host yields. Refs that relied on the host answering
    // that URI still resolve, into the copy, which carries the same content.
    // Distinct documents colliding on one `$id` is a different, genuine error
    // and is rejected during bundling.
    if subtree_obj.get("$id").and_then(Value::as_str) == Some(source_uri.as_str()) {
        subtree_obj.remove("$id");
    }
    let defs = subtree_obj
        .entry("$defs")
        .or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(defs) = defs else {
        return Err("$defs must be an object to anchor retained refs".to_string());
    };
    if defs.contains_key(embed_key) {
        return Err(format!(
            "$defs/{embed_key} is reserved for anchoring the source document"
        ));
    }
    defs.insert(embed_key.to_string(), embedded);
    Ok(true)
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

    // A caller may legitimately give the host the source's `$id` — ambient
    // materialization does, so its absolute helper refs resolve when anchoring
    // stays dormant. Once anchoring fires, the embedded copy must win that URI
    // or the source root's constraints stop reaching the retained ref. Asserted
    // as verdicts through a Draft 2020-12 validator, not as emitted text.
    #[test]
    fn anchoring_takes_the_uri_from_a_host_that_claims_it() {
        let source = serde_json::json!({
            "$id": "https://example.test/instrument.json",
            "type": "object",
            "required": ["handler_id"],
            "properties": { "handler_id": { "type": "string" } }
        });
        let mut host = serde_json::json!({
            "$id": "https://example.test/instrument.json",
            "allOf": [{ "$ref": "#" }]
        });

        let anchored = anchor_to_source(
            &mut host,
            &source,
            "urn:ucp-schema:test",
            "__ucp_source",
            |r| r == "#",
        )
        .unwrap();
        assert!(anchored, "a bare `#` must trigger anchoring");
        assert!(
            host.get("$id").is_none(),
            "the host must yield the URI to the embedded copy, got {host:#}"
        );

        let validator = jsonschema::validator_for(&host).expect("anchored host must compile");
        assert!(
            !validator.is_valid(&serde_json::json!({})),
            "the source root's required[handler_id] must reach the retained ref"
        );
        assert!(validator.is_valid(&serde_json::json!({ "handler_id": "h1" })));
    }

    // Anchoring is dormant without a qualifying ref, so a host `$id` that no
    // embedded copy contests is left alone.
    #[test]
    fn dormant_anchoring_leaves_a_host_id_untouched() {
        let source = serde_json::json!({ "$id": "https://example.test/a.json" });
        let mut host = serde_json::json!({
            "$id": "https://example.test/a.json",
            "$ref": "#/$defs/x",
            "$defs": { "x": { "type": "string" } }
        });

        let anchored = anchor_to_source(&mut host, &source, "urn:x", "__ucp_source", |r| {
            r.starts_with('#') && !r.starts_with("#/$defs/")
        })
        .unwrap();
        assert!(
            !anchored,
            "nothing qualifies, so anchoring must stay dormant"
        );
        assert_eq!(host["$id"], "https://example.test/a.json");
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
