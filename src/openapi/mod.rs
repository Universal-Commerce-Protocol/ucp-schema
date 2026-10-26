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
    default_servers: Vec<Value>,
    paths: Map<String, Value>,
    path_servers: BTreeMap<String, Vec<Value>>,
    webhooks: Map<String, Value>,
    parameters: Map<String, Value>,
    headers: Map<String, Value>,
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
    let compiled = compile_types(&GenerateTypesOptions::new().profile(&options.profile))?;

    let mut merged = load_bind_and_merge_templates(
        &options.profile,
        &selected_services,
        &parsed,
        &compiled,
        options,
    )?;

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

    let servers = resolve_root_and_path_servers(
        &mut merged.paths,
        &merged.path_servers,
        merged.default_servers,
        options,
    );
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

fn load_bind_and_merge_templates(
    profile_source: &str,
    selected_services: &[&RestServiceBinding],
    parsed: &crate::codegen::ParsedProfile,
    compiled: &crate::codegen::CompiledTypes,
    options: &GenerateOpenApiOptions,
) -> Result<MergedTemplates, CodegenError> {
    let mut merged = MergedTemplates {
        openapi_version: "3.1.0".to_string(),
        info: json!({ "title": "UCP REST Service", "version": "1.0.0" }),
        ..MergedTemplates::default()
    };
    let mut first_active_service: Option<String> = None;
    let mut fallback_metadata: Option<(String, Value, Vec<Value>)> = None;
    let mut path_owners: BTreeMap<String, String> = BTreeMap::new();

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

        let template_ver = root_obj
            .get("openapi")
            .and_then(Value::as_str)
            .unwrap_or("3.1.0")
            .to_string();
        let service_info = build_service_info(binding, root_obj);
        let binding_servers = resolve_binding_servers(binding, root_obj, options);
        if idx == 0 {
            fallback_metadata = Some((
                template_ver.clone(),
                service_info.clone(),
                binding_servers.clone(),
            ));
        }

        let empty_map = Map::new();
        let components = root_obj.get("components").and_then(Value::as_object);
        let template_schemas = components
            .and_then(|c| c.get("schemas"))
            .and_then(Value::as_object)
            .unwrap_or(&empty_map);
        let schema_bindings = parse_template_bindings(template_schemas);

        let mut svc_paths = root_obj
            .get("paths")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut svc_webhooks = root_obj
            .get("webhooks")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        prune_and_bind_route_map(
            &mut svc_paths,
            &schema_bindings,
            &compiled.active_capabilities,
            &compiled.defs,
        );
        prune_and_bind_route_map(
            &mut svc_webhooks,
            &schema_bindings,
            &compiled.active_capabilities,
            &compiled.defs,
        );

        if svc_paths.is_empty() && svc_webhooks.is_empty() {
            continue;
        }

        if first_active_service.is_none() {
            merged.openapi_version = template_ver;
            merged.info = service_info;
            merged.default_servers = binding_servers.clone();
            first_active_service = Some(binding.service_name.clone());
        } else if first_active_service.as_deref() == Some(binding.service_name.as_str()) {
            append_unique_servers(&mut merged.default_servers, &binding_servers);
        }

        for (path_key, path_item) in svc_paths {
            let Some(existing_owner) = path_owners.get(&path_key) else {
                path_owners.insert(path_key.clone(), binding.service_name.clone());
                merged
                    .path_servers
                    .insert(path_key.clone(), binding_servers.clone());
                merged.paths.insert(path_key, path_item);
                continue;
            };
            if existing_owner != &binding.service_name {
                return Err(CodegenError::ConflictingServicePath {
                    path: path_key,
                    first_service: existing_owner.clone(),
                    second_service: binding.service_name.clone(),
                });
            }
            let entry = merged.path_servers.entry(path_key).or_default();
            append_unique_servers(entry, &binding_servers);
        }

        for (hook_key, hook_item) in svc_webhooks {
            merged.webhooks.entry(hook_key).or_insert(hook_item);
        }

        if let Some(comp) = components {
            extend_object_map(&mut merged.parameters, comp.get("parameters"));
            extend_object_map(&mut merged.headers, comp.get("headers"));
        }
    }

    if let (None, Some((ver, info, servers))) = (&first_active_service, fallback_metadata) {
        merged.openapi_version = ver;
        merged.info = info;
        merged.default_servers = servers;
    }

    Ok(merged)
}

