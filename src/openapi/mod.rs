//! OpenAPI 3.1 Exporter for UCP Schemas.
//!
//! Compiles UCP JSON Schemas (Draft 2020-12) into standard, valid OpenAPI 3.1.0 specifications:
//! - Normalizes UCP annotations and directional request/response models.
//! - Hoists `$defs` schemas to top-level `components.schemas` with self-ref rewriting to parent.
//! - Transforms `allOf` + `if`/`then` conditional branches and `oneOf` unions into OpenAPI 3.1 `discriminator`s (PR #688).
//! - Distributes properties in bare `anyOf` branches (e.g. `ValueConstraint`).
//! - Projects REST operations (`POST`, `GET`, `PUT`) with standard protocol headers and RFC 9421 message security.
//! - Emits deterministic, cleanly ordered JSON.

pub mod discriminator;
pub mod normalizer;
pub mod operations;
pub mod types;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::compose::{capability_short_name, is_container_schema};
use crate::error::ResolveError;
use crate::loader::collect_schema_files;
use crate::namespace::is_reverse_domain_name;
use discriminator::{synthesize_oneof_discriminators, to_pascal_case};
use normalizer::{
    is_generic_def_name, normalize_component_schema, rewrite_defs_refs_to_components,
    rewrite_self_refs_to_parent, slice_directional_schemas,
};
use operations::{
    build_standard_parameters, build_standard_security_schemes, default_error_response_schema,
    default_operation_security, project_container_operations, project_discovery_operations,
    project_resource_operations,
};
pub use types::*;

/// Options configuring the OpenAPI 3.1 specification export.
#[derive(Debug, Clone)]
pub struct ExportOpenApiOptions {
    /// Directory containing UCP JSON schema files (e.g. `./schemas`).
    pub schema_dir: PathBuf,
    /// API title injected into `info.title`.
    pub title: String,
    /// API version injected into `info.version`.
    pub api_version: String,
    /// Optional API description injected into `info.description`.
    pub description: Option<String>,
    /// Target domain profile (e.g. `shopping`, `all`).
    pub profile: Option<String>,
    /// Strict mode: inject `additionalProperties: false` on objects.
    pub strict: bool,
}

impl Default for ExportOpenApiOptions {
    fn default() -> Self {
        Self {
            schema_dir: PathBuf::from("./schemas"),
            title: "Universal Commerce Protocol (UCP) API".to_string(),
            api_version: "2026-09-01".to_string(),
            description: Some("Universal Commerce Protocol API Specification".to_string()),
            profile: None,
            strict: false,
        }
    }
}

impl ExportOpenApiOptions {
    /// Create new export options for a given schema directory.
    pub fn new(schema_dir: impl Into<PathBuf>) -> Self {
        Self {
            schema_dir: schema_dir.into(),
            ..Default::default()
        }
    }

    /// Set API title.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// Set API version.
    pub fn api_version(mut self, version: impl Into<String>) -> Self {
        self.api_version = version.into();
        self
    }

    /// Set description.
    pub fn description(mut self, description: Option<String>) -> Self {
        self.description = description;
        self
    }

    /// Set profile.
    pub fn profile(mut self, profile: Option<String>) -> Self {
        self.profile = profile;
        self
    }

    /// Set strict mode.
    pub fn strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }
}

/// Type alias for [`ExportOpenApiOptions`].
pub type OpenApiExportOptions = ExportOpenApiOptions;

/// Errors during OpenAPI export.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OpenApiExportError {
    #[error("schema path not found: {path}")]
    PathNotFound { path: PathBuf },

    #[error("failed to read schema file {path}: {source}")]
    IoError {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid JSON in schema file {path}: {source}")]
    JsonError {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("schema resolution error: {0}")]
    ResolveError(#[from] ResolveError),
}

impl OpenApiExportError {
    /// Return the CLI exit code corresponding to this error.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::PathNotFound { .. } | Self::IoError { .. } => 3,
            Self::JsonError { .. } | Self::ResolveError(_) => 2,
        }
    }
}

