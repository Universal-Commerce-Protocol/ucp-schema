//! Route projection, standard protocol headers, and RFC 9421 security bindings.
//!
//! Generates OpenAPI 3.1 `paths` operations for capability resources,
//! binding standard UCP protocol headers (`UCP-Agent`, `Idempotency-Key`, `Signature`, `Signature-Input`, `Signature-Agent`)
//! and authentication security schemes (`HttpSignatureAuth`, `BearerAuth`).

use serde_json::{json, Value};
use std::collections::BTreeMap;

use crate::openapi::discriminator::to_pascal_case;
use crate::openapi::types::{
    MediaType, Operation, Parameter, ParameterIn, ParameterOrRef, PathItem, RequestBody,
    RequestBodyOrRef, ResponseItem, ResponseItemOrRef, SecurityScheme,
};

/// Convert a singular resource name in snake_case or PascalCase to a plural path segment.
pub fn pluralize_path_segment(singular: &str) -> String {
    let lower = singular.to_lowercase();
    if lower.ends_with('y')
        && !lower.ends_with("ey")
        && !lower.ends_with("ay")
        && !lower.ends_with("oy")
    {
        format!("{}ies", &lower[..lower.len() - 1])
    } else if lower.ends_with('s') || lower.ends_with("ch") || lower.ends_with("sh") {
        format!("{}es", lower)
    } else {
        format!("{}s", lower)
    }
}

/// Convert a PascalCase name to camelCase (e.g. `Checkout` -> `checkout`, `CheckoutCreateRequest` -> `checkoutCreateRequest`).
pub fn to_camel_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_lowercase().collect::<String>() + chars.as_str(),
    }
}

fn header_param(name: &str, desc: &str, required: bool, schema: Value) -> Parameter {
    Parameter {
        name: name.to_string(),
        in_: ParameterIn::Header,
        description: Some(desc.to_string()),
        required: Some(required),
        schema: Some(schema),
    }
}

fn ref_param(name: &str) -> ParameterOrRef {
    ParameterOrRef::Ref {
        reference: format!("#/components/parameters/{}", name),
    }
}

/// Construct standard UCP protocol parameters for `components.parameters`.
pub fn build_standard_parameters() -> BTreeMap<String, Parameter> {
    let definitions = [
        (
            "UcpAgent",
            "UCP-Agent",
            "Client or platform identification string (e.g., platform/1.0.0; agent/2.1.0).",
            true,
            json!({ "type": "string" }),
        ),
        (
            "IdempotencyKey",
            "Idempotency-Key",
            "UUID v4 idempotency token to safely retry mutating operations without side-effects.",
            true,
            json!({ "type": "string", "format": "uuid" }),
        ),
        (
            "Signature",
            "Signature",
            "RFC 9421 HTTP Message Signature value.",
            false,
            json!({ "type": "string" }),
        ),
        (
            "SignatureInput",
            "Signature-Input",
            "RFC 9421 Signature input metadata describing signed components.",
            false,
            json!({ "type": "string" }),
        ),
        (
            "SignatureAgent",
            "Signature-Agent",
            "Web Bot Auth key lookup metadata (type=jwks_uri, type=cimd, or type=directory).",
            false,
            json!({ "type": "string" }),
        ),
    ];

    definitions
        .into_iter()
        .map(|(key, name, desc, req, schema)| {
            (key.to_string(), header_param(name, desc, req, schema))
        })
        .collect()
}

/// Construct standard UCP security schemes for `components.securitySchemes`.
pub fn build_standard_security_schemes() -> BTreeMap<String, SecurityScheme> {
    BTreeMap::from([
        (
            "HttpSignatureAuth".to_string(),
            SecurityScheme {
                type_: "http".to_string(),
                scheme: Some("signature".to_string()),
                bearer_format: None,
                description: Some(
                    "RFC 9421 HTTP Message Signatures permissionless authentication.".to_string(),
                ),
            },
        ),
        (
            "BearerAuth".to_string(),
            SecurityScheme {
                type_: "http".to_string(),
                scheme: Some("bearer".to_string()),
                bearer_format: Some("JWT".to_string()),
                description: Some(
                    "Bearer token authentication for authenticated buyer sessions.".to_string(),
                ),
            },
        ),
    ])
}

