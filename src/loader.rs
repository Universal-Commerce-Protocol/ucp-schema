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
// Strategy: UCP resolution (annotations, monotonicity, strict-closing) must
// see through external `$ref`s, so we materialize them. Reference *semantics*
// (base URIs, lexical scope, fragments, recursion) are delegated to
// `jsonschema::dereference`, which is tested against the official JSON Schema
// suite. We keep three local passes it cannot do for us:
//   1. root-internal refs stay refs (the validator resolves them; matches the
//      long-standing output contract) — sentineled around dereference;
//   2. `$ref` siblings are conjunctive in 2020-12; upstream dereference drops
//      them, so we hoist siblings before and merge conjunctively after
//      (upstream issue pending; see PR notes);
//   3. inlined resource copies keep `$id`/`$schema` only when a retained
//      (cyclic) `$ref` still needs that base — otherwise stripped, since
//      Draft 2020-12 §8.1.1 forbids `$schema` outside a resource root.
// ---------------------------------------------------------------------------

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
        // Protect `$ref` siblings and instance-data refs in *every* document
        // that enters the registry, not only the root: upstream dereference
        // replaces a ref-carrying object with its target (dropping siblings)
        // and chases `$ref`-shaped payload inside `const`/`enum`.
        mask_instance_refs(&mut doc)?;
        hoist_ref_siblings(&mut doc);
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
        // 2. file:// URIs load directly.
        if let Some(path) = uri.strip_prefix("file://") {
            return Ok(self.load_and_learn(Path::new(path))?);
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
            mask_instance_refs(&mut doc)?;
            hoist_ref_siblings(&mut doc);
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
        for_each_subschema(value, &mut |obj| {
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
            let joined = Url::parse(&doc_base)
                .and_then(|b| b.join(target))
                .map_err(|e| ResolveError::BundleError {
                    message: format!("invalid $ref {reference:?} against {doc_base}: {e}"),
                })?;
            let mut joined = joined;
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
    // The sentinel name is reserved everywhere, not only inside instance
    // data: the final unmask is a blind whole-tree rename, so a schema key
    // with this name would silently become `$ref` in the output.
    reject_sentinel_members(&work)?;

    let mut prefixes = Vec::new();
    if let (Some(id), Some(dir)) = (work.get("$id").and_then(Value::as_str), &base_dir) {
        if let Some((id_dir, _)) = id.rsplit_once('/') {
            prefixes.push((format!("{id_dir}/"), dir.clone()));
        }
    }

    mask_instance_refs(&mut work)?;
    hoist_ref_siblings(&mut work);

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

    collapse_and_strip(&mut flat, true);
    unmask_instance_refs(&mut flat);
    *schema = flat;
    Ok(())
}

/// Refuse any member named like the masking sentinel anywhere in the input.
fn reject_sentinel_members(value: &Value) -> Result<(), ResolveError> {
    match value {
        Value::Object(obj) => {
            if obj.contains_key(INSTANCE_REF_SENTINEL) {
                return Err(ResolveError::BundleError {
                    message: format!("reserved member name: {INSTANCE_REF_SENTINEL}"),
                });
            }
            obj.values().try_for_each(reject_sentinel_members)
        }
        Value::Array(arr) => arr.iter().try_for_each(reject_sentinel_members),
        _ => Ok(()),
    }
}

/// Sentinel used to hide `$ref` keys inside instance data from upstream
/// dereference, which walks `const`/`enum`/`default` values as if they were
/// schemas and tries to resolve payload that merely looks like a reference.
const INSTANCE_REF_SENTINEL: &str = "__ucp_instance_ref__";

/// Rename `$ref` keys inside instance-data subtrees so no resolver touches
/// them. Restored verbatim by [`unmask_instance_refs`] on the final output
/// (fetched documents inline into it, so their masks unwind there too).
fn mask_instance_refs(schema: &mut Value) -> Result<(), ResolveError> {
    fn rename(value: &mut Value, from: &str, to: &str) -> Result<(), ResolveError> {
        match value {
            Value::Object(obj) => {
                if obj.contains_key(to) {
                    return Err(ResolveError::BundleError {
                        message: format!("reserved member name in instance data: {to}"),
                    });
                }
                // Rebuild in place so member order is preserved exactly:
                // instance data must round-trip byte-equivalent (JSON object
                // equality is unordered, but emitted artifacts and
                // order-sensitive comparators must see the original bytes).
                if obj.contains_key(from) {
                    let entries: Vec<(String, Value)> = std::mem::take(obj)
                        .into_iter()
                        .map(|(k, v)| {
                            if k == from {
                                (to.to_string(), v)
                            } else {
                                (k, v)
                            }
                        })
                        .collect();
                    obj.extend(entries);
                }
                for v in obj.values_mut() {
                    rename(v, from, to)?;
                }
                Ok(())
            }
            Value::Array(arr) => arr.iter_mut().try_for_each(|v| rename(v, from, to)),
            _ => Ok(()),
        }
    }
    let mut result = Ok(());
    for_each_subschema_mut(schema, &mut |obj| {
        for keyword in INSTANCE_DATA_KEYWORDS {
            if let Some(v) = obj.get_mut(*keyword) {
                if result.is_ok() {
                    result = rename(v, "$ref", INSTANCE_REF_SENTINEL);
                }
            }
        }
    });
    result
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

/// Keywords whose values are instance data, not subschemas.
const INSTANCE_DATA_KEYWORDS: &[&str] = &["const", "enum", "default", "examples"];

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
fn for_each_subschema_mut(schema: &mut Value, f: &mut impl FnMut(&mut Map<String, Value>)) {
    match schema {
        Value::Object(obj) => {
            f(obj);
            for (key, child) in obj.iter_mut() {
                if !INSTANCE_DATA_KEYWORDS.contains(&key.as_str()) {
                    for_each_subschema_mut(child, f);
                }
            }
        }
        Value::Array(arr) => {
            for child in arr.iter_mut() {
                for_each_subschema_mut(child, f);
            }
        }
        _ => {}
    }
}

/// Immutable twin of [`for_each_subschema_mut`].
fn for_each_subschema(schema: &Value, f: &mut impl FnMut(&Map<String, Value>)) {
    match schema {
        Value::Object(obj) => {
            f(obj);
            for (key, child) in obj {
                if !INSTANCE_DATA_KEYWORDS.contains(&key.as_str()) {
                    for_each_subschema(child, f);
                }
            }
        }
        Value::Array(arr) => {
            for child in arr {
                for_each_subschema(child, f);
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
    for_each_subschema_mut(value, &mut |obj| {
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
fn collapse_and_strip(schema: &mut Value, is_root: bool) {
    let Value::Object(_) = schema else { return };

    // Children first so nested one-branch allOfs collapse before parents.
    // Same default-open traversal as the walkers: recurse everywhere except
    // instance-data subtrees, whose values must survive byte-for-byte.
    {
        let obj = schema.as_object_mut().expect("checked above");
        for (key, child) in obj.iter_mut() {
            if !INSTANCE_DATA_KEYWORDS.contains(&key.as_str()) {
                collapse_children(child);
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
    if !is_root && !contains_schema_ref(schema) {
        let obj = schema.as_object_mut().expect("checked above");
        obj.remove("$id");
        obj.remove("$schema");
    }
}

/// Recurse [`collapse_and_strip`] through arrays and objects uniformly.
fn collapse_children(value: &mut Value) {
    match value {
        Value::Object(_) => collapse_and_strip(value, false),
        Value::Array(arr) => arr.iter_mut().for_each(collapse_children),
        _ => {}
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

/// Whether any schema position at or below `schema` still carries a `$ref`.
/// `$ref`-shaped instance data (inside `const`, `enum`, …) does not count.
fn contains_schema_ref(schema: &Value) -> bool {
    let mut found = false;
    for_each_subschema(schema, &mut |obj| {
        found |= obj.contains_key("$ref");
    });
    found
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