/// Check if a schema declares directional request/response annotations (`ucp_request`, `ucp_response`).
fn has_directional_annotations(schema: &serde_json::Value) -> bool {
    match schema {
        serde_json::Value::Object(map) => {
            if map.contains_key("ucp_request") || map.contains_key("ucp_response") {
                return true;
            }
            map.values().any(has_directional_annotations)
        }
        serde_json::Value::Array(arr) => arr.iter().any(has_directional_annotations),
        _ => false,
    }
}

/// Check if a schema represents a capability extension schema (e.g. discount.json, fulfillment.json).
///
/// Returns true if a schema extends other capabilities via `$defs[<reverse.domain>]`.
pub fn is_extension_schema(schema: &serde_json::Value) -> bool {
    if let Some(defs) = schema.get("$defs").and_then(|d| d.as_object()) {
        defs.keys()
            .any(|k| is_reverse_domain_name(k) && !k.ends_with(".json"))
    } else {
        false
    }
}

/// Check if a container schema defines operation request/response pairs in `$defs`.
pub fn has_container_operations(schema: &serde_json::Value) -> bool {
    if let Some(defs) = schema.get("$defs").and_then(|d| d.as_object()) {
        defs.keys().any(|k| {
            if let Some(op) = k.strip_suffix("_request") {
                defs.contains_key(&format!("{}_response", op))
            } else {
                false
            }
        })
    } else {
        false
    }
}

/// Check if a definition key in an extension's $defs matches a root capability name or stem.
fn def_matches_root(
    def_key: &str,
    root_name: Option<&str>,
    root_stem: &str,
    root_base_name: &str,
) -> bool {
    if let Some(r_name) = root_name {
        if def_key == r_name {
            return true;
        }
        if capability_short_name(def_key) == capability_short_name(r_name) {
            return true;
        }
    }
    if def_key == root_stem || capability_short_name(def_key) == root_stem {
        return true;
    }
    if to_pascal_case(def_key) == root_base_name
        || to_pascal_case(&capability_short_name(def_key)) == root_base_name
    {
        return true;
    }
    false
}

/// Compose an extension definition into a root capability schema.
fn compose_extension_into_root(root: &mut serde_json::Value, ext_def: &serde_json::Value) {
    if let Some(all_of) = ext_def.get("allOf").and_then(|a| a.as_array()) {
        for branch in all_of {
            // Skip branch if it is just a $ref to the base schema
            if let Some(r) = branch.get("$ref").and_then(|v| v.as_str()) {
                if r == "#" || r.ends_with(".json") || is_reverse_domain_name(r) {
                    continue;
                }
            }
            merge_extension_object(root, branch);
        }
    } else {
        merge_extension_object(root, ext_def);
    }
}

/// Merge extension properties, required fields, and constraints into a root capability schema.
fn merge_extension_object(root: &mut serde_json::Value, ext_obj: &serde_json::Value) {
    let Some(root_obj) = root.as_object_mut() else {
        return;
    };

    // 1. Merge properties
    if let Some(ext_props) = ext_obj.get("properties").and_then(|p| p.as_object()) {
        let root_props = root_obj
            .entry("properties".to_string())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
            .as_object_mut();
        if let Some(props_map) = root_props {
            for (k, v) in ext_props {
                props_map.insert(k.clone(), v.clone());
            }
        }
    }

    // 2. Merge required array
    if let Some(ext_req) = ext_obj.get("required").and_then(|r| r.as_array()) {
        let root_req = root_obj
            .entry("required".to_string())
            .or_insert_with(|| serde_json::Value::Array(Vec::new()))
            .as_array_mut();
        if let Some(req_arr) = root_req {
            for item in ext_req {
                if !req_arr.contains(item) {
                    req_arr.push(item.clone());
                }
            }
        }
    }

    // 3. Merge allOf constraints (if any non-ref branches)
    if let Some(ext_all_of) = ext_obj.get("allOf").and_then(|a| a.as_array()) {
        let root_all_of = root_obj
            .entry("allOf".to_string())
            .or_insert_with(|| serde_json::Value::Array(Vec::new()))
            .as_array_mut();
        if let Some(arr) = root_all_of {
            for branch in ext_all_of {
                if let Some(r) = branch.get("$ref").and_then(|v| v.as_str()) {
                    if r == "#" || r.ends_with(".json") || is_reverse_domain_name(r) {
                        continue;
                    }
                }
                arr.push(branch.clone());
            }
        }
    }
}