/// Default security requirements applied to operations.
pub fn default_operation_security() -> Vec<BTreeMap<String, Vec<String>>> {
    vec![
        BTreeMap::from([("HttpSignatureAuth".to_string(), Vec::new())]),
        BTreeMap::from([("BearerAuth".to_string(), Vec::new())]),
    ]
}

/// Standard mutating header parameter references (POST, PATCH, PUT).
pub fn mutating_protocol_parameters() -> Vec<ParameterOrRef> {
    [
        "UcpAgent",
        "IdempotencyKey",
        "Signature",
        "SignatureInput",
        "SignatureAgent",
    ]
    .into_iter()
    .map(ref_param)
    .collect()
}

/// Standard non-mutating header parameter references (GET, DELETE).
pub fn non_mutating_protocol_parameters() -> Vec<ParameterOrRef> {
    ["UcpAgent", "Signature", "SignatureInput", "SignatureAgent"]
        .into_iter()
        .map(ref_param)
        .collect()
}

/// Standard ErrorResponse schema definition.
pub fn default_error_response_schema() -> Value {
    json!({
        "title": "Error Response",
        "type": "object",
        "required": ["code", "message"],
        "properties": {
            "code": {
                "type": "string",
                "description": "Machine-readable error code."
            },
            "message": {
                "type": "string",
                "description": "Human-readable error description."
            },
            "errors": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["path", "message"],
                    "properties": {
                        "path": { "type": "string" },
                        "message": { "type": "string" },
                        "code": { "type": "string" }
                    }
                },
                "description": "Optional list of detailed field-level validation errors."
            }
        }
    })
}

/// Construct a JSON media type map for request bodies and responses: `{"application/json": MediaType { schema: ... }}`.
pub fn build_json_media_type(schema_ref: &str) -> BTreeMap<String, MediaType> {
    let mut content = BTreeMap::new();
    content.insert(
        "application/json".to_string(),
        MediaType {
            schema: Some(json!({ "$ref": schema_ref })),
        },
    );
    content
}

/// Construct a status code to ResponseItemOrRef pair.
pub fn build_json_response(
    status: &str,
    description: impl Into<String>,
    schema_ref: Option<&str>,
) -> (String, ResponseItemOrRef) {
    (
        status.to_string(),
        ResponseItemOrRef::Item(ResponseItem {
            description: description.into(),
            content: schema_ref.map(build_json_media_type),
            headers: None,
        }),
    )
}

/// Construct an application/json RequestBody.
pub fn build_request_body(
    description: impl Into<String>,
    schema_ref: &str,
    required: bool,
) -> RequestBodyOrRef {
    RequestBodyOrRef::Item(RequestBody {
        description: Some(description.into()),
        content: build_json_media_type(schema_ref),
        required: Some(required),
    })
}

/// Build an OpenAPI Operation struct with uniform attributes.
#[allow(clippy::too_many_arguments)]
pub fn build_operation(
    operation_id: impl Into<String>,
    summary: impl Into<String>,
    description: impl Into<String>,
    tags: Vec<String>,
    params: Option<Vec<ParameterOrRef>>,
    body: Option<RequestBodyOrRef>,
    responses: BTreeMap<String, ResponseItemOrRef>,
    security: Option<Vec<BTreeMap<String, Vec<String>>>>,
) -> Operation {
    Operation {
        operation_id: Some(operation_id.into()),
        summary: Some(summary.into()),
        description: Some(description.into()),
        tags: if tags.is_empty() { None } else { Some(tags) },
        parameters: params,
        request_body: body,
        responses,
        security,
    }
}

/// Construct a standardized set of 2xx, 400, 401, and optional 404 responses.
pub fn crud_responses(
    success_status: &str,
    success_desc: impl Into<String>,
    schema_ref: Option<&str>,
    bad_request_desc: Option<&str>,
    not_found_desc: Option<&str>,
) -> BTreeMap<String, ResponseItemOrRef> {
    let mut responses = BTreeMap::new();
    let (status, resp) = build_json_response(success_status, success_desc, schema_ref);
    responses.insert(status, resp);

    if let Some(br) = bad_request_desc {
        let (status, resp) =
            build_json_response("400", br, Some("#/components/schemas/ErrorResponse"));
        responses.insert(status, resp);
    }

    let (status, resp) = build_json_response("401", "Unauthorized request.", None);
    responses.insert(status, resp);

    if let Some(nf) = not_found_desc {
        let (status, resp) = build_json_response("404", nf, None);
        responses.insert(status, resp);
    }

    responses
}

