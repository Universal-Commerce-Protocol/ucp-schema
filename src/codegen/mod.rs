//! Modular JSON Schema 2020-12 type-bundle code generation (`generate-types`).

mod compose;
mod hoist;
pub mod normalizer;
mod reachability;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::codegen::compose::compose_active_extensions;
use crate::codegen::hoist::hoist_defs;
use crate::codegen::normalizer::{
    align_directional_refs, has_directional_annotations, normalize_def_schema,
    slice_directional_schemas,
};
use crate::codegen::reachability::{
    compute_reachable_closure, load_all_schemas, select_active_schemas, LoadedSchema,
};
use crate::error::{ComposeError, ResolveError};
use crate::loader::for_each_schema_object;

/// Configuration options for [`compile_types`] and [`generate_types`].
#[derive(Debug, Clone)]
pub struct GenerateTypesOptions {
    pub profile: Option<String>,
    pub capabilities: Option<Vec<String>>,
    pub extensions: Option<Vec<String>>,
    pub schema_dir: Option<PathBuf>,
    pub schema_remote_base: Option<String>,
    pub title: String,
    pub description: Option<String>,
}

impl Default for GenerateTypesOptions {
    fn default() -> Self {
        Self {
            profile: None,
            capabilities: None,
            extensions: None,
            schema_dir: None,
            schema_remote_base: None,
            title: "UCP Schema Types".to_string(),
            description: None,
        }
    }
}

impl GenerateTypesOptions {
    /// Create default [`GenerateTypesOptions`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the local schema directory path.
    pub fn schema_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.schema_dir = Some(dir.into());
        self
    }

    /// Set the profile path or URL.
    pub fn profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = Some(profile.into());
        self
    }

    /// Set active capability filters (accepts string literals without `.to_string()`).
    pub fn capabilities(mut self, caps: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.capabilities = Some(caps.into_iter().map(Into::into).collect());
        self
    }

    /// Set active extension filters (accepts string literals without `.to_string()`).
    pub fn extensions(mut self, exts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.extensions = Some(exts.into_iter().map(Into::into).collect());
        self
    }

    /// Set the bundle document title.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// Set the bundle document description.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// Top-level JSON Schema 2020-12 `$defs` bundle emitted by [`generate_types`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TypesBundleDoc {
    #[serde(rename = "$schema")]
    pub schema_dialect: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "$defs")]
    pub defs: BTreeMap<String, Value>,
}

/// Intermediate compilation artifacts produced by [`compile_types`].
#[derive(Debug, Clone)]
pub struct CompiledTypes {
    pub defs: BTreeMap<String, Value>,
    pub capability_resources: BTreeMap<String, Value>,
    pub container_schemas: Vec<(PathBuf, Value)>,
    /// Stores both full reverse-domain capability names (e.g. "dev.ucp.shopping.catalog.search")
    /// and canonical schema file stems (e.g. "catalog_search", "checkout") for active capabilities.
    pub active_capabilities: BTreeSet<String>,
}

/// Errors produced during type-bundle or OpenAPI code generation.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CodegenError {
    #[error("schema resolution error: {0}")]
    ResolveError(#[from] ResolveError),
    #[error("schema composition error: {0}")]
    ComposeError(#[from] ComposeError),
    #[error("profile '{profile}' does not declare any service binding with transport 'rest'")]
    NoRestServiceBinding { profile: String },
    #[error(
        "REST service binding '{service}' in profile '{profile}' is missing required 'schema' URL"
    )]
    MissingServiceSchema { profile: String, service: String },
}

impl CodegenError {
    /// Returns the CLI exit code for this error type.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::ResolveError(e) => e.exit_code(),
            Self::ComposeError(e) => e.exit_code(),
            Self::NoRestServiceBinding { .. } | Self::MissingServiceSchema { .. } => 2,
        }
    }
}

