//! Naming, UCP keyword stripping, and `#/$defs/` reference rewriting for code generation.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

use crate::compose::capability_short_name;
use crate::loader::for_each_schema_object_mut;
use crate::types::{is_valid_version, UCP_ANNOTATIONS};

/// Convert a snake_case, kebab-case, or reverse-domain identifier into PascalCase.
pub fn to_pascal_case(s: &str) -> String {
    let base = if s.contains('.') && !s.ends_with(".json") {
        capability_short_name(s)
    } else {
        s.to_string()
    };

    base.split(['_', '-', ' ', '/'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mixed = part.chars().any(|c| c.is_ascii_uppercase())
                && part.chars().any(|c| c.is_ascii_lowercase());
            let mut chars = part.chars();
            let Some(first) = chars.next() else {
                return String::new();
            };
            let rest = if mixed {
                chars.as_str().to_string()
            } else {
                chars.as_str().to_ascii_lowercase()
            };
            format!("{}{rest}", first.to_ascii_uppercase())
        })
        .collect()
}

/// Returns `true` when `s` contains at least 3 dot-separated segments where every segment
/// is non-empty ASCII alphanumeric, `_`, or `-` (excluding `.json` filenames).
pub fn is_reverse_domain_name(s: &str) -> bool {
    if s.ends_with(".json") {
        return false;
    }
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() >= 3
        && parts.iter().all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
}

/// Returns `true` when a `$defs` key name is generic and must be qualified with its parent
/// schema name when hoisted into the shared `#/$defs/` namespace.
pub fn is_generic_def_name(name: &str) -> bool {
    const GENERIC: &str = "base|Base|entity|Entity|platform_schema|PlatformSchema|business_schema|BusinessSchema|response_schema|ResponseSchema|request|Request|response|Response|error|Error|config|Config|endpoint|Endpoint|quantity|Quantity";
    !name.is_empty() && GENERIC.split('|').any(|g| g == name)
}

/// Qualify a hoisted `$defs` entry name with `parent_pascal` when `def_key` is generic
/// or represents a container operation definition.
pub fn qualify_def_name(parent_pascal: &str, def_key: &str) -> String {
    let def_pascal = to_pascal_case(def_key);
    if is_generic_def_name(def_key) || is_generic_def_name(&def_pascal) {
        if parent_pascal.is_empty() || def_pascal.starts_with(parent_pascal) {
            def_pascal
        } else {
            format!("{parent_pascal}{def_pascal}")
        }
    } else if (parent_pascal.ends_with("Search") || parent_pascal.ends_with("Lookup"))
        && (def_key.ends_with("_request") || def_key.ends_with("_response"))
    {
        qualify_container_op_name(parent_pascal, def_key)
    } else {
        def_pascal
    }
}

/// Qualify a container schema operation definition key (`{op}_{direction}`) with the
/// container schema's domain prefix (e.g. `("CatalogSearch", "search_request")` -> `"CatalogSearchRequest"`).
pub fn qualify_container_op_name(stem_pascal: &str, op_key: &str) -> String {
    let op_pascal = to_pascal_case(op_key);
    if stem_pascal.is_empty() || op_pascal.starts_with(stem_pascal) {
        return op_pascal;
    }
    let prefix = match stem_pascal.rfind(|c: char| c.is_ascii_uppercase()) {
        Some(idx) if idx > 0 => &stem_pascal[..idx],
        _ => stem_pascal,
    };
    if op_pascal.starts_with(prefix) {
        op_pascal
    } else {
        format!("{prefix}{op_pascal}")
    }
}

/// Resolve a `$ref` string (`#`, `#/$defs/<key>`, `<file>#/$defs/<key>`, or `<file>.json`)
/// to its canonical PascalCase `$defs` key name.
pub fn ref_to_def_name(ref_str: &str, parent_name: Option<&str>) -> String {
    if ref_str == "#" {
        return parent_name.map_or_else(|| "Self".to_string(), to_pascal_case);
    }
    if let Some(def_key) = ref_str
        .strip_prefix("#/$defs/")
        .or_else(|| ref_str.strip_prefix("#/definitions/"))
    {
        let parent_pascal = parent_name.map(to_pascal_case).unwrap_or_default();
        return qualify_def_name(&parent_pascal, def_key);
    }
    if let Some((file_part, def_key)) = ref_str
        .split_once("#/$defs/")
        .or_else(|| ref_str.split_once("#/definitions/"))
    {
        return qualify_def_name(&file_stem_to_pascal(file_part), def_key);
    }
    file_stem_to_pascal(ref_str.split('#').next().unwrap_or(ref_str))
}