/// Project REST operations for a capability resource into OpenAPI `paths`.
///
/// Handles:
/// - `POST /<resources>`: Create operation
/// - `GET /<resources>/{id}`: Get operation
/// - `PUT /<resources>/{id}`: Update operation
/// - Lifecycle sub-resource actions (e.g. `/complete`, `/cancel`)
pub fn project_resource_operations(
    resource_name: &str,
    raw_schema: Option<&Value>,
    available_schemas: &BTreeMap<String, Value>,
    paths: &mut BTreeMap<String, PathItem>,
) {
    let (collection_path, item_path) = if let Some(custom_path) = raw_schema
        .and_then(|s| s.get("x-ucp-path"))
        .and_then(|p| p.as_str())
    {
        let clean = if custom_path.starts_with('/') {
            custom_path.to_string()
        } else {
            format!("/{}", custom_path)
        };
        (clean.clone(), format!("{}/{{id}}", clean))
    } else if resource_name == "Profile" {
        (
            "/.well-known/ucp".to_string(),
            "/.well-known/ucp".to_string(),
        )
    } else {
        let plural = pluralize_path_segment(resource_name);
        (format!("/{}", plural), format!("/{}/{{id}}", plural))
    };

    let tag = if resource_name == "Profile" {
        "Discovery".to_string()
    } else {
        resource_name.to_string()
    };

    let response_schema_name = resource_name.to_string();
    let resp_ref = format!("#/components/schemas/{}", response_schema_name);

    // Singleton document resources (such as Profile at /.well-known/ucp) only expose GET on the root path
    if resource_name == "Profile" {
        if available_schemas.contains_key(&response_schema_name) {
            let responses = BTreeMap::from([build_json_response(
                "200",
                "Merchant discovery profile and capabilities.",
                Some(&resp_ref),
            )]);

            let get_profile_op = build_operation(
                "getMerchantProfile",
                "Get Merchant Profile",
                "Fetch the merchant discovery profile and advertised capabilities.",
                vec![tag],
                Some(vec![]),
                None,
                responses,
                None,
            );

            paths.entry(collection_path).or_default().get = Some(get_profile_op);
        }
        return;
    }

    let create_schema_name = format!("{}CreateRequest", resource_name);
    let update_schema_name = format!("{}UpdateRequest", resource_name);
    let has_create = available_schemas.contains_key(&create_schema_name);
    let has_update = available_schemas.contains_key(&update_schema_name);
    let has_response = available_schemas.contains_key(&response_schema_name);

    // 1. Collection Path: POST (Create)
    if has_create {
        let create_ref = format!("#/components/schemas/{}", create_schema_name);
        let responses = crud_responses(
            "201",
            format!("{} created successfully.", resource_name),
            Some(&resp_ref),
            Some(&format!(
                "Invalid {} create request payload.",
                resource_name
            )),
            None,
        );

        let post_op = build_operation(
            format!("create{}", resource_name),
            format!("Create a new {} resource", resource_name),
            format!("Create a new {} session or resource.", resource_name),
            vec![tag.clone()],
            Some(mutating_protocol_parameters()),
            Some(build_request_body(
                format!("Payload to create a new {}.", resource_name),
                &create_ref,
                true,
            )),
            responses,
            Some(default_operation_security()),
        );

        paths.entry(collection_path).or_default().post = Some(post_op);
    }

    // 2. Item Path: GET & PUT
    let path_param_id = ParameterOrRef::Item(Parameter {
        name: "id".to_string(),
        in_: ParameterIn::Path,
        description: Some(format!("Unique identifier of the {}.", resource_name)),
        required: Some(true),
        schema: Some(json!({ "type": "string" })),
    });

    if has_response {
        let mut get_params = vec![path_param_id.clone()];
        get_params.extend(non_mutating_protocol_parameters());

        let responses = crud_responses(
            "200",
            format!("{} details.", resource_name),
            Some(&resp_ref),
            None,
            Some(&format!("{} not found.", resource_name)),
        );

        let get_op = build_operation(
            format!("get{}", resource_name),
            format!("Get {} by ID", resource_name),
            format!("Retrieve an existing {} by its identifier.", resource_name),
            vec![tag.clone()],
            Some(get_params),
            None,
            responses,
            Some(default_operation_security()),
        );

        paths.entry(item_path.clone()).or_default().get = Some(get_op);
    }

    if has_update {
        let update_ref = format!("#/components/schemas/{}", update_schema_name);
        let mut put_params = vec![path_param_id.clone()];
        put_params.extend(mutating_protocol_parameters());

        let responses = crud_responses(
            "200",
            format!("{} updated successfully.", resource_name),
            Some(&resp_ref),
            Some(&format!("Invalid {} update payload.", resource_name)),
            Some(&format!("{} not found.", resource_name)),
        );

        let put_op = build_operation(
            format!("update{}", resource_name),
            format!("Update existing {}", resource_name),
            format!("Update an active {} session or resource.", resource_name),
            vec![tag.clone()],
            Some(put_params),
            Some(build_request_body(
                format!("Payload to update {}.", resource_name),
                &update_ref,
                true,
            )),
            responses,
            Some(default_operation_security()),
        );

        paths.entry(item_path.clone()).or_default().put = Some(put_op);
    }

    // 3. Generic Lifecycle Sub-Resource Actions
    project_lifecycle_actions(
        resource_name,
        &item_path,
        &path_param_id,
        raw_schema,
        available_schemas,
        paths,
    );
}

