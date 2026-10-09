//! Layer 2 Profile-Driven OpenAPI 3.1.0 Generation (`generate-openapi`).
//!
//! Binds a UCP discovery profile's declared REST service OpenAPI 3.1 templates
//! (`ucp.services[<service>][transport == "rest"].schema`) to Layer 1's compiled
//! capability and extension schemas (`compile_types`), pruning inactive routes,
//! replacing snake_case template wrapper aliases with canonical PascalCase
//! `#/components/schemas/<Name>` references, and pruning `components` to the
//! transitive reachability closure of the retained routes and webhooks.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::codegen::normalizer::ref_to_def_name;
use crate::codegen::profile::{
    parse_profile_source, resolve_service_schema_target, RestServiceBinding,
};
use crate::codegen::{compile_types, CodegenError, GenerateTypesOptions};
use crate::error::ResolveError;
use crate::loader::{load_schema_auto, INSTANCE_DATA_KEYWORDS};

const OAS_3_1_DIALECT: &str = "https://spec.openapis.org/oas/3.1/dialect/base";

const HTTP_METHODS: &[&str] = &[
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

/// Configuration options for [`generate_openapi`].
#[derive(Debug, Clone)]
pub struct GenerateOpenApiOptions {
    pub profile: String,
    pub service: Option<String>,
    pub server_url: Option<String>,
}

impl GenerateOpenApiOptions {
    /// Create [`GenerateOpenApiOptions`] for a profile file path or URL.
    pub fn new(profile: impl Into<String>) -> Self {
        Self {
            profile: profile.into(),
            service: None,
            server_url: None,
        }
    }

    /// Create [`GenerateOpenApiOptions`] scoped to a UCP discovery profile path or URL.
    pub fn from_profile(profile: impl Into<String>) -> Self {
        Self::new(profile)
    }

    /// Filter to a specific REST service FQDN (e.g. `"dev.ucp.shopping"`).
    pub fn service(mut self, service: impl Into<String>) -> Self {
        let svc = service.into();
        self.service = (!svc.is_empty()).then_some(svc);
        self
    }

    /// Override the OpenAPI `servers` array with an explicit base URL.
    pub fn server_url(mut self, server_url: impl Into<String>) -> Self {
        let url = server_url.into();
        self.server_url = (!url.is_empty()).then_some(url);
        self
    }
}

#[derive(Debug, Clone)]
struct TemplateSchemaBinding {
    capability_stem: Option<String>,
    target_def: String,
    is_error_oneof: bool,
}

#[derive(Default)]
struct MergedTemplates {
    openapi_version: String,
    info: Value,
    fallback_servers: Option<Value>,
    paths: Map<String, Value>,
    webhooks: Map<String, Value>,
    parameters: Map<String, Value>,
    headers: Map<String, Value>,
    template_schemas: Map<String, Value>,
}

/// Rewrite all local `#/$defs/<Name>` references in a JSON Schema or OpenAPI AST
/// to `#/components/schemas/<Name>`.
pub fn rewrite_defs_to_component_schemas(value: &mut Value) {
    match value {
        Value::Object(obj) => {
            if let Some(target) = obj
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|r| r.strip_prefix("#/$defs/"))
                .map(str::to_string)
            {
                obj.insert(
                    "$ref".to_string(),
                    Value::String(format!("#/components/schemas/{target}")),
                );
            }
            for (k, v) in obj.iter_mut() {
                if INSTANCE_DATA_KEYWORDS.contains(&k.as_str()) {
                    continue;
                }
                rewrite_defs_to_component_schemas(v);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                rewrite_defs_to_component_schemas(item);
            }
        }
        _ => {}
    }
}

/// Generate a self-contained OpenAPI 3.1.0 specification for the REST service bindings
/// and active capabilities declared in a UCP discovery profile.
pub fn generate_openapi(options: &GenerateOpenApiOptions) -> Result<Value, CodegenError> {
    let parsed = parse_profile_source(&options.profile)?;
    let selected_services = select_rest_services(&options.profile, &parsed.rest_services, options)?;

    let mut merged = load_and_merge_templates(&options.profile, &selected_services, &parsed)?;
    let compiled = compile_types(&GenerateTypesOptions::new().profile(&options.profile))?;
    let bindings = parse_template_bindings(&merged.template_schemas);

    prune_and_bind_route_map(
        &mut merged.paths,
        &bindings,
        &compiled.active_capabilities,
        &compiled.defs,
    );
    prune_and_bind_route_map(
        &mut merged.webhooks,
        &bindings,
        &compiled.active_capabilities,
        &compiled.defs,
    );

    let mut components_schemas = BTreeMap::new();
    for (name, mut schema_val) in compiled.defs {
        rewrite_defs_to_component_schemas(&mut schema_val);
        components_schemas.insert(name, schema_val);
    }
    for param_val in merged.parameters.values_mut() {
        rewrite_defs_to_component_schemas(param_val);
    }
    for header_val in merged.headers.values_mut() {
        rewrite_defs_to_component_schemas(header_val);
    }

    prune_unreachable_components(
        &merged.paths,
        &merged.webhooks,
        &mut merged.parameters,
        &mut merged.headers,
        &mut components_schemas,
    );

    let servers = build_servers_array(&selected_services, options, merged.fallback_servers);
    Ok(assemble_openapi_document(
        merged.openapi_version,
        merged.info,
        servers,
        merged.paths,
        merged.webhooks,
        merged.parameters,
        merged.headers,
        components_schemas,
    ))
}