/// Compile UCP schemas into a self-contained `$defs` type table and intermediate metadata.
pub fn compile_types(options: &GenerateTypesOptions) -> Result<CompiledTypes, CodegenError> {
    let Some(schema_dir) = options.schema_dir.as_deref() else {
        return Err(CodegenError::ResolveError(ResolveError::InvalidSchema {
            message: "--schema-dir is required when --profile is not provided".to_string(),
        }));
    };
    if !schema_dir.exists() {
        return Err(CodegenError::ResolveError(ResolveError::FileNotFound {
            path: schema_dir.to_path_buf(),
        }));
    };

    // Stage 1: Entrypoint & Transitive $ref Reachability Crawl
    let loaded = load_all_schemas(schema_dir)?;
    let (active_cap_indices, active_ext_indices, active_capabilities) = select_active_schemas(
        &loaded,
        options.capabilities.as_deref(),
        options.extensions.as_deref(),
    )?;
    let include_all = options.capabilities.is_none() && options.extensions.is_none();
    let reachable_indices = compute_reachable_closure(
        &loaded,
        &active_cap_indices,
        &active_ext_indices,
        &active_capabilities,
        include_all,
    );

    // Stage 2: Upfront $defs Hoisting & Collision Qualification
    let mut defs = BTreeMap::new();
    let (working_schemas, mut root_raw_schemas, pending_overlays) = hoist_defs(
        &loaded,
        &reachable_indices,
        &active_cap_indices,
        &active_ext_indices,
        &active_capabilities,
        &mut defs,
    )?;

    // Stage 3: Capability & Sub-Type Extension Composition
    let (capability_resources, container_schemas) = compose_active_extensions(
        &loaded,
        &working_schemas,
        &active_cap_indices,
        &active_ext_indices,
        pending_overlays,
        &mut root_raw_schemas,
        &mut defs,
    )?;

    // Stage 4: Inline Conditional Variant Hoisting (added in Phase 3 / Task 6)

    // Stage 5: Directional Slicing & Base Normalization
    slice_and_normalize_defs(&loaded, &active_cap_indices, &root_raw_schemas, &mut defs)?;

    // Stage 6: Directional $ref Alignment
    align_all_directional_refs(&mut defs);

    // Stage 7: Ordered anyOf Union Lowering & Subtype Registration (added in Phase 3 / Task 7)

    // Stage 8: Empty-Object Pruning (retaining ErrorResponse)
    prune_empty_object_defs(&mut defs);

    Ok(CompiledTypes {
        defs,
        capability_resources,
        container_schemas,
        active_capabilities,
    })
}

/// Generate a self-contained JSON Schema 2020-12 `$defs` bundle document.
pub fn generate_types(options: &GenerateTypesOptions) -> Result<TypesBundleDoc, CodegenError> {
    let compiled = compile_types(options)?;
    let title = if options.title.is_empty() {
        "UCP Schema Types".to_string()
    } else {
        options.title.clone()
    };
    Ok(TypesBundleDoc {
        schema_dialect: "https://json-schema.org/draft/2020-12/schema".to_string(),
        title,
        description: options.description.clone(),
        defs: compiled.defs,
    })
}

fn slice_and_normalize_defs(
    loaded: &[LoadedSchema],
    active_cap_indices: &BTreeSet<usize>,
    root_raw_schemas: &BTreeMap<String, (usize, Value)>,
    defs: &mut BTreeMap<String, Value>,
) -> Result<(), CodegenError> {
    for (base_name, (idx, raw_schema)) in root_raw_schemas {
        let is_active_root_cap = active_cap_indices.contains(idx) && !loaded[*idx].is_container;
        if is_active_root_cap || has_directional_annotations(raw_schema) {
            for (slice_name, slice_val) in slice_directional_schemas(raw_schema, base_name)? {
                defs.insert(slice_name, slice_val);
            }
        } else {
            defs.insert(
                base_name.clone(),
                normalize_def_schema(raw_schema, base_name, Some(base_name)),
            );
        }
    }
    Ok(())
}