/// Project lifecycle sub-resource actions (complete, cancel) generically.
fn project_lifecycle_actions(
    resource_name: &str,
    item_path: &str,
    path_param_id: &ParameterOrRef,
    raw_schema: Option<&Value>,
    available_schemas: &BTreeMap<String, Value>,
    paths: &mut BTreeMap<String, PathItem>,
) {
    let tag = resource_name;
    let resp_ref = format!("#/components/schemas/{}", resource_name);

    let complete_schema_name = format!("{}CompleteRequest", resource_name);
    let cancel_schema_name = format!("{}CancelRequest", resource_name);

    let explicit_lifecycle = raw_schema
        .and_then(|s| s.get("x-ucp-lifecycle"))
        .and_then(|v| v.as_array());

    let has_complete = if let Some(actions) = explicit_lifecycle {
        actions.iter().any(|a| a.as_str() == Some("complete"))
    } else {
        available_schemas.contains_key(&complete_schema_name)
    };

    let has_cancel = if let Some(actions) = explicit_lifecycle {
        actions.iter().any(|a| a.as_str() == Some("cancel"))
    } else {
        available_schemas.contains_key(&cancel_schema_name)
    };

    // Complete lifecycle action: POST /{collection}/{id}/complete
    if has_complete {
        let complete_path = format!("{}/complete", item_path);
        let mut complete_params = vec![path_param_id.clone()];
        complete_params.extend(mutating_protocol_parameters());

        let complete_req_schema = if available_schemas.contains_key(&complete_schema_name) {
            Some(complete_schema_name.as_str())
        } else if available_schemas.contains_key(resource_name) {
            Some(resource_name)
        } else {
            None
        };

        let request_body = complete_req_schema.map(|schema| {
            build_request_body(
                format!(
                    "Payload to complete the {} session.",
                    resource_name.to_lowercase()
                ),
                &format!("#/components/schemas/{}", schema),
                false,
            )
        });

        let responses = crud_responses(
            "200",
            format!("{} session completed successfully.", resource_name),
            Some(&resp_ref),
            Some(&format!(
                "Invalid {} completion request.",
                resource_name.to_lowercase()
            )),
            Some(&format!("{} session not found.", resource_name)),
        );

        let complete_op = build_operation(
            format!("complete{}", resource_name),
            format!("Complete {} Session", resource_name),
            format!(
                "Finalize and complete an active {} session.",
                resource_name.to_lowercase()
            ),
            vec![tag.to_string()],
            Some(complete_params),
            request_body,
            responses,
            Some(default_operation_security()),
        );

        paths.entry(complete_path).or_default().post = Some(complete_op);
    }

    // Cancel lifecycle action: POST /{collection}/{id}/cancel
    if has_cancel {
        let cancel_path = format!("{}/cancel", item_path);
        let mut cancel_params = vec![path_param_id.clone()];
        cancel_params.extend(mutating_protocol_parameters());

        let responses = crud_responses(
            "200",
            format!("{} session canceled successfully.", resource_name),
            Some(&resp_ref),
            Some(&format!(
                "Cannot cancel {} session in current state.",
                resource_name.to_lowercase()
            )),
            Some(&format!("{} session not found.", resource_name)),
        );

        let cancel_op = build_operation(
            format!("cancel{}", resource_name),
            format!("Cancel {} Session", resource_name),
            format!("Cancel an active {} session.", resource_name.to_lowercase()),
            vec![tag.to_string()],
            Some(cancel_params),
            None,
            responses,
            Some(default_operation_security()),
        );

        paths.entry(cancel_path).or_default().post = Some(cancel_op);
    }
}