fn build_service_info(binding: &RestServiceBinding, root_obj: &Map<String, Value>) -> Value {
    let default_version = Some(binding.version.trim())
        .filter(|v| !v.is_empty())
        .unwrap_or("1.0.0");
    let mut info = Map::new();
    info.insert(
        "title".to_string(),
        Value::String("UCP REST Service".to_string()),
    );
    info.insert(
        "version".to_string(),
        Value::String(default_version.to_string()),
    );
    let Some(src_info) = root_obj.get("info").and_then(Value::as_object) else {
        return Value::Object(info);
    };
    for (k, v) in src_info {
        if k == "version" && v.as_str().is_some_and(|s| s.trim().is_empty()) {
            continue;
        }
        info.insert(k.clone(), v.clone());
    }
    Value::Object(info)
}

fn resolve_binding_servers(
    binding: &RestServiceBinding,
    root_obj: &Map<String, Value>,
    options: &GenerateOpenApiOptions,
) -> Vec<Value> {
    if let Some(url) = options
        .server_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return vec![json!({ "url": url })];
    }
    if let Some(ep) = binding
        .endpoint
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return vec![json!({ "url": ep })];
    }
    root_obj
        .get("servers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn append_unique_servers(dst: &mut Vec<Value>, src: &[Value]) {
    for item in src {
        if !dst.contains(item) {
            dst.push(item.clone());
        }
    }
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

fn resolve_root_and_path_servers(
    paths: &mut Map<String, Value>,
    path_servers: &BTreeMap<String, Vec<Value>>,
    default_servers: Vec<Value>,
    options: &GenerateOpenApiOptions,
) -> Option<Value> {
    if let Some(url) = options
        .server_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        for path_val in paths.values_mut() {
            if let Some(item_obj) = path_val.as_object_mut() {
                item_obj.remove("servers");
            }
        }
        return Some(json!([{ "url": url }]));
    }

    let Some(first_key) = paths.keys().next() else {
        return Some(Value::Array(default_servers));
    };
    let first_servers = path_servers.get(first_key).cloned().unwrap_or_default();
    let uniform = paths
        .keys()
        .all(|k| path_servers.get(k).is_some_and(|s| s == &first_servers));
    if uniform {
        return Some(Value::Array(first_servers));
    }

    for (path_key, path_val) in paths.iter_mut() {
        let Some(servers) = path_servers.get(path_key).filter(|s| !s.is_empty()) else {
            continue;
        };
        let Some(item_obj) = path_val.as_object_mut() else {
            continue;
        };
        let existing_entries: Vec<(String, Value)> = std::mem::take(item_obj)
            .into_iter()
            .filter(|(k, _)| k != "servers")
            .collect();
        item_obj.insert("servers".to_string(), Value::Array(servers.clone()));
        item_obj.extend(existing_entries);
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn assemble_openapi_document(
    openapi_version: String,
    info: Value,
    servers: Option<Value>,
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
    if let Some(servers_val) = servers {
        root.insert("servers".to_string(), servers_val);
    }
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

    #[test]
    fn build_service_info_ignores_empty_template_version_and_resolve_servers_adapts() {
        let binding = RestServiceBinding {
            service_name: "dev.ucp.shopping".to_string(),
            version: "2026-08-25".to_string(),
            endpoint: Some("https://shop.example.com".to_string()),
            schema_url: "rest.openapi.json".to_string(),
        };
        let template_obj = json!({
            "info": {
                "title": "Custom Shopping",
                "version": "   "
            }
        });
        let info = build_service_info(&binding, template_obj.as_object().unwrap());
        assert_eq!(info["title"], "Custom Shopping");
        assert_eq!(info["version"], "2026-08-25");

        let mut paths = Map::from_iter([
            ("/checkout".to_string(), json!({ "post": {} })),
            ("/ask".to_string(), json!({ "post": {} })),
        ]);
        let path_servers = BTreeMap::from([
            (
                "/checkout".to_string(),
                vec![json!({ "url": "https://shop.example.com" })],
            ),
            (
                "/ask".to_string(),
                vec![json!({ "url": "https://common.example.com" })],
            ),
        ]);
        let opts = GenerateOpenApiOptions::new("profile.json");
        let root_servers = resolve_root_and_path_servers(&mut paths, &path_servers, vec![], &opts);
        assert!(root_servers.is_none());
        assert_eq!(
            paths["/checkout"]["servers"],
            json!([{ "url": "https://shop.example.com" }])
        );
        assert_eq!(
            paths["/ask"]["servers"],
            json!([{ "url": "https://common.example.com" }])
        );

        let override_opts =
            GenerateOpenApiOptions::new("profile.json").server_url("https://gateway.example.com");
        let overridden =
            resolve_root_and_path_servers(&mut paths, &path_servers, vec![], &override_opts);
        assert_eq!(
            overridden,
            Some(json!([{ "url": "https://gateway.example.com" }]))
        );
        assert!(paths["/checkout"].get("servers").is_none());
        assert!(paths["/ask"].get("servers").is_none());
    }
}