fn align_all_directional_refs(defs: &mut BTreeMap<String, Value>) {
    let known_defs: BTreeSet<String> = defs.keys().cloned().collect();
    for (def_name, schema_val) in defs.iter_mut() {
        let Some(suffix) = ["CreateRequest", "UpdateRequest", "CompleteRequest"]
            .into_iter()
            .find(|s| def_name.ends_with(s))
        else {
            continue;
        };
        align_directional_refs(schema_val, suffix, &known_defs);
    }
}

fn prune_empty_object_defs(defs: &mut BTreeMap<String, Value>) {
    let mut referenced = BTreeSet::new();
    for val in defs.values() {
        for_each_schema_object(val, &mut |obj| {
            if let Some(target) = obj
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|r| r.strip_prefix("#/$defs/"))
            {
                referenced.insert(target.to_string());
            }
        });
    }
    defs.retain(|name, val| {
        name == "ErrorResponse" || referenced.contains(name) || !is_empty_object_schema(val)
    });
}

fn is_empty_object_schema(val: &Value) -> bool {
    let Some(obj) = val.as_object() else {
        return false;
    };
    if !obj.get("type").is_none_or(|t| t.as_str() == Some("object")) {
        return false;
    }
    let has_props = obj
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|m| !m.is_empty());
    let has_composition = ["allOf", "oneOf", "anyOf"].iter().any(|k| {
        obj.get(*k)
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty())
    });
    let has_ref = obj.get("$ref").is_some_and(Value::is_string);
    let has_map_schema = obj.contains_key("propertyNames")
        || obj
            .get("additionalProperties")
            .is_some_and(Value::is_object);
    let has_scalar_constraints =
        obj.contains_key("enum") || obj.contains_key("const") || obj.contains_key("pattern");

    !(has_props || has_composition || has_ref || has_map_schema || has_scalar_constraints)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn prune_empty_object_defs_prunes_unreferenced_and_retains_referenced_and_error_response() {
        let mut defs = BTreeMap::from([
            (
                "UnreferencedEmpty".to_string(),
                json!({ "type": "object", "properties": {}, "additionalProperties": true }),
            ),
            (
                "ReferencedEmpty".to_string(),
                json!({ "type": "object", "additionalProperties": true }),
            ),
            (
                "ErrorResponse".to_string(),
                json!({ "type": "object", "additionalProperties": true }),
            ),
            (
                "ActionsMap".to_string(),
                json!({
                    "type": "object",
                    "propertyNames": { "pattern": "^[a-z]+$" },
                    "additionalProperties": { "type": "boolean" }
                }),
            ),
            (
                "Holder".to_string(),
                json!({
                    "type": "object",
                    "properties": { "marker": { "$ref": "#/$defs/ReferencedEmpty" } }
                }),
            ),
        ]);

        prune_empty_object_defs(&mut defs);

        assert!(!defs.contains_key("UnreferencedEmpty"));
        assert!(defs.contains_key("ReferencedEmpty"));
        assert!(defs.contains_key("ErrorResponse"));
        assert!(defs.contains_key("ActionsMap"));
        assert!(defs.contains_key("Holder"));
    }

    #[test]
    fn options_builders_and_error_exit_codes_behave_as_expected() {
        let opts = GenerateTypesOptions::new()
            .profile("https://example.com/profile.json")
            .title("");
        assert_eq!(
            opts.profile.as_deref(),
            Some("https://example.com/profile.json")
        );
        assert_eq!(opts.title, "");

        let no_rest = CodegenError::NoRestServiceBinding {
            profile: "p".to_string(),
        };
        assert_eq!(no_rest.exit_code(), 2);

        let missing_svc = CodegenError::MissingServiceSchema {
            profile: "p".to_string(),
            service: "s".to_string(),
        };
        assert_eq!(missing_svc.exit_code(), 2);
    }
}