/// Extract capability group stem/namespace from schema name or file path.
pub fn extract_capability_group(file_path: &std::path::Path, schema: &Value) -> String {
    if let Some(name) = schema.get("name").and_then(|v| v.as_str()) {
        let parts: Vec<&str> = name.split('.').collect();
        if parts.len() >= 4 {
            return parts[3].to_string();
        } else if parts.len() == 3 {
            return parts[2].to_string();
        }
    }

    let stem = file_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("capability");
    if let Some(pos) = stem.find('_') {
        stem[..pos].to_string()
    } else {
        stem.to_string()
    }
}

/// Project container capability operations dynamically from `$defs` request/response pairs.
///
/// Matches `{op}_request` and `{op}_response` in `$defs`, projecting:
/// - `get_{entity}` -> `GET /{group}/{plural_entity}/{id}`
/// - `{op}` -> `POST /{group}/{op}`
///
/// Returns the list of tags projected.
pub fn project_container_operations(
    file_path: &std::path::Path,
    schema: &Value,
    available_schemas: &BTreeMap<String, Value>,
    paths: &mut BTreeMap<String, PathItem>,
) -> Vec<String> {
    let defs = match schema.get("$defs").and_then(|d| d.as_object()) {
        Some(d) => d,
        None => return Vec::new(),
    };

    let group = extract_capability_group(file_path, schema);
    let tag = to_pascal_case(&group);
    let base_path = format!("/{}", group);

    // Find and sort all matched {op}_request and {op}_response pairs for determinism
    let mut matched_ops: Vec<String> = defs
        .keys()
        .filter_map(|k| k.strip_suffix("_request").map(|op| op.to_string()))
        .filter(|op| defs.contains_key(&format!("{}_response", op)))
        .collect();
    matched_ops.sort();

    if matched_ops.is_empty() {
        return Vec::new();
    }

    let mut projected_tags = Vec::new();

    for op in &matched_ops {
        let req_def_key = format!("{}_request", op);
        let resp_def_key = format!("{}_response", op);
        let req_def = match defs.get(&req_def_key) {
            Some(d) => d,
            None => continue,
        };
        let req_schema_name = to_pascal_case(&req_def_key);
        let resp_schema_name = to_pascal_case(&resp_def_key);

        if !available_schemas.contains_key(&resp_schema_name) {
            continue;
        }

        let resp_ref = format!("#/components/schemas/{}", resp_schema_name);

        if let Some(entity) = op.strip_prefix("get_") {
            // Single-entity retrieval: GET /{group}/{plural_entity}/{id}
            let entity_title = to_pascal_case(entity);
            let item_path = format!(
                "{}/{}/{{id}}",
                base_path,
                pluralize_path_segment(&entity_title)
            );

            let param_desc = format!("{} ID to lookup.", entity_title);
            let mut params = vec![ParameterOrRef::Item(Parameter {
                name: "id".to_string(),
                in_: ParameterIn::Path,
                description: Some(param_desc),
                required: Some(true),
                schema: Some(json!({ "type": "string" })),
            })];
            params.extend(non_mutating_protocol_parameters());

            let summary = format!("Get {} Details", entity_title);
            let description = req_def
                .get("description")
                .and_then(|d| d.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("Fetch full detail for a single {} by ID.", entity));
            let resp_desc = format!("Full {} details.", entity);

            let responses =
                BTreeMap::from([build_json_response("200", resp_desc, Some(&resp_ref))]);

            let operation_id = format!("get{}{}", tag, entity_title);

            let get_op = build_operation(
                operation_id,
                summary,
                description,
                vec![tag.clone()],
                Some(params),
                None,
                responses,
                Some(default_operation_security()),
            );

            paths.entry(item_path).or_default().get = Some(get_op);
            projected_tags.push(tag.clone());
        } else {
            // Action or search query: POST /{group}/{op}
            let action_path = format!("{}/{}", base_path, op);

            let req_required = req_def
                .get("required")
                .and_then(|r| r.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(false);

            let req_ref = format!("#/components/schemas/{}", req_schema_name);
            let has_req_schema = available_schemas.contains_key(&req_schema_name);

            let body_desc = req_def
                .get("description")
                .and_then(|d| d.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("Payload for {} {}.", op, group));

            let request_body = if has_req_schema {
                Some(build_request_body(body_desc, &req_ref, req_required))
            } else {
                None
            };

            let summary = req_def
                .get("title")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("{} {}", to_pascal_case(op), tag));

            let description = req_def
                .get("description")
                .and_then(|d| d.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("Execute {} operation on {}.", op, group));

            let resp_desc = format!("Results for {} {}.", op, group);

            let responses =
                BTreeMap::from([build_json_response("200", resp_desc, Some(&resp_ref))]);

            let operation_id = format!("{}{}", to_camel_case(op), tag);

            let post_op = build_operation(
                operation_id,
                summary,
                description,
                vec![tag.clone()],
                Some(non_mutating_protocol_parameters()),
                request_body,
                responses,
                Some(default_operation_security()),
            );

            paths.entry(action_path).or_default().post = Some(post_op);
            projected_tags.push(tag.clone());
        }
    }

    projected_tags
}