/// Strip UCP-specific authoring keywords and root metadata from a schema AST while
/// preserving instance-data payloads (`const`, `enum`, `default`, `examples`).
pub fn strip_ucp_keywords(value: &mut Value) {
    if let Some(root) = value.as_object_mut() {
        if root
            .get("version")
            .and_then(Value::as_str)
            .is_some_and(is_valid_version)
        {
            root.remove("version");
        }
        root.remove("requires");
        root.remove("embedded");
    }

    for_each_schema_object_mut(value, &mut |obj| {
        for key in UCP_ANNOTATIONS {
            obj.remove(*key);
        }
        obj.remove("ucp_shared_request");
        if obj
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(is_reverse_domain_name)
        {
            obj.remove("name");
        }
        for key in ["$schema", "$id"] {
            if obj.get(key).is_some_and(Value::is_string) {
                obj.remove(key);
            }
        }
        obj.retain(|k, _| !k.starts_with("x-ucp-"));
        prune_dangling_required(obj);
    });
}

/// Rewrite all schema-position `$ref` pointers in `value` to `#/$defs/<PascalName>`.
pub fn rewrite_refs_to_defs(value: &mut Value, current_def: &str, parent_name: Option<&str>) {
    let effective_parent = parent_name.unwrap_or(current_def);
    for_each_schema_object_mut(value, &mut |obj| {
        if let Some(Value::String(ref_str)) = obj.get("$ref") {
            let target = ref_to_def_name(ref_str, Some(effective_parent));
            obj.insert(
                "$ref".to_string(),
                Value::String(format!("#/$defs/{target}")),
            );
        }
    });
}

fn file_stem_to_pascal(path_or_url: &str) -> String {
    let filename = path_or_url.rsplit('/').next().unwrap_or(path_or_url);
    let stem = filename
        .strip_suffix(".schema.json")
        .or_else(|| filename.strip_suffix(".json"))
        .unwrap_or(filename);
    to_pascal_case(stem)
}

fn inspect_allof_props(obj: &Map<String, Value>, props: &mut BTreeSet<String>) -> (bool, bool) {
    let mut has_props = false;
    if let Some(Value::Object(map)) = obj.get("properties") {
        props.extend(map.keys().cloned());
        has_props = true;
    }
    let mut has_ref = obj.get("$ref").is_some_and(Value::is_string);
    if let Some(Value::Array(all_of)) = obj.get("allOf") {
        for branch in all_of.iter().filter_map(Value::as_object) {
            let (bp, br) = inspect_allof_props(branch, props);
            has_props |= bp;
            has_ref |= br;
        }
    }
    (has_props, has_ref)
}