/// Single-object roots define direct properties and REST CRUD lifecycle endpoints.
/// A resource is a root capability if it declares a capability package name (or explicit route path)
/// and defines an entity with id or directional annotations, and is NOT a container schema
/// and NOT an extension schema.
fn is_root_capability_resource(_file_path: &Path, stem: &str, schema: &serde_json::Value) -> bool {
    // Container schemas and extension schemas are classified separately
    if is_container_schema(schema) || is_extension_schema(schema) {
        return false;
    }

    // Never classify protocol-level meta schemas as domain capability resources
    if matches!(stem, "profile" | "capability" | "service") {
        return false;
    }

    // Check if schema declares a root capability package name
    let has_capability_name = if let Some(name) = schema.get("name").and_then(|v| v.as_str()) {
        is_reverse_domain_name(name) && !name.contains(".types.")
    } else {
        false
    };

    let has_explicit_path = schema.get("x-ucp-path").is_some();

    if !has_capability_name && !has_explicit_path {
        return false;
    }

    // Must define an entity object with properties (or id / directional annotations / explicit lifecycle)
    if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
        props.contains_key("id")
            || has_directional_annotations(schema)
            || schema.get("x-ucp-lifecycle").is_some()
            || has_explicit_path
            || !props.is_empty()
    } else {
        false
    }
}