/// Project normative UCP protocol discovery endpoint (GET /.well-known/ucp per RFC 8615).
pub fn project_discovery_operations(
    available_schemas: &BTreeMap<String, Value>,
    paths: &mut BTreeMap<String, PathItem>,
) {
    project_resource_operations("Profile", None, available_schemas, paths);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pluralize_path_segment() {
        assert_eq!(pluralize_path_segment("Checkout"), "checkouts");
        assert_eq!(pluralize_path_segment("Cart"), "carts");
        assert_eq!(pluralize_path_segment("Order"), "orders");
        assert_eq!(pluralize_path_segment("Policy"), "policies");
        assert_eq!(pluralize_path_segment("Address"), "addresses");
    }

    #[test]
    fn test_standard_parameters() {
        let params = build_standard_parameters();
        assert!(params.contains_key("UcpAgent"));
        assert!(params.contains_key("IdempotencyKey"));
        assert!(params.contains_key("Signature"));
        assert!(params.contains_key("SignatureInput"));
        assert!(params.contains_key("SignatureAgent"));

        let ucp_agent = &params["UcpAgent"];
        assert_eq!(ucp_agent.name, "UCP-Agent");
        assert_eq!(ucp_agent.in_, ParameterIn::Header);
        assert_eq!(ucp_agent.required, Some(true));

        let idemp = &params["IdempotencyKey"];
        assert_eq!(idemp.name, "Idempotency-Key");
        assert_eq!(idemp.in_, ParameterIn::Header);
        assert_eq!(idemp.required, Some(true));
    }

    #[test]
    fn test_standard_security_schemes() {
        let schemes = build_standard_security_schemes();
        assert!(schemes.contains_key("HttpSignatureAuth"));
        assert!(schemes.contains_key("BearerAuth"));

        let http_sig = &schemes["HttpSignatureAuth"];
        assert_eq!(http_sig.type_, "http");
        assert_eq!(http_sig.scheme.as_deref(), Some("signature"));
    }

    #[test]
    fn test_project_resource_operations() {
        let mut schemas = BTreeMap::new();
        schemas.insert("CheckoutCreateRequest".to_string(), json!({}));
        schemas.insert("CheckoutUpdateRequest".to_string(), json!({}));
        schemas.insert("CheckoutCompleteRequest".to_string(), json!({}));
        schemas.insert("Checkout".to_string(), json!({}));

        let mut paths = BTreeMap::new();
        let raw_checkout = json!({
            "x-ucp-path": "/checkout-sessions",
            "x-ucp-lifecycle": ["complete", "cancel"]
        });
        project_resource_operations("Checkout", Some(&raw_checkout), &schemas, &mut paths);

        assert!(paths.contains_key("/checkout-sessions"));
        assert!(paths.contains_key("/checkout-sessions/{id}"));
        assert!(paths.contains_key("/checkout-sessions/{id}/complete"));
        assert!(paths.contains_key("/checkout-sessions/{id}/cancel"));

        let collection = &paths["/checkout-sessions"];
        assert!(collection.post.is_some());
        let post_op = collection.post.as_ref().unwrap();
        assert_eq!(post_op.operation_id.as_deref(), Some("createCheckout"));

        let item = &paths["/checkout-sessions/{id}"];
        assert!(item.get.is_some());
        assert!(item.put.is_some());
        assert_eq!(
            item.get.as_ref().unwrap().operation_id.as_deref(),
            Some("getCheckout")
        );
        assert_eq!(
            item.put.as_ref().unwrap().operation_id.as_deref(),
            Some("updateCheckout")
        );

        let complete = &paths["/checkout-sessions/{id}/complete"];
        assert!(complete.post.is_some());
        let complete_op = complete.post.as_ref().unwrap();
        assert_eq!(
            complete_op.operation_id.as_deref(),
            Some("completeCheckout")
        );

        let cancel = &paths["/checkout-sessions/{id}/cancel"];
        assert!(cancel.post.is_some());
        let cancel_op = cancel.post.as_ref().unwrap();
        assert_eq!(cancel_op.operation_id.as_deref(), Some("cancelCheckout"));

        // Also test non-checkout resource like Cart uses pluralized route, PUT, and cancel
        let mut cart_schemas = BTreeMap::new();
        cart_schemas.insert("CartCreateRequest".to_string(), json!({}));
        cart_schemas.insert("CartUpdateRequest".to_string(), json!({}));
        cart_schemas.insert("Cart".to_string(), json!({}));

        let mut cart_paths = BTreeMap::new();
        let raw_cart = json!({
            "x-ucp-lifecycle": ["cancel"]
        });
        project_resource_operations("Cart", Some(&raw_cart), &cart_schemas, &mut cart_paths);
        assert!(cart_paths.contains_key("/carts"));
        assert!(cart_paths.contains_key("/carts/{id}"));
        assert!(cart_paths.contains_key("/carts/{id}/cancel"));
        assert!(cart_paths["/carts/{id}"].put.is_some());

        // Test declarative x-ucp-path override
        let mut custom_schemas = BTreeMap::new();
        custom_schemas.insert("CustomCreateRequest".to_string(), json!({}));
        custom_schemas.insert("Custom".to_string(), json!({}));

        let custom_schema = json!({ "x-ucp-path": "custom-sessions" });
        let mut custom_paths = BTreeMap::new();
        project_resource_operations(
            "Custom",
            Some(&custom_schema),
            &custom_schemas,
            &mut custom_paths,
        );
        assert!(custom_paths.contains_key("/custom-sessions"));
        assert!(custom_paths.contains_key("/custom-sessions/{id}"));
    }

    #[test]
    fn test_builder_helpers() {
        let media_type = build_json_media_type("#/components/schemas/Test");
        assert!(media_type.contains_key("application/json"));
        assert_eq!(
            media_type["application/json"].schema.as_ref().unwrap()["$ref"],
            "#/components/schemas/Test"
        );

        let (status, resp) =
            build_json_response("200", "Success", Some("#/components/schemas/Test"));
        assert_eq!(status, "200");
        match resp {
            ResponseItemOrRef::Item(item) => {
                assert_eq!(item.description, "Success");
                assert!(item.content.is_some());
            }
            ResponseItemOrRef::Ref { .. } => panic!("Expected Item"),
        }

        let body = build_request_body("Req payload", "#/components/schemas/Req", true);
        match body {
            RequestBodyOrRef::Item(item) => {
                assert_eq!(item.description.as_deref(), Some("Req payload"));
                assert_eq!(item.required, Some(true));
            }
            RequestBodyOrRef::Ref { .. } => panic!("Expected Item"),
        }

        let op = build_operation(
            "testOp",
            "Test Op",
            "Test Op Description",
            vec!["TestTag".to_string()],
            None,
            None,
            BTreeMap::new(),
            None,
        );
        assert_eq!(op.operation_id.as_deref(), Some("testOp"));
        assert_eq!(op.tags.as_ref().unwrap(), &vec!["TestTag".to_string()]);
    }

    #[test]
    fn test_project_container_operations() {
        let catalog_search_schema = json!({
            "name": "dev.ucp.shopping.catalog.search",
            "title": "Catalog Search",
            "$defs": {
                "search_request": {
                    "type": "object",
                    "properties": { "query": { "type": "string" } }
                },
                "search_response": {
                    "type": "object",
                    "properties": { "products": { "type": "array" } }
                }
            }
        });

        let mut available = BTreeMap::new();
        available.insert("SearchRequest".to_string(), json!({}));
        available.insert("SearchResponse".to_string(), json!({}));

        let mut paths = BTreeMap::new();
        let path = std::path::Path::new("schemas/shopping/catalog_search.json");
        let tags =
            project_container_operations(path, &catalog_search_schema, &available, &mut paths);

        assert!(tags.contains(&"Catalog".to_string()));
        assert!(paths.contains_key("/catalog/search"));
        let search_path = &paths["/catalog/search"];
        assert!(search_path.post.is_some());
        let post_op = search_path.post.as_ref().unwrap();
        assert_eq!(post_op.operation_id.as_deref(), Some("searchCatalog"));

        // Test catalog_lookup with lookup and get_product
        let catalog_lookup_schema = json!({
            "name": "dev.ucp.shopping.catalog.lookup",
            "title": "Catalog Lookup",
            "$defs": {
                "lookup_request": {
                    "type": "object",
                    "required": ["ids"],
                    "properties": { "ids": { "type": "array" } }
                },
                "lookup_response": {
                    "type": "object"
                },
                "get_product_request": {
                    "type": "object",
                    "required": ["id"],
                    "properties": { "id": { "type": "string" } }
                },
                "get_product_response": {
                    "type": "object"
                }
            }
        });

        available.insert("LookupRequest".to_string(), json!({}));
        available.insert("LookupResponse".to_string(), json!({}));
        available.insert("GetProductRequest".to_string(), json!({}));
        available.insert("GetProductResponse".to_string(), json!({}));

        let mut lookup_paths = BTreeMap::new();
        let lookup_path = std::path::Path::new("schemas/shopping/catalog_lookup.json");
        let lookup_tags = project_container_operations(
            lookup_path,
            &catalog_lookup_schema,
            &available,
            &mut lookup_paths,
        );

        assert!(lookup_tags.contains(&"Catalog".to_string()));
        assert!(lookup_paths.contains_key("/catalog/lookup"));
        assert!(lookup_paths.contains_key("/catalog/products/{id}"));

        let lookup_op = lookup_paths["/catalog/lookup"].post.as_ref().unwrap();
        assert_eq!(lookup_op.operation_id.as_deref(), Some("lookupCatalog"));

        let get_op = lookup_paths["/catalog/products/{id}"].get.as_ref().unwrap();
        assert_eq!(get_op.operation_id.as_deref(), Some("getCatalogProduct"));
        assert!(get_op.parameters.is_some());

        // Test arbitrary namespace: dev.ucp.common.location.search
        let location_search_schema = json!({
            "name": "dev.ucp.common.location.search",
            "$defs": {
                "search_request": { "type": "object" },
                "search_response": { "type": "object" }
            }
        });
        let mut loc_paths = BTreeMap::new();
        let loc_path = std::path::Path::new("schemas/common/location_search.json");
        let loc_tags = project_container_operations(
            loc_path,
            &location_search_schema,
            &available,
            &mut loc_paths,
        );
        assert!(loc_tags.contains(&"Location".to_string()));
        assert!(loc_paths.contains_key("/location/search"));
        let loc_op = loc_paths["/location/search"].post.as_ref().unwrap();
        assert_eq!(loc_op.operation_id.as_deref(), Some("searchLocation"));
    }
}