fn prune_dangling_required(obj: &mut Map<String, Value>) {
    if !obj.get("required").is_some_and(Value::is_array) {
        return;
    }
    let mut declared = BTreeSet::new();
    let (has_props, has_ref) = inspect_allof_props(obj, &mut declared);
    let Some(Value::Array(reqs)) = obj.get_mut("required") else {
        return;
    };
    if has_props && !has_ref {
        reqs.retain(|v| v.as_str().is_some_and(|k| declared.contains(k)));
    }
    if reqs.is_empty() {
        obj.remove("required");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn to_pascal_case_converts_snake_kebab_and_reverse_domain() {
        assert_eq!(to_pascal_case("checkout"), "Checkout");
        assert_eq!(to_pascal_case("line_item"), "LineItem");
        assert_eq!(to_pascal_case("order-line-item"), "OrderLineItem");
        assert_eq!(to_pascal_case("dev.ucp.shopping.checkout"), "Checkout");
        assert_eq!(
            to_pascal_case("dev.ucp.shopping.buyer_consent"),
            "BuyerConsent"
        );
        assert_eq!(to_pascal_case("PlatformSchema"), "PlatformSchema");
    }

    #[test]
    fn is_reverse_domain_name_identifies_capability_names() {
        assert!(is_reverse_domain_name("dev.ucp.shopping.checkout"));
        assert!(is_reverse_domain_name("dev.shopify.catalog"));
        assert!(is_reverse_domain_name("dev.ucp.shopping.catalog.search"));
        assert!(!is_reverse_domain_name("checkout"));
        assert!(!is_reverse_domain_name("shopping.checkout"));
        assert!(!is_reverse_domain_name("rest.openapi.json"));
        assert!(!is_reverse_domain_name(
            "https://ucp.dev/schemas/checkout.json"
        ));
    }

    #[test]
    fn is_generic_def_name_matches_spec_list() {
        for generic in [
            "Base",
            "base",
            "Entity",
            "entity",
            "PlatformSchema",
            "platform_schema",
            "BusinessSchema",
            "business_schema",
            "ResponseSchema",
            "response_schema",
            "Request",
            "request",
            "Response",
            "response",
            "Error",
            "error",
            "Config",
            "config",
            "Endpoint",
            "endpoint",
            "Quantity",
            "quantity",
        ] {
            assert!(
                is_generic_def_name(generic),
                "expected {generic} to be generic"
            );
        }
        assert!(!is_generic_def_name("jwk_public_key"));
        assert!(!is_generic_def_name("lookup_variant"));
        assert!(!is_generic_def_name("discounts_object"));
    }

    #[test]
    fn qualify_def_name_qualifies_generic_and_preserves_specific_names() {
        assert_eq!(
            qualify_def_name("Capability", "platform_schema"),
            "CapabilityPlatformSchema"
        );
        assert_eq!(
            qualify_def_name("OrderLineItem", "quantity"),
            "OrderLineItemQuantity"
        );
        assert_eq!(
            qualify_def_name("Pagination", "request"),
            "PaginationRequest"
        );
        assert_eq!(
            qualify_def_name("Permalink", "endpoint"),
            "PermalinkEndpoint"
        );
        assert_eq!(qualify_def_name("Permalink", "config"), "PermalinkConfig");
        assert_eq!(
            qualify_def_name("CatalogLookup", "lookup_variant"),
            "LookupVariant"
        );
        assert_eq!(
            qualify_def_name("Profile", "jwk_public_key"),
            "JwkPublicKey"
        );
    }

    #[test]
    fn qualify_container_op_name_handles_shared_and_distinct_prefixes() {
        assert_eq!(
            qualify_container_op_name("CatalogSearch", "search_request"),
            "CatalogSearchRequest"
        );
        assert_eq!(
            qualify_container_op_name("CatalogSearch", "search_response"),
            "CatalogSearchResponse"
        );
        assert_eq!(
            qualify_container_op_name("CatalogLookup", "lookup_request"),
            "CatalogLookupRequest"
        );
        assert_eq!(
            qualify_container_op_name("CatalogLookup", "get_product_request"),
            "CatalogGetProductRequest"
        );
        assert_eq!(
            qualify_container_op_name("CatalogLookup", "get_product_response"),
            "CatalogGetProductResponse"
        );
        assert_eq!(
            qualify_container_op_name("LocationSearch", "location_search_request"),
            "LocationSearchRequest"
        );
        assert_eq!(
            qualify_container_op_name("LocationSearch", "search_request"),
            "LocationSearchRequest"
        );
    }

    #[test]
    fn ref_to_def_name_resolves_self_internal_and_external_refs() {
        assert_eq!(
            ref_to_def_name("#", Some("PaymentInstrument")),
            "PaymentInstrument"
        );
        assert_eq!(
            ref_to_def_name("#/$defs/base", Some("Profile")),
            "ProfileBase"
        );
        assert_eq!(
            ref_to_def_name("#/$defs/jwk_public_key", Some("Profile")),
            "JwkPublicKey"
        );
        assert_eq!(
            ref_to_def_name("../capability.json#/$defs/platform_schema", Some("Order")),
            "CapabilityPlatformSchema"
        );
        assert_eq!(
            ref_to_def_name(
                "../common/types/pagination.json#/$defs/request",
                Some("CatalogSearch")
            ),
            "PaginationRequest"
        );
        assert_eq!(
            ref_to_def_name(
                "catalog_lookup.json#/$defs/get_product_response",
                Some("Fulfillment")
            ),
            "CatalogGetProductResponse"
        );
        assert_eq!(
            ref_to_def_name("types/line_item.json", Some("Checkout")),
            "LineItem"
        );
        assert_eq!(
            ref_to_def_name(
                "https://ucp.dev/draft/schemas/common/types/error_response.json",
                None
            ),
            "ErrorResponse"
        );
    }

    #[test]
    fn strip_ucp_keywords_removes_annotations_and_preserves_instance_data() {
        let mut schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://ucp.dev/schemas/shopping/checkout.json",
            "name": "dev.ucp.shopping.checkout",
            "version": "2026-01-11",
            "requires": { "protocol": { "min": "2026-01-11" } },
            "embedded": { "methods": {} },
            "type": "object",
            "ucp_shared_request": true,
            "x-ucp-schema-transition": { "from": "optional", "to": "omit" },
            "required": ["id", "omitted_field"],
            "properties": {
                "id": {
                    "type": "string",
                    "ucp_request": "omit",
                    "ucp_response": "required",
                    "default": {
                        "ucp_request": "keep_inside_default",
                        "$schema": "keep_inside_default"
                    }
                },
                "name": {
                    "type": "string"
                }
            }
        });

        strip_ucp_keywords(&mut schema);

        assert!(schema.get("$schema").is_none());
        assert!(schema.get("$id").is_none());
        assert!(schema.get("name").is_none());
        assert!(schema.get("version").is_none());
        assert!(schema.get("requires").is_none());
        assert!(schema.get("embedded").is_none());
        assert!(schema.get("ucp_shared_request").is_none());
        assert!(schema.get("x-ucp-schema-transition").is_none());
        assert_eq!(schema["required"], json!(["id"]));
        assert!(schema["properties"]["id"].get("ucp_request").is_none());
        assert!(schema["properties"]["id"].get("ucp_response").is_none());
        assert_eq!(
            schema["properties"]["id"]["default"]["ucp_request"],
            "keep_inside_default"
        );
        assert!(schema["properties"].get("name").is_some());
    }

    #[test]
    fn strip_ucp_keywords_preserves_required_when_ref_present_and_removes_empty_required() {
        let mut with_ref = json!({
            "type": "object",
            "allOf": [{ "allOf": [{ "$ref": "#/$defs/Base" }] }],
            "properties": {
                "own_field": { "type": "string" }
            },
            "required": ["own_field", "inherited_field"]
        });
        strip_ucp_keywords(&mut with_ref);
        assert_eq!(
            with_ref["required"],
            json!(["own_field", "inherited_field"])
        );

        let mut with_nested_allof_props = json!({
            "allOf": [
                {
                    "allOf": [
                        { "properties": { "nested_prop": { "type": "string" } } }
                    ]
                }
            ],
            "required": ["nested_prop", "omitted_prop"],
            "anyOf": [
                { "required": ["eq"] },
                { "required": ["in"] }
            ]
        });
        strip_ucp_keywords(&mut with_nested_allof_props);
        assert_eq!(with_nested_allof_props["required"], json!(["nested_prop"]));
        assert_eq!(
            with_nested_allof_props["anyOf"][0]["required"],
            json!(["eq"])
        );

        let mut empty_req = json!({
            "type": "object",
            "properties": {},
            "required": ["omitted_field"]
        });
        strip_ucp_keywords(&mut empty_req);
        assert!(empty_req.get("required").is_none());
    }

    #[test]
    fn rewrite_refs_to_defs_rewrites_schema_refs_and_skips_instance_data() {
        let mut schema = json!({
            "type": "object",
            "allOf": [
                { "$ref": "../capability.json#/$defs/platform_schema" },
                { "$ref": "#/$defs/base" },
                { "$ref": "#/$defs/jwk_public_key" }
            ],
            "properties": {
                "parent": { "$ref": "#" },
                "item": { "$ref": "types/line_item.json" },
                "example_payload": {
                    "type": "object",
                    "examples": [
                        { "$ref": "should/not/be/rewritten.json" }
                    ],
                    "const": { "$ref": "#" }
                }
            }
        });

        rewrite_refs_to_defs(&mut schema, "Profile", Some("Profile"));

        assert_eq!(
            schema["allOf"][0]["$ref"],
            "#/$defs/CapabilityPlatformSchema"
        );
        assert_eq!(schema["allOf"][1]["$ref"], "#/$defs/ProfileBase");
        assert_eq!(schema["allOf"][2]["$ref"], "#/$defs/JwkPublicKey");
        assert_eq!(schema["properties"]["parent"]["$ref"], "#/$defs/Profile");
        assert_eq!(schema["properties"]["item"]["$ref"], "#/$defs/LineItem");
        assert_eq!(
            schema["properties"]["example_payload"]["examples"][0]["$ref"],
            "should/not/be/rewritten.json"
        );
        assert_eq!(
            schema["properties"]["example_payload"]["const"]["$ref"],
            "#"
        );

        // Idempotence check: running a second time preserves already-rewritten refs
        let once = schema.clone();
        rewrite_refs_to_defs(&mut schema, "Profile", Some("Profile"));
        assert_eq!(schema, once);
    }
}