fn select_rest_services<'a>(
    profile: &str,
    rest_services: &'a [RestServiceBinding],
    options: &GenerateOpenApiOptions,
) -> Result<Vec<&'a RestServiceBinding>, CodegenError> {
    let selected: Vec<&RestServiceBinding> = rest_services
        .iter()
        .filter(|b| {
            options
                .service
                .as_deref()
                .is_none_or(|svc| b.service_name == svc)
        })
        .collect();

    if selected.is_empty() {
        return Err(CodegenError::NoRestServiceBinding {
            profile: profile.to_string(),
        });
    }

    for binding in &selected {
        if binding.schema_url.trim().is_empty() {
            return Err(CodegenError::MissingServiceSchema {
                profile: profile.to_string(),
                service: binding.service_name.clone(),
            });
        }
    }

    Ok(selected)
}

fn load_and_merge_templates(
    profile_source: &str,
    selected_services: &[&RestServiceBinding],
    parsed: &crate::codegen::ParsedProfile,
) -> Result<MergedTemplates, CodegenError> {
    let mut merged = MergedTemplates {
        openapi_version: "3.1.0".to_string(),
        info: json!({ "title": "UCP REST Service", "version": "1.0.0" }),
        ..MergedTemplates::default()
    };

    for (idx, binding) in selected_services.iter().enumerate() {
        let target = resolve_service_schema_target(
            profile_source,
            &binding.schema_url,
            &parsed.capabilities,
        )?;
        let doc = load_schema_auto(&target)?;
        let Some(root_obj) = doc.as_object() else {
            return Err(CodegenError::ResolveError(ResolveError::InvalidSchema {
                message: format!("OpenAPI service template '{target}' must be a JSON object"),
            }));
        };

        if idx == 0 {
            if let Some(ver) = root_obj.get("openapi").and_then(Value::as_str) {
                merged.openapi_version = ver.to_string();
            }
            if let Some(info) = root_obj.get("info") {
                merged.info = info.clone();
            }
            merged.fallback_servers = root_obj.get("servers").cloned();
        }

        extend_object_map(&mut merged.paths, root_obj.get("paths"));
        extend_object_map(&mut merged.webhooks, root_obj.get("webhooks"));

        let Some(components) = root_obj.get("components").and_then(Value::as_object) else {
            continue;
        };
        extend_object_map(&mut merged.parameters, components.get("parameters"));
        extend_object_map(&mut merged.headers, components.get("headers"));
        extend_object_map(&mut merged.template_schemas, components.get("schemas"));
    }

    Ok(merged)
}

fn extend_object_map(dst: &mut Map<String, Value>, src: Option<&Value>) {
    let Some(obj) = src.and_then(Value::as_object) else {
        return;
    };
    for (k, v) in obj {
        dst.entry(k.clone()).or_insert_with(|| v.clone());
    }
}

fn parse_template_bindings(
    template_schemas: &Map<String, Value>,
) -> BTreeMap<String, TemplateSchemaBinding> {
    let mut bindings = BTreeMap::new();
    for (key, val) in template_schemas {
        let Some(binding) = parse_single_template_binding(val) else {
            continue;
        };
        bindings.insert(key.clone(), binding);
    }
    bindings
}

fn parse_single_template_binding(val: &Value) -> Option<TemplateSchemaBinding> {
    let obj = val.as_object()?;
    if let Some(ref_str) = obj.get("$ref").and_then(Value::as_str) {
        return parse_ref_binding(ref_str, false);
    }

    let one_of = obj.get("oneOf").and_then(Value::as_array)?;
    let mut primary_ref = None;
    let mut has_error_ref = false;
    for branch in one_of {
        let branch_ref = branch.get("$ref").and_then(Value::as_str)?;
        if extract_file_stem(branch_ref).as_deref() == Some("error_response") {
            has_error_ref = true;
        } else {
            primary_ref = Some(branch_ref);
        }
    }

    if !has_error_ref {
        return None;
    }
    parse_ref_binding(primary_ref?, true)
}