/// Recursively align directional references in request schemas (e.g. rewrite LineItem -> LineItemCreateRequest in CartCreateRequest)
fn align_directional_refs(val: &mut serde_json::Value, suffix: &str, existing_schemas: &[String]) {
    match val {
        serde_json::Value::Object(map) => {
            if let Some(ref_str) = map.get("$ref").and_then(|v| v.as_str()) {
                if let Some(target) = ref_str.strip_prefix("#/components/schemas/") {
                    let candidate = format!("{}{}", target, suffix);
                    if existing_schemas.iter().any(|s| s == &candidate) {
                        map.insert(
                            "$ref".to_string(),
                            serde_json::Value::String(format!(
                                "#/components/schemas/{}",
                                candidate
                            )),
                        );
                    } else if suffix == "CompleteRequest" {
                        let update_candidate = format!("{}UpdateRequest", target);
                        if existing_schemas.iter().any(|s| s == &update_candidate) {
                            map.insert(
                                "$ref".to_string(),
                                serde_json::Value::String(format!(
                                    "#/components/schemas/{}",
                                    update_candidate
                                )),
                            );
                        }
                    }
                }
            }

            // Also check and align discriminator.mapping if present
            if let Some(disc) = map.get_mut("discriminator").and_then(|d| d.as_object_mut()) {
                if let Some(mapping) = disc.get_mut("mapping").and_then(|m| m.as_object_mut()) {
                    for (_k, v) in mapping.iter_mut() {
                        if let Some(ref_str) = v.as_str() {
                            if let Some(target) = ref_str.strip_prefix("#/components/schemas/") {
                                let candidate = format!("{}{}", target, suffix);
                                if existing_schemas.iter().any(|s| s == &candidate) {
                                    *v = serde_json::Value::String(format!(
                                        "#/components/schemas/{}",
                                        candidate
                                    ));
                                } else if suffix == "CompleteRequest" {
                                    let update_candidate = format!("{}UpdateRequest", target);
                                    if existing_schemas.iter().any(|s| s == &update_candidate) {
                                        *v = serde_json::Value::String(format!(
                                            "#/components/schemas/{}",
                                            update_candidate
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }

            for (_k, v) in map.iter_mut() {
                align_directional_refs(v, suffix, existing_schemas);
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                align_directional_refs(item, suffix, existing_schemas);
            }
        }
        _ => {}
    }
}

/// Map a canonical absolute schema URL (e.g. `https://ucp.dev/draft/schemas/common/types/amount.json`)
/// to a local file path relative to `schema_dir`.
pub fn resolve_schema_url_to_path(url: &str, schema_dir: &Path) -> Option<PathBuf> {
    let url_no_frag = url.split('#').next().unwrap_or(url);

    let mut candidates = Vec::new();
    let prefixes = [
        "https://ucp.dev/draft/schemas/",
        "https://ucp.dev/schemas/",
        "http://ucp.dev/draft/schemas/",
        "http://ucp.dev/schemas/",
        "https://ucp.dev/draft/",
        "https://ucp.dev/",
        "http://ucp.dev/draft/",
        "http://ucp.dev/",
    ];

    for prefix in prefixes {
        if let Some(remainder) = url_no_frag.strip_prefix(prefix) {
            candidates.push(remainder);
        }
    }

    if let Some(pos) = url_no_frag.find("/schemas/") {
        candidates.push(&url_no_frag[pos + "/schemas/".len()..]);
    }

    for candidate in &candidates {
        let clean = candidate.trim_start_matches('/');
        let target = schema_dir.join(clean);
        if target.exists() {
            return Some(target);
        }
        if let Some(stripped) = clean.strip_prefix("schemas/") {
            let target = schema_dir.join(stripped);
            if target.exists() {
                return Some(target);
            }
        }
        if let Some(stripped) = clean.strip_prefix("draft/") {
            let target = schema_dir.join(stripped);
            if target.exists() {
                return Some(target);
            }
        }
    }

    candidates
        .first()
        .map(|c| schema_dir.join(c.trim_start_matches('/')))
}

/// Recursively collect all $ref targets (relative paths and absolute URLs) from a JSON value.
fn extract_ref_targets(
    val: &serde_json::Value,
    current_dir: &Path,
    schema_dir: &Path,
    targets: &mut HashSet<PathBuf>,
) {
    match val {
        serde_json::Value::Object(map) => {
            if let Some(ref_str) = map.get("$ref").and_then(|v| v.as_str()) {
                if !ref_str.starts_with('#') {
                    if ref_str.starts_with("http://") || ref_str.starts_with("https://") {
                        if let Some(target_path) = resolve_schema_url_to_path(ref_str, schema_dir) {
                            if let Ok(canonical) = target_path.canonicalize() {
                                targets.insert(canonical);
                            } else {
                                targets.insert(target_path);
                            }
                        }
                    } else {
                        let file_part = ref_str.split('#').next().unwrap_or("");
                        if !file_part.is_empty() {
                            let target_path = current_dir.join(file_part);
                            if let Ok(canonical) = target_path.canonicalize() {
                                targets.insert(canonical);
                            } else {
                                targets.insert(target_path);
                            }
                        }
                    }
                }
            }
            for v in map.values() {
                extract_ref_targets(v, current_dir, schema_dir, targets);
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                extract_ref_targets(item, current_dir, schema_dir, targets);
            }
        }
        _ => {}
    }
}

/// Compute the transitive closure of reachable schema files for a profile.
fn compute_profile_reachable_files(
    schema_dir: &Path,
    json_files: &[PathBuf],
    profile: &str,
) -> HashSet<PathBuf> {
    let mut visited = HashSet::new();
    let mut queue = Vec::new();

    let profile_lower = profile.to_lowercase();

    // Identify entrypoint files for the profile
    for file in json_files {
        let path_str = file.to_string_lossy().to_lowercase();
        let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("");

        let is_entrypoint = if profile_lower == "discovery" {
            stem == "profile"
                || stem.starts_with("catalog_")
                || stem.ends_with("_search")
                || stem.ends_with("_lookup")
                || path_str.ends_with("profile.json")
        } else {
            path_str.contains(&format!("/{}", profile_lower))
                || path_str.contains(&format!("\\{}", profile_lower))
                || stem.starts_with(&profile_lower)
        };

        if is_entrypoint {
            let canon = file.canonicalize().unwrap_or_else(|_| file.clone());
            if visited.insert(canon.clone()) {
                queue.push(file.clone());
            }
        }
    }

    // Transitive closure through $refs
    while let Some(current_file) = queue.pop() {
        let parent_dir = current_file.parent().unwrap_or(schema_dir);
        let content = match std::fs::read_to_string(&current_file) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let schema_val: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let mut targets = HashSet::new();
        extract_ref_targets(&schema_val, parent_dir, schema_dir, &mut targets);

        for target in targets {
            let canon = target.canonicalize().unwrap_or_else(|_| target.clone());
            if visited.insert(canon) && target.exists() {
                queue.push(target);
            }
        }
    }

    visited
}

/// Compile UCP JSON Schemas into a complete, valid OpenAPI 3.1 specification document.
pub fn export_openapi(options: &ExportOpenApiOptions) -> Result<OpenApiDoc, OpenApiExportError> {
    if !options.schema_dir.exists() {
        return Err(OpenApiExportError::PathNotFound {
            path: options.schema_dir.clone(),
        });
    }

    let json_files = collect_schema_files(&options.schema_dir);

    let reachable_filter: Option<HashSet<PathBuf>> = match options.profile.as_deref() {
        Some(p) if !p.is_empty() && !p.eq_ignore_ascii_case("all") => Some(
            compute_profile_reachable_files(&options.schema_dir, &json_files, p),
        ),
        _ => None,
    };

    let mut raw_schemas = Vec::new();
    for file_path in &json_files {
        if let Some(ref reachable) = reachable_filter {
            let canon = file_path
                .canonicalize()
                .unwrap_or_else(|_| file_path.clone());
            if !reachable.contains(&canon) {
                continue;
            }
        }

        let content =
            std::fs::read_to_string(file_path).map_err(|e| OpenApiExportError::IoError {
                path: file_path.clone(),
                source: e,
            })?;
        let value: serde_json::Value =
            serde_json::from_str(&content).map_err(|e| OpenApiExportError::JsonError {
                path: file_path.clone(),
                source: e,
            })?;
        raw_schemas.push((file_path.clone(), value));
    }

    let mut schemas = BTreeMap::new();
    let mut capability_resources: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    let mut container_schemas: Vec<(PathBuf, serde_json::Value)> = Vec::new();

    // Pass 1: Hoist internal $defs from all schemas to components.schemas upfront
    for (file_path, raw_schema) in &raw_schemas {
        let stem = file_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("schema");
        let base_name = to_pascal_case(stem);

        if let Some(defs_map) = raw_schema.get("$defs").and_then(|d| d.as_object()) {
            for (def_key, def_val) in defs_map {
                if !is_reverse_domain_name(def_key) {
                    let def_pascal = to_pascal_case(def_key);
                    let is_generic =
                        is_generic_def_name(def_key) || is_generic_def_name(&def_pascal);
                    let def_name = if is_generic {
                        format!("{}{}", base_name, def_pascal)
                    } else {
                        def_pascal.clone()
                    };
                    let mut hoisted_val = def_val.clone();

                    // Rewrite self-ref "#" to the parent schema name
                    rewrite_self_refs_to_parent(&mut hoisted_val, &base_name);
                    rewrite_defs_refs_to_components(&mut hoisted_val, Some(&base_name));

                    let normalized_def = normalize_component_schema(&hoisted_val, &def_name);
                    schemas.entry(def_name).or_insert(normalized_def);
                }
            }
        }
    }

    // Pass 2: Compose capability extensions, slice directional models, and project components
    for (file_path, raw_schema) in &raw_schemas {
        let stem = file_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("schema");
        let base_name = to_pascal_case(stem);

        let is_container = is_container_schema(raw_schema)
            && !is_extension_schema(raw_schema)
            && has_container_operations(raw_schema);

        if is_container && !is_root_capability_resource(file_path, stem, raw_schema) {
            let mut container_clone = raw_schema.clone();
            // Compose any extension operation shapes into container
            let container_name = raw_schema.get("name").and_then(|v| v.as_str());
            for (_ext_path, ext_schema) in &raw_schemas {
                if is_extension_schema(ext_schema) {
                    if let Some(defs) = ext_schema.get("$defs").and_then(|d| d.as_object()) {
                        for (def_key, def_val) in defs {
                            if def_matches_root(def_key, container_name, stem, &base_name) {
                                if let Some(ext_container_defs) =
                                    def_val.get("$defs").and_then(|d| d.as_object())
                                {
                                    if let Some(c_defs) = container_clone
                                        .get_mut("$defs")
                                        .and_then(|d| d.as_object_mut())
                                    {
                                        for (op_k, op_v) in ext_container_defs {
                                            c_defs
                                                .entry(op_k.clone())
                                                .or_insert_with(|| op_v.clone());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            container_schemas.push((file_path.clone(), container_clone));
        }

        let is_root = is_root_capability_resource(file_path, stem, raw_schema);
        let mut parent_schema = raw_schema.clone();

        // If parent schema is a container schema (only holds $defs without direct type/properties/allOf),
        // populate its body from $defs.base, $defs.response_schema, or $defs.entity
        let is_empty_container = parent_schema.get("properties").is_none()
            && parent_schema.get("allOf").is_none()
            && parent_schema.get("oneOf").is_none()
            && parent_schema.get("anyOf").is_none()
            && parent_schema.get("type").is_none();

        if is_empty_container {
            if let Some(defs) = raw_schema.get("$defs").and_then(|d| d.as_object()) {
                if let Some(base_def) = defs
                    .get("base")
                    .or_else(|| defs.get("response_schema"))
                    .or_else(|| defs.get("entity"))
                {
                    if let serde_json::Value::Object(ref mut parent_map) = parent_schema {
                        if let serde_json::Value::Object(ref base_map) = base_def {
                            for (k, v) in base_map {
                                if !parent_map.contains_key(k) {
                                    parent_map.insert(k.clone(), v.clone());
                                }
                            }
                        }
                    }
                }
            }
        }

        if is_root {
            // Compose capability extensions into this root capability before slicing
            let root_name = raw_schema.get("name").and_then(|v| v.as_str());
            for (_ext_path, ext_schema) in &raw_schemas {
                if is_extension_schema(ext_schema) {
                    if let Some(defs) = ext_schema.get("$defs").and_then(|d| d.as_object()) {
                        for (def_key, def_val) in defs {
                            if def_matches_root(def_key, root_name, stem, &base_name) {
                                compose_extension_into_root(&mut parent_schema, def_val);
                            }
                        }
                    }
                }
            }
            capability_resources.insert(base_name.clone(), parent_schema.clone());
        }

        rewrite_self_refs_to_parent(&mut parent_schema, &base_name);
        rewrite_defs_refs_to_components(&mut parent_schema, Some(&base_name));

        // Strip $defs from parent now that they are hoisted
        if let serde_json::Value::Object(ref mut map) = parent_schema {
            map.remove("$defs");
        }

        if is_root || has_directional_annotations(raw_schema) {
            // Perform directional slicing for Request / Response models
            let sliced = slice_directional_schemas(&parent_schema, &base_name, options.strict)?;
            for (name, schema) in sliced {
                schemas.insert(name, schema);
            }
        } else {
            // Standalone type / subtype model: do not emit empty container schemas
            if !crate::compose::is_container_schema(raw_schema) {
                let normalized = normalize_component_schema(&parent_schema, &base_name);
                let is_empty = normalized.get("type").and_then(|t| t.as_str()) == Some("object")
                    && normalized
                        .get("properties")
                        .and_then(|p| p.as_object())
                        .map(|p| p.is_empty())
                        .unwrap_or(true)
                    && normalized.get("allOf").is_none()
                    && normalized.get("oneOf").is_none()
                    && normalized.get("anyOf").is_none()
                    && !normalized
                        .get("additionalProperties")
                        .map(|v| v.is_object())
                        .unwrap_or(false)
                    && normalized.get("$ref").is_none();
                if !is_empty {
                    schemas.insert(base_name, normalized);
                }
            }
        }
    }

    // 2. Synthesize explicit discriminators on oneOf unions before aligning directional models
    synthesize_oneof_discriminators(&mut schemas);

    // 1c. Align directional references in request schemas (e.g. LineItem -> LineItemCreateRequest inside CartCreateRequest)
    let schema_keys: Vec<String> = schemas.keys().cloned().collect();
    for (schema_name, schema_val) in schemas.iter_mut() {
        for suffix in ["CreateRequest", "UpdateRequest", "CompleteRequest"] {
            if schema_name.ends_with(suffix) {
                align_directional_refs(schema_val, suffix, &schema_keys);
                break;
            }
        }
    }

    // Prune any empty object schemas without properties or compositions
    schemas.retain(|name, s| {
        if name == "ErrorResponse" {
            return true;
        }
        if s.get("type").and_then(|t| t.as_str()) == Some("object") {
            let has_props = s
                .get("properties")
                .and_then(|p| p.as_object())
                .map(|p| !p.is_empty())
                .unwrap_or(false);
            let has_composition = s.get("allOf").is_some()
                || s.get("oneOf").is_some()
                || s.get("anyOf").is_some()
                || s.get("$ref").is_some();
            let has_additional = s
                .get("additionalProperties")
                .map(|v| v.is_object())
                .unwrap_or(false);
            has_props || has_composition || has_additional
        } else {
            true
        }
    });

    // 3. Ensure standard ErrorResponse is present under components.schemas
    schemas
        .entry("ErrorResponse".to_string())
        .or_insert_with(default_error_response_schema);

    // 4. Build standard parameters and security schemes
    let parameters = build_standard_parameters();
    let security_schemes = build_standard_security_schemes();

    // 5. Project REST routes for capability resources
    let mut paths = BTreeMap::new();
    let mut tags = Vec::new();

    for (resource_name, schema) in &capability_resources {
        project_resource_operations(resource_name, Some(schema), &schemas, &mut paths);
        let desc = schema
            .get("description")
            .and_then(|d| d.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                schema
                    .get("title")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| format!("Operations on {} resources.", resource_name));
        tags.push(Tag {
            name: resource_name.clone(),
            description: Some(desc),
        });
    }

    // 5b. Project dynamic container capabilities
    let mut container_tags = BTreeMap::new();
    for (file_path, container_schema) in &container_schemas {
        let projected =
            project_container_operations(file_path, container_schema, &schemas, &mut paths);
        let tag_desc = container_schema
            .get("description")
            .and_then(|d| d.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                container_schema
                    .get("title")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
            });
        for tag in projected {
            container_tags
                .entry(tag)
                .or_insert_with(|| tag_desc.clone());
        }
    }

    // 5c. Project discovery profile operation
    project_discovery_operations(&schemas, &mut paths);
    if paths.contains_key("/.well-known/ucp") {
        tags.push(Tag {
            name: "Discovery".to_string(),
            description: Some("Merchant profile and capability discovery.".to_string()),
        });
    }

    for (ctag, desc_opt) in container_tags {
        if !tags.iter().any(|t| t.name == ctag) {
            let desc = desc_opt.unwrap_or_else(|| format!("Operations for {}.", ctag));
            tags.push(Tag {
                name: ctag,
                description: Some(desc),
            });
        }
    }

    let components = Components {
        schemas,
        parameters: Some(parameters),
        security_schemes: Some(security_schemes),
        responses: None,
    };

    let doc = OpenApiDoc {
        openapi: "3.1.0".to_string(),
        info: Info {
            title: options.title.clone(),
            version: options.api_version.clone(),
            description: options.description.clone(),
        },
        paths,
        components: Some(components),
        security: Some(default_operation_security()),
        tags: if tags.is_empty() { None } else { Some(tags) },
    };

    Ok(doc)
}
