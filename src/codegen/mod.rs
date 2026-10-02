//! Modular JSON Schema 2020-12 type-bundle code generation (`generate-types`).

mod compose;
mod hoist;
pub mod normalizer;
pub mod profile;
mod reachability;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::codegen::compose::compose_active_extensions;
use crate::codegen::hoist::{hoist_defs, insert_sliced_or_normalized_def};
use crate::codegen::normalizer::align_directional_refs;
pub use crate::codegen::profile::{parse_profile_source, ParsedProfile, RestServiceBinding};
use crate::codegen::reachability::{
    compute_reachable_closure, load_all_schemas, select_active_schemas,
};
use crate::error::{ComposeError, ResolveError};
use crate::loader::{for_each_schema_object, for_each_schema_object_mut};

/// Configuration options for [`compile_types`] and [`generate_types`].
#[derive(Debug, Clone)]
pub struct GenerateTypesOptions {
    pub profile: Option<String>,
    pub capabilities: Option<Vec<String>>,
    pub extensions: Option<Vec<String>>,
    pub schema_dir: Option<PathBuf>,
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
        let collected: Vec<String> = caps.into_iter().map(Into::into).collect();
        self.capabilities = (!collected.is_empty()).then_some(collected);
        self
    }

    /// Set active extension filters (accepts string literals without `.to_string()`).
    pub fn extensions(mut self, exts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let collected: Vec<String> = exts.into_iter().map(Into::into).collect();
        self.extensions = (!collected.is_empty()).then_some(collected);
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
    // Stage 1: Entrypoint & Transitive $ref Reachability Crawl
    let (
        mut loaded,
        reachable_indices,
        active_cap_indices,
        active_ext_indices,
        active_capabilities,
    ) = if let Some(profile_source) = options.profile.as_deref() {
        let has_caps = options
            .capabilities
            .as_deref()
            .is_some_and(|s| !s.is_empty());
        let has_exts = options.extensions.as_deref().is_some_and(|s| !s.is_empty());
        if options.schema_dir.is_some() || has_caps || has_exts {
            return Err(CodegenError::ResolveError(ResolveError::InvalidSchema {
                message:
                    "--profile cannot be combined with --schema-dir, --capability, or --extension"
                        .to_string(),
            }));
        }
        profile::load_and_compute_profile_closure(profile_source)?
    } else {
        let Some(schema_dir) = options.schema_dir.as_deref() else {
            return Err(CodegenError::ResolveError(ResolveError::InvalidSchema {
                message: "--schema-dir is required when --profile is not provided".to_string(),
            }));
        };
        if !schema_dir.exists() {
            return Err(CodegenError::ResolveError(ResolveError::FileNotFound {
                path: schema_dir.to_path_buf(),
            }));
        }
        let loaded = load_all_schemas(schema_dir)?;
        let cap_queries = options.capabilities.as_deref().filter(|s| !s.is_empty());
        let ext_queries = options.extensions.as_deref().filter(|s| !s.is_empty());
        let (active_cap_indices, active_ext_indices, active_capabilities) =
            select_active_schemas(&loaded, cap_queries, ext_queries)?;
        let include_all = cap_queries.is_none() && ext_queries.is_none();
        let reachable_indices = compute_reachable_closure(
            &loaded,
            &active_cap_indices,
            &active_ext_indices,
            &active_capabilities,
            include_all,
        );
        (
            loaded,
            reachable_indices,
            active_cap_indices,
            active_ext_indices,
            active_capabilities,
        )
    };

    // Stage 2: Upfront $defs Hoisting & Collision Qualification
    let mut defs = BTreeMap::new();
    let mut sliced_base_names = BTreeSet::new();
    let (mut root_raw_schemas, pending_overlays) = hoist_defs(
        &mut loaded,
        &reachable_indices,
        &active_cap_indices,
        &active_ext_indices,
        &active_capabilities,
        &mut defs,
        &mut sliced_base_names,
    )?;

    // Stage 3: Capability & Sub-Type Extension Composition
    let mut inlined_mixin_defs = BTreeSet::new();
    let (capability_resources, container_schemas) = compose_active_extensions(
        &loaded,
        &active_cap_indices,
        &active_ext_indices,
        pending_overlays,
        &mut root_raw_schemas,
        &mut defs,
        &mut inlined_mixin_defs,
    )?;

    // Stage 4: Inline Conditional Variant Hoisting (added in Phase 3 / Task 6)

    // Stage 5: Directional Slicing & Base Normalization
    for (base_name, raw_schema) in &root_raw_schemas {
        insert_sliced_or_normalized_def(
            raw_schema,
            base_name,
            base_name,
            capability_resources.contains_key(base_name),
            &mut defs,
            &mut sliced_base_names,
        )?;
    }

    // Stage 6: Directional $ref Alignment
    align_all_directional_refs(&mut defs, &sliced_base_names);

    // Stage 7: Ordered anyOf Union Lowering & Subtype Registration (added in Phase 3 / Task 7)

    // Stage 8: Empty-Object, Inlined-Mixin & Unreachable Request-Slice Pruning
    let active_cap_root_requests: BTreeSet<String> = active_cap_indices
        .iter()
        .flat_map(|&idx| {
            let stem = &loaded[idx].stem_pascal;
            [
                format!("{stem}CreateRequest"),
                format!("{stem}UpdateRequest"),
                format!("{stem}CompleteRequest"),
            ]
        })
        .collect();
    prune_empty_object_defs(&mut defs, &inlined_mixin_defs, &active_cap_root_requests);

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

pub(super) type SchemaMap = BTreeMap<String, Value>;

pub(super) fn local_def_ref(val: &Value) -> Option<&str> {
    val.get("$ref")?.as_str()?.strip_prefix("#/$defs/")
}

pub(super) fn has_root_schema_body(val: &Value) -> bool {
    val.is_object() && !is_empty_object_schema(val)
}

fn align_all_directional_refs(
    defs: &mut BTreeMap<String, Value>,
    sliced_base_names: &BTreeSet<String>,
) {
    let known_defs: BTreeSet<String> = defs.keys().cloned().collect();
    for (def_name, schema_val) in defs.iter_mut() {
        let Some(suffix) = ["CreateRequest", "UpdateRequest", "CompleteRequest"]
            .into_iter()
            .find(|s| def_name.ends_with(s))
        else {
            continue;
        };
        align_directional_refs(schema_val, suffix, &known_defs);
        for_each_schema_object_mut(schema_val, &mut |obj| {
            let Some(Value::Array(all_of)) = obj.get_mut("allOf") else {
                return;
            };
            all_of.retain(|branch| {
                local_def_ref(branch).is_none_or(|target| !sliced_base_names.contains(target))
            });
        });
    }
}

fn prune_empty_object_defs(
    defs: &mut BTreeMap<String, Value>,
    inlined_mixin_defs: &BTreeSet<String>,
    active_cap_root_requests: &BTreeSet<String>,
) {
    loop {
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
        let before = defs.len();
        defs.retain(|name, val| {
            if name == "ErrorResponse" || referenced.contains(name) {
                return true;
            }
            if is_empty_object_schema(val) || inlined_mixin_defs.contains(name) {
                return false;
            }
            let is_request_slice = ["CreateRequest", "UpdateRequest", "CompleteRequest"]
                .iter()
                .any(|s| name.ends_with(s));
            !(is_request_slice && !active_cap_root_requests.contains(name))
        });
        if defs.len() == before {
            break;
        }
    }
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
    let has_scalar_constraints = ["enum", "const", "pattern", "items"]
        .iter()
        .any(|k| obj.contains_key(*k));

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
                "InlinedMixin".to_string(),
                json!({ "type": "object", "properties": { "x": { "type": "string" } } }),
            ),
            (
                "DeadChildCreateRequest".to_string(),
                json!({ "type": "object", "properties": { "leaf": { "$ref": "#/$defs/DeadLeafCreateRequest" } } }),
            ),
            (
                "DeadLeafCreateRequest".to_string(),
                json!({ "type": "object", "properties": { "id": { "type": "string" } } }),
            ),
            (
                "CheckoutCreateRequest".to_string(),
                json!({ "type": "object", "properties": { "id": { "type": "string" } } }),
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
                json!({ "type": "object", "propertyNames": { "pattern": "^[a-z]+$" }, "additionalProperties": { "type": "boolean" } }),
            ),
            (
                "Holder".to_string(),
                json!({ "type": "object", "properties": { "marker": { "$ref": "#/$defs/ReferencedEmpty" } } }),
            ),
        ]);

        prune_empty_object_defs(
            &mut defs,
            &BTreeSet::from(["InlinedMixin".to_string()]),
            &BTreeSet::from(["CheckoutCreateRequest".to_string()]),
        );

        for pruned in [
            "UnreferencedEmpty",
            "InlinedMixin",
            "DeadChildCreateRequest",
            "DeadLeafCreateRequest",
        ] {
            assert!(!defs.contains_key(pruned), "expected {pruned} to be pruned");
        }
        for retained in [
            "CheckoutCreateRequest",
            "ReferencedEmpty",
            "ErrorResponse",
            "ActionsMap",
            "Holder",
        ] {
            assert!(
                defs.contains_key(retained),
                "expected {retained} to be retained"
            );
        }
    }

    #[test]
    fn options_builders_and_error_exit_codes_behave_as_expected() {
        let opts = GenerateTypesOptions::new()
            .profile("https://example.com/profile.json")
            .schema_dir("/some/dir")
            .capabilities(Vec::<&str>::new())
            .extensions(Vec::<&str>::new())
            .title("");
        assert_eq!(
            opts.profile.as_deref(),
            Some("https://example.com/profile.json")
        );
        assert!(opts.capabilities.is_none());
        assert!(opts.extensions.is_none());
        assert_eq!(opts.title, "");

        let conflict_err =
            compile_types(&opts).expect_err("combining profile and schema_dir must error");
        assert_eq!(conflict_err.exit_code(), 2);

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