fn parse_ref_binding(ref_str: &str, is_error_oneof: bool) -> Option<TemplateSchemaBinding> {
    let stem = extract_file_stem(ref_str)?;
    let has_def_fragment = ref_str.contains("#/$defs/") || ref_str.contains("#/definitions/");
    if stem == "ucp" && !has_def_fragment {
        return Some(TemplateSchemaBinding {
            capability_stem: None,
            target_def: "UcpBase".to_string(),
            is_error_oneof,
        });
    }
    if stem == "error_response" && !has_def_fragment {
        return Some(TemplateSchemaBinding {
            capability_stem: None,
            target_def: "ErrorResponse".to_string(),
            is_error_oneof,
        });
    }
    Some(TemplateSchemaBinding {
        capability_stem: Some(stem),
        target_def: ref_to_def_name(ref_str, None),
        is_error_oneof,
    })
}

fn extract_file_stem(ref_str: &str) -> Option<String> {
    let file_part = ref_str.split('#').next().unwrap_or("");
    if file_part.is_empty() {
        return None;
    }
    Path::new(file_part)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string)
}

fn prune_and_bind_route_map(
    route_map: &mut Map<String, Value>,
    bindings: &BTreeMap<String, TemplateSchemaBinding>,
    active_capabilities: &BTreeSet<String>,
    compiled_defs: &BTreeMap<String, Value>,
) {
    route_map.retain(|_, path_val| {
        let Some(path_item) = path_val.as_object_mut() else {
            return false;
        };

        path_item.retain(|key, op_val| {
            if !HTTP_METHODS.contains(&key.as_str()) {
                return true;
            }
            if !is_operation_active(op_val, bindings, active_capabilities, compiled_defs) {
                return false;
            }
            bind_operation_schemas(op_val, bindings, compiled_defs);
            true
        });

        HTTP_METHODS.iter().any(|&m| path_item.contains_key(m))
    });
}

fn is_operation_active(
    op_val: &Value,
    bindings: &BTreeMap<String, TemplateSchemaBinding>,
    active_capabilities: &BTreeSet<String>,
    compiled_defs: &BTreeMap<String, Value>,
) -> bool {
    let mut referenced_keys = BTreeSet::new();
    collect_component_refs(op_val, "#/components/schemas/", &mut referenced_keys);
    for key in referenced_keys {
        let Some(binding) = bindings.get(&key) else {
            if !compiled_defs.contains_key(&key) {
                return false;
            }
            continue;
        };
        if let Some(cap_stem) = &binding.capability_stem {
            if !active_capabilities.contains(cap_stem) {
                return false;
            }
        }
        if !compiled_defs.contains_key(&binding.target_def) {
            return false;
        }
    }
    true
}

fn bind_operation_schemas(
    op_val: &mut Value,
    bindings: &BTreeMap<String, TemplateSchemaBinding>,
    compiled_defs: &BTreeMap<String, Value>,
) {
    let Some(op_obj) = op_val.as_object_mut() else {
        return;
    };
    let op_id = op_obj
        .get("operationId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let req_suffix = directional_request_suffix(&op_id);

    for (key, child) in op_obj.iter_mut() {
        let suffix = (key == "requestBody").then_some(req_suffix).flatten();
        let in_responses = key == "responses";
        rewrite_operation_subtree(child, bindings, compiled_defs, suffix, in_responses);
    }
}

fn directional_request_suffix(operation_id: &str) -> Option<&'static str> {
    if operation_id.starts_with("create_") {
        Some("CreateRequest")
    } else if operation_id.starts_with("update_") {
        Some("UpdateRequest")
    } else if operation_id.starts_with("complete_") {
        Some("CompleteRequest")
    } else {
        None
    }
}

fn rewrite_operation_subtree(
    val: &mut Value,
    bindings: &BTreeMap<String, TemplateSchemaBinding>,
    compiled_defs: &BTreeMap<String, Value>,
    req_suffix: Option<&str>,
    in_responses: bool,
) {
    match val {
        Value::Object(obj) => {
            if let Some(schema_key) = obj
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|r| r.strip_prefix("#/components/schemas/"))
                .map(str::to_string)
            {
                apply_schema_binding_to_ref_obj(
                    obj,
                    &schema_key,
                    bindings,
                    compiled_defs,
                    req_suffix,
                    in_responses,
                );
                return;
            }
            for child in obj.values_mut() {
                rewrite_operation_subtree(child, bindings, compiled_defs, req_suffix, in_responses);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                rewrite_operation_subtree(item, bindings, compiled_defs, req_suffix, in_responses);
            }
        }
        _ => {}
    }
}

fn apply_schema_binding_to_ref_obj(
    obj: &mut Map<String, Value>,
    schema_key: &str,
    bindings: &BTreeMap<String, TemplateSchemaBinding>,
    compiled_defs: &BTreeMap<String, Value>,
    req_suffix: Option<&str>,
    in_responses: bool,
) {
    let Some(binding) = bindings.get(schema_key) else {
        return;
    };

    if in_responses && binding.is_error_oneof {
        obj.remove("$ref");
        obj.insert(
            "oneOf".to_string(),
            json!([
                { "$ref": format!("#/components/schemas/{}", binding.target_def) },
                { "$ref": "#/components/schemas/ErrorResponse" }
            ]),
        );
        return;
    }

    let target = resolve_directional_target(&binding.target_def, req_suffix, compiled_defs);
    obj.insert(
        "$ref".to_string(),
        Value::String(format!("#/components/schemas/{target}")),
    );
}

fn resolve_directional_target<'a>(
    base_target: &'a str,
    req_suffix: Option<&str>,
    compiled_defs: &'a BTreeMap<String, Value>,
) -> &'a str {
    let Some(suffix) = req_suffix else {
        return base_target;
    };
    let candidate = format!("{base_target}{suffix}");
    let Some((matched_key, _)) = compiled_defs.get_key_value(&candidate) else {
        return base_target;
    };
    matched_key.as_str()
}

fn prune_unreachable_components(
    paths: &Map<String, Value>,
    webhooks: &Map<String, Value>,
    parameters: &mut Map<String, Value>,
    headers: &mut Map<String, Value>,
    schemas: &mut BTreeMap<String, Value>,
) {
    let mut reachable_params = BTreeSet::new();
    let mut reachable_headers = BTreeSet::new();
    let mut reachable_schemas = BTreeSet::new();

    for val in paths.values().chain(webhooks.values()) {
        collect_component_refs(val, "#/components/parameters/", &mut reachable_params);
        collect_component_refs(val, "#/components/headers/", &mut reachable_headers);
        collect_component_refs(val, "#/components/schemas/", &mut reachable_schemas);
    }

    parameters.retain(|k, _| reachable_params.contains(k));
    for param_val in parameters.values() {
        collect_component_refs(param_val, "#/components/headers/", &mut reachable_headers);
        collect_component_refs(param_val, "#/components/schemas/", &mut reachable_schemas);
    }

    headers.retain(|k, _| reachable_headers.contains(k));
    for header_val in headers.values() {
        collect_component_refs(header_val, "#/components/schemas/", &mut reachable_schemas);
    }

    let mut queue: VecDeque<String> = reachable_schemas.iter().cloned().collect();
    while let Some(schema_name) = queue.pop_front() {
        let Some(schema_val) = schemas.get(&schema_name) else {
            continue;
        };
        let mut child_refs = BTreeSet::new();
        collect_component_refs(schema_val, "#/components/schemas/", &mut child_refs);
        for child in child_refs {
            if reachable_schemas.insert(child.clone()) {
                queue.push_back(child);
            }
        }
    }

    schemas.retain(|k, _| reachable_schemas.contains(k));
}

fn collect_component_refs(val: &Value, prefix: &str, out: &mut BTreeSet<String>) {
    match val {
        Value::Object(obj) => {
            if let Some(target) = obj
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|r| r.strip_prefix(prefix))
            {
                out.insert(target.to_string());
            }
            for (k, v) in obj {
                if INSTANCE_DATA_KEYWORDS.contains(&k.as_str()) {
                    continue;
                }
                collect_component_refs(v, prefix, out);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                collect_component_refs(item, prefix, out);
            }
        }
        _ => {}
    }
}

fn build_servers_array(
    selected_services: &[&RestServiceBinding],
    options: &GenerateOpenApiOptions,
    fallback_servers: Option<Value>,
) -> Value {
    if let Some(url) = options.server_url.as_deref().filter(|s| !s.is_empty()) {
        return json!([{ "url": url }]);
    }

    let mut seen = BTreeSet::new();
    let mut endpoints = Vec::new();
    for binding in selected_services {
        let Some(ep) = binding.endpoint.as_deref().map(str::trim) else {
            continue;
        };
        if ep.is_empty() || !seen.insert(ep.to_string()) {
            continue;
        }
        endpoints.push(json!({ "url": ep }));
    }

    if !endpoints.is_empty() {
        return Value::Array(endpoints);
    }
    fallback_servers.unwrap_or_else(|| json!([]))
}

#[allow(clippy::too_many_arguments)]
fn assemble_openapi_document(
    openapi_version: String,
    info: Value,
    servers: Value,
    paths: Map<String, Value>,
    webhooks: Map<String, Value>,
    parameters: Map<String, Value>,
    headers: Map<String, Value>,
    schemas: BTreeMap<String, Value>,
) -> Value {
    let mut root = Map::new();
    root.insert("openapi".to_string(), Value::String(openapi_version));
    root.insert(
        "jsonSchemaDialect".to_string(),
        Value::String(OAS_3_1_DIALECT.to_string()),
    );
    root.insert("info".to_string(), info);
    root.insert("servers".to_string(), servers);
    root.insert("paths".to_string(), Value::Object(paths));
    if !webhooks.is_empty() {
        root.insert("webhooks".to_string(), Value::Object(webhooks));
    }

    let mut components = Map::new();
    if !parameters.is_empty() {
        components.insert("parameters".to_string(), Value::Object(parameters));
    }
    if !headers.is_empty() {
        components.insert("headers".to_string(), Value::Object(headers));
    }
    let schemas_map: Map<String, Value> = schemas.into_iter().collect();
    components.insert("schemas".to_string(), Value::Object(schemas_map));
    root.insert("components".to_string(), Value::Object(components));

    Value::Object(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_defs_to_component_schemas_rewrites_schema_refs_and_skips_instance_keywords() {
        let mut doc = json!({
            "type": "object",
            "properties": {
                "item": { "$ref": "#/$defs/LineItem" },
                "literal_const": {
                    "type": "object",
                    "const": { "$ref": "#/$defs/DoNotTouch" },
                    "default": { "$ref": "#/$defs/DoNotTouchEither" }
                }
            },
            "anyOf": [
                { "$ref": "#/$defs/ShippingMethod" },
                { "$ref": "#/$defs/FulfillmentMethodBase" }
            ]
        });

        rewrite_defs_to_component_schemas(&mut doc);

        assert_eq!(
            doc["properties"]["item"]["$ref"],
            "#/components/schemas/LineItem"
        );
        assert_eq!(
            doc["anyOf"][0]["$ref"],
            "#/components/schemas/ShippingMethod"
        );
        assert_eq!(
            doc["anyOf"][1]["$ref"],
            "#/components/schemas/FulfillmentMethodBase"
        );
        assert_eq!(
            doc["properties"]["literal_const"]["const"]["$ref"],
            "#/$defs/DoNotTouch"
        );
        assert_eq!(
            doc["properties"]["literal_const"]["default"]["$ref"],
            "#/$defs/DoNotTouchEither"
        );
    }

    #[test]
    fn parse_single_template_binding_maps_direct_and_oneof_template_entries() {
        let checkout = parse_single_template_binding(&json!({
            "$ref": "../../schemas/shopping/checkout.json"
        }))
        .unwrap();
        assert_eq!(checkout.capability_stem.as_deref(), Some("checkout"));
        assert_eq!(checkout.target_def, "Checkout");
        assert!(!checkout.is_error_oneof);

        let checkout_resp = parse_single_template_binding(&json!({
            "oneOf": [
                { "$ref": "../../schemas/shopping/checkout.json" },
                { "$ref": "../../schemas/common/types/error_response.json" }
            ]
        }))
        .unwrap();
        assert_eq!(checkout_resp.capability_stem.as_deref(), Some("checkout"));
        assert_eq!(checkout_resp.target_def, "Checkout");
        assert!(checkout_resp.is_error_oneof);

        let get_product_resp = parse_single_template_binding(&json!({
            "oneOf": [
                { "$ref": "../../schemas/shopping/catalog_lookup.json#/$defs/get_product_response" },
                { "$ref": "../../schemas/common/types/error_response.json" }
            ]
        }))
        .unwrap();
        assert_eq!(
            get_product_resp.capability_stem.as_deref(),
            Some("catalog_lookup")
        );
        assert_eq!(get_product_resp.target_def, "CatalogGetProductResponse");
        assert!(get_product_resp.is_error_oneof);

        let ucp = parse_single_template_binding(&json!({
            "$ref": "../../schemas/ucp.json"
        }))
        .unwrap();
        assert!(ucp.capability_stem.is_none());
        assert_eq!(ucp.target_def, "UcpBase");
        assert!(!ucp.is_error_oneof);
    }
}
