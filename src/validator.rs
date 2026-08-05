//! Payload validation against resolved schemas.

use serde_json::{Map, Value};

use crate::compose::is_container_schema;
use crate::error::{ResolveError, SchemaError, ValidateError};
use crate::resolver::resolve;
use crate::types::ResolveOptions;

/// Validate a payload against a UCP schema.
///
/// Resolves the schema for the given direction and operation, selects the
/// operation shape for container-shaped capabilities, then validates the
/// payload against the resulting schema.
///
/// # Errors
///
/// Returns `ValidateError::Resolve` if schema resolution or operation-shape
/// selection fails, or `ValidateError::Invalid` if the payload doesn't match.
pub fn validate(
    schema: &Value,
    payload: &Value,
    options: &ResolveOptions,
) -> Result<(), ValidateError> {
    let resolved = resolve(schema, options)?;

    // The message body to validate depends on the capability's shape:
    // single-object capabilities validate at the root; container capabilities
    // validate at the selected operation shape.
    let target = select_operation_schema(&resolved, options)?;

    validate_against_schema(&target, payload)
}

/// Resolve a (possibly container-shaped) schema to its validation target.
///
/// Selection has two modes:
///
/// - **Explicit** (`options.def_name`): root at the named `$defs` entry,
///   regardless of schema shape. Names non-derivable shapes — transport message
///   types (`error_response`), host views (`business_schema`) — and sub-types of
///   single-object schemas (`cart` → `checkout`), where the root has a body but
///   a fragment is being validated. Absent name → `DefNotFound`.
/// - **Derived** (no `def_name`): single-object capabilities validate at the
///   root unchanged; for a container capability (see
///   [`crate::is_container_schema`]) the target is the message body for this
///   `(op, direction)`, held at `$defs/{op}_{direction}`. A container root has
///   no body of its own, so an absent shape → `OperationShapeNotFound` rather
///   than a fall-through to an unconstrained root.
///
/// Either way the chosen `$def` is rooted via a `$ref` wrapper that keeps the
/// sibling `$defs` and `$schema` in scope — and, when the def's graph
/// references the source file's root (e.g. a self-root `$ref: "#"`), the whole
/// source schema as an embedded `$id`'d resource — so the dialect and every
/// internal ref resolve as they do in the source file.
pub fn select_operation_schema(
    schema: &Value,
    options: &ResolveOptions,
) -> Result<Value, ResolveError> {
    if let Some(def) = &options.def_name {
        return select_def(schema, def, SelectMode::Explicit);
    }
    if !is_container_schema(schema) {
        return Ok(schema.clone());
    }
    let key = format!("{}_{}", options.operation, options.direction.dir_str());
    select_def(schema, &key, SelectMode::Derived)
}

/// Whether the selected `$def` name was authored (`--def`) or computed from
/// `(op, direction)`. Only affects which "available" hint and error variant a
/// miss produces.
enum SelectMode {
    Explicit,
    Derived,
}

/// Root validation at `$defs/{name}` via a `$ref` wrapper that retains the
/// sibling `$defs` and `$schema` — and, when the def graph references the
/// source file's root, the source root itself (see
/// [`select_def_with_embedded_source`]).
fn select_def(schema: &Value, name: &str, mode: SelectMode) -> Result<Value, ResolveError> {
    let defs = schema.get("$defs").and_then(|d| d.as_object());
    let present = defs.map(|d| d.contains_key(name)).unwrap_or(false);
    if !present {
        let available = defs
            .map(|d| match mode {
                // Derived selection only ever targets operation shapes, so the
                // hint lists those; explicit selection can name any $def.
                SelectMode::Derived => d
                    .keys()
                    .filter(|k| k.ends_with("_request") || k.ends_with("_response"))
                    .cloned()
                    .collect::<Vec<_>>(),
                SelectMode::Explicit => d.keys().cloned().collect::<Vec<_>>(),
            })
            .unwrap_or_default()
            .join(", ");
        return Err(match mode {
            SelectMode::Derived => ResolveError::OperationShapeNotFound {
                key: name.to_string(),
                available,
            },
            SelectMode::Explicit => ResolveError::DefNotFound {
                def: name.to_string(),
                available,
            },
        });
    }

    // A def may reference its source file's ROOT — e.g. payment_instrument.json's
    // selected_payment_instrument is `allOf: [{"$ref": "#"}, ...]`. A wrapper
    // that holds only `$ref` + a copy of `$defs` re-binds `#` to the wrapper
    // root, whose only content is the `$ref` back into the def — an unbounded
    // resolution cycle that overflows the stack (issue #45). When the SELECTED
    // def's reachable subgraph depends on the source root, embed the whole
    // source file as an `$id`'d `$defs` resource (the 2020-12
    // embedded-resource rule) and root the wrapper via that `$id`: `#`,
    // `#/...` pointers, and refs to the file's own `$id` all keep their
    // source-file meaning. Detection is scoped to what the selected def can
    // reach — an unrelated sibling's root ref must not divert this selection —
    // so any other def keeps the plain wrapper, whose shape is emitted output
    // (`resolve --def`) and asserted by downstream consumers.
    let root_id = schema.get("$id").and_then(|v| v.as_str());
    let defs_map = defs.expect("presence checked above");
    if def_graph_references_source_root(defs_map, name, root_id) {
        return Ok(select_def_with_embedded_source(schema, name, root_id));
    }
    let defs_value = schema.get("$defs").expect("presence checked above");

    let mut wrapper = Map::new();
    if let Some(s) = schema.get("$schema") {
        wrapper.insert("$schema".to_string(), s.clone());
    }
    wrapper.insert(
        "$ref".to_string(),
        Value::String(format!("#/$defs/{}", name)),
    );
    wrapper.insert("$defs".to_string(), defs_value.clone());
    Ok(Value::Object(wrapper))
}

/// Wrapper for a def whose graph references its source file's root: the source
/// is embedded whole as an `$id`'d resource and the selection `$ref` goes
/// through that `$id`, so root-targeting refs resolve to the source root
/// rather than cycling through the wrapper (issue #45).
fn select_def_with_embedded_source(schema: &Value, name: &str, root_id: Option<&str>) -> Value {
    let root_id = match root_id {
        Some(id) => id.to_string(),
        None => synthesized_root_id(schema),
    };
    let mut source = schema.clone();
    if let Value::Object(obj) = &mut source {
        obj.entry("$id")
            .or_insert_with(|| Value::String(root_id.clone()));
    }

    let mut wrapper = Map::new();
    if let Some(s) = schema.get("$schema") {
        wrapper.insert("$schema".to_string(), s.clone());
    }
    wrapper.insert(
        "$ref".to_string(),
        Value::String(format!("{}#/$defs/{}", root_id.trim_end_matches('#'), name)),
    );
    let mut wrapper_defs = Map::new();
    wrapper_defs.insert(SELECTION_SOURCE_KEY.to_string(), source);
    wrapper.insert("$defs".to_string(), Value::Object(wrapper_defs));
    Value::Object(wrapper)
}

/// True if the selected def, or any sibling def it reaches transitively
/// through `#/$defs/...` pointers, references the source file's root resource
/// (see [`scan_root_refs`]). Scoped to the reachable subgraph so an unrelated
/// sibling's root ref does not divert an independent selection off the plain
/// wrapper, whose shape is emitted `resolve --def` output.
fn def_graph_references_source_root(
    defs: &Map<String, Value>,
    name: &str,
    root_id: Option<&str>,
) -> bool {
    let mut visited = std::collections::HashSet::new();
    let mut pending = vec![name.to_string()];
    while let Some(def_name) = pending.pop() {
        if !visited.insert(def_name.clone()) {
            continue;
        }
        // A pointer to a def that doesn't exist dangles the same way in both
        // wrapper shapes; it contributes nothing to root reachability.
        let Some(body) = defs.get(&def_name) else {
            continue;
        };
        let mut edges = Vec::new();
        if scan_root_refs(body, root_id, &mut edges) {
            return true;
        }
        pending.extend(edges);
    }
    false
}

/// Scan one def body. Returns true if it directly references the source
/// file's root — content the plain selection wrapper does not carry. Covers
/// every root-targeting ref form: the bare self-root `"#"`, fragment pointers
/// that escape `$defs` (e.g. `"#/properties/id"` — the wrapper holds only
/// `$defs`), and absolute refs to the file's own `$id`. Plain `#/$defs/...`
/// pointers and `#anchor` refs resolve identically inside the plain wrapper,
/// so they don't count as root refs; `#/$defs/X` targets are instead pushed
/// onto `edges` so the caller can walk the reachable subgraph.
fn scan_root_refs(value: &Value, root_id: Option<&str>, edges: &mut Vec<String>) -> bool {
    match value {
        Value::Object(map) => {
            for (key, v) in map {
                if key == "$ref" || key == "$dynamicRef" {
                    if let Some(r) = v.as_str() {
                        if r == "#" || (r.starts_with("#/") && !r.starts_with("#/$defs/")) {
                            return true;
                        }
                        if let Some(rest) = r.strip_prefix("#/$defs/") {
                            let token = rest.split('/').next().unwrap_or(rest);
                            // RFC 6901 pointer-token unescape: ~1 then ~0.
                            edges.push(token.replace("~1", "/").replace("~0", "~"));
                        }
                        if let Some(id) = root_id {
                            // Absolute refs into the file's own $id resolve by
                            // $id lookup, which only the embedded-source
                            // wrapper provides.
                            let base = id.trim_end_matches('#');
                            if r == base || r.strip_prefix(base).is_some_and(|s| s.starts_with('#'))
                            {
                                return true;
                            }
                        }
                    }
                }
                if scan_root_refs(v, root_id, edges) {
                    return true;
                }
            }
            false
        }
        Value::Array(arr) => arr.iter().any(|v| scan_root_refs(v, root_id, edges)),
        _ => false,
    }
}

/// `$defs` key the selection wrapper embeds the source schema under. The
/// embedded resource is addressed by its `$id`, so the key itself only needs to
/// be readable in emitted output (`resolve --def`).
const SELECTION_SOURCE_KEY: &str = "ucp_selected_def_source";

/// Stable root `$id` for a source file that has none, so the selection
/// wrapper's `$ref` (and any `#` inside the def) has a resource to bind to.
/// Content-hashed (same file, same id) in a reserved URN namespace, mirroring
/// the bundler's fallback for embedded self-root resources.
fn synthesized_root_id(file_root: &Value) -> String {
    let candidate = format!(
        "urn:ucp-schema:selected-def-root:{:016x}",
        stable_hash(&file_root.to_string())
    );
    unique_root_id(candidate, file_root)
}

/// The wrapper's `$ref` resolves by `$id` lookup, so a synthesized id must not
/// collide with an `$id` the source already declares (which would make the
/// lookup ambiguous). Suffix a counter until the candidate is unclaimed.
fn unique_root_id(candidate: String, schema: &Value) -> String {
    let mut id = candidate.clone();
    let mut n = 1;
    while declares_id(schema, &id) {
        n += 1;
        id = format!("{}-{}", candidate, n);
    }
    id
}

/// True if any object in `value` declares `"$id": id`.
fn declares_id(value: &Value, id: &str) -> bool {
    match value {
        Value::Object(map) => {
            map.get("$id").and_then(|v| v.as_str()) == Some(id)
                || map.values().any(|v| declares_id(v, id))
        }
        Value::Array(arr) => arr.iter().any(|v| declares_id(v, id)),
        _ => false,
    }
}

/// FNV-1a. Deterministic across runs and platforms (unlike `DefaultHasher`,
/// which is not guaranteed stable), so synthesized ids are reproducible.
fn stable_hash(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in s.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Validate a payload against an already-resolved schema.
///
/// Use this when you've already resolved the schema and want to validate
/// multiple payloads against it.
pub fn validate_against_schema(schema: &Value, payload: &Value) -> Result<(), ValidateError> {
    let validator = jsonschema::validator_for(schema).map_err(|e| {
        ValidateError::Resolve(ResolveError::InvalidSchema {
            message: e.to_string(),
        })
    })?;

    let errors: Vec<SchemaError> = validator
        .iter_errors(payload)
        .map(|e| SchemaError {
            path: e.instance_path.to_string(),
            message: e.to_string(),
        })
        .collect();

    if errors.is_empty() {
        Ok(())
    } else {
        Err(ValidateError::Invalid { errors })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Direction;
    use serde_json::json;

    #[test]
    fn validate_valid_payload() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            },
            "required": ["name"]
        });
        let payload = json!({ "name": "test" });
        let options = ResolveOptions::new(Direction::Request, "create");

        let result = validate(&schema, &payload, &options);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_missing_required_field() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "ucp_request": "required" }
            }
        });
        let payload = json!({});
        let options = ResolveOptions::new(Direction::Request, "create");

        let result = validate(&schema, &payload, &options);
        assert!(matches!(result, Err(ValidateError::Invalid { .. })));
    }

    #[test]
    fn validate_wrong_type() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            }
        });
        let payload = json!({ "name": 123 });
        let options = ResolveOptions::new(Direction::Request, "create");

        let result = validate(&schema, &payload, &options);
        assert!(matches!(result, Err(ValidateError::Invalid { .. })));
    }

    #[test]
    fn validate_omitted_field_rejected() {
        // When additionalProperties is false and a field is omitted,
        // sending that field should fail validation
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "id": { "type": "string", "ucp_request": "omit" },
                "name": { "type": "string" }
            }
        });
        let payload = json!({ "name": "test", "id": "123" });
        let options = ResolveOptions::new(Direction::Request, "create");

        let result = validate(&schema, &payload, &options);
        assert!(matches!(result, Err(ValidateError::Invalid { .. })));
    }

    #[test]
    fn validate_collects_multiple_errors() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "ucp_request": "required" },
                "age": { "type": "number", "ucp_request": "required" }
            }
        });
        let payload = json!({});
        let options = ResolveOptions::new(Direction::Request, "create");

        let result = validate(&schema, &payload, &options);
        match result {
            Err(ValidateError::Invalid { errors }) => {
                assert_eq!(errors.len(), 2);
            }
            _ => panic!("expected validation error with 2 errors"),
        }
    }

    #[test]
    fn validate_allof_strict_accepts_properties_from_all_branches() {
        // allOf with strict mode should accept properties defined in ANY branch
        // This tests that unevaluatedProperties correctly sees all branch properties
        let schema = json!({
            "allOf": [
                {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" }
                    }
                },
                {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" }
                    }
                }
            ]
        });
        // Payload uses properties from BOTH branches
        let payload = json!({ "id": "123", "name": "test" });
        let options = ResolveOptions::new(Direction::Request, "create").strict(true);

        let result = validate(&schema, &payload, &options);
        assert!(
            result.is_ok(),
            "should accept properties from all allOf branches"
        );
    }

    #[test]
    fn validate_allof_strict_rejects_unknown_properties() {
        // allOf with strict mode should reject properties not in ANY branch
        let schema = json!({
            "allOf": [
                {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" }
                    }
                },
                {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" }
                    }
                }
            ]
        });
        // Payload has unknown property
        let payload = json!({ "id": "123", "name": "test", "unknown": "bad" });
        let options = ResolveOptions::new(Direction::Request, "create").strict(true);

        let result = validate(&schema, &payload, &options);
        assert!(
            matches!(result, Err(ValidateError::Invalid { .. })),
            "should reject unknown properties in strict mode"
        );
    }

    #[test]
    fn select_def_self_root_ref_binds_to_source_root() {
        // Issue #45: a def shaped like payment_instrument.json's
        // selected_payment_instrument ($ref: "#" back to its file root) used to
        // send "#" into the selection wrapper and overflow the stack.
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://ucp.dev/schemas/shopping/types/payment_instrument.json",
            "type": "object",
            "required": ["handler_id", "type"],
            "properties": {
                "handler_id": { "type": "string" },
                "type": { "type": "string" }
            },
            "$defs": {
                "selected_payment_instrument": {
                    "allOf": [
                        { "$ref": "#" },
                        {
                            "type": "object",
                            "properties": { "selected": { "type": "boolean" } }
                        }
                    ]
                }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create")
            .def_name(Some("selected_payment_instrument".to_string()));
        let target = select_operation_schema(&schema, &options).unwrap();

        // Valid: satisfies the file root (via "#") and the def's own branch.
        let valid = json!({ "handler_id": "h", "type": "card", "selected": true });
        assert!(validate_against_schema(&target, &valid).is_ok());

        // Invalid: "handler_id" is required by the file ROOT, reachable only
        // through a correctly bound "#".
        let invalid = json!({ "type": "card", "selected": true });
        assert!(matches!(
            validate_against_schema(&target, &invalid),
            Err(ValidateError::Invalid { .. })
        ));
    }

    #[test]
    fn select_def_self_root_ref_without_id_synthesizes_stable_root() {
        let schema = json!({
            "type": "object",
            "required": ["handler_id"],
            "properties": { "handler_id": { "type": "string" } },
            "$defs": {
                "selected": {
                    "allOf": [
                        { "$ref": "#" },
                        { "properties": { "selected": { "type": "boolean" } } }
                    ]
                }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create")
            .def_name(Some("selected".to_string()));
        let target = select_operation_schema(&schema, &options).unwrap();

        // Synthesized id is deterministic: same source, same wrapper.
        let again = select_operation_schema(&schema, &options).unwrap();
        assert_eq!(target, again);

        assert!(
            validate_against_schema(&target, &json!({ "handler_id": "h", "selected": true }))
                .is_ok()
        );
        assert!(matches!(
            validate_against_schema(&target, &json!({ "selected": true })),
            Err(ValidateError::Invalid { .. })
        ));
    }

    #[test]
    fn select_def_without_self_root_ref_validates_as_before() {
        // The pre-#45 wrapper already handled sibling #/$defs/... refs; the
        // embedded-source wrapper must accept/reject exactly the same payloads.
        let schema = json!({
            "type": "object",
            "properties": { "name": { "type": "string" } },
            "$defs": {
                "error_response": {
                    "type": "object",
                    "required": ["code"],
                    "properties": {
                        "code": { "type": "string" },
                        "detail": { "$ref": "#/$defs/detail" }
                    }
                },
                "detail": { "type": "string" }
            }
        });
        let options = ResolveOptions::new(Direction::Request, "create")
            .def_name(Some("error_response".to_string()));
        let target = select_operation_schema(&schema, &options).unwrap();

        // Shape contract: without a root dependency the wrapper is unchanged
        // from before the #45 fix (it is emitted by `resolve --def`).
        assert_eq!(target["$ref"], "#/$defs/error_response");
        assert_eq!(target["$defs"], schema["$defs"]);

        assert!(
            validate_against_schema(&target, &json!({ "code": "oops", "detail": "broke" })).is_ok()
        );
        assert!(matches!(
            validate_against_schema(&target, &json!({ "detail": 5 })),
            Err(ValidateError::Invalid { .. })
        ));
        assert!(matches!(
            validate_against_schema(&target, &json!({ "code": "oops", "detail": 5 })),
            Err(ValidateError::Invalid { .. })
        ));
    }

    #[test]
    fn select_def_ignores_root_ref_in_unreachable_sibling() {
        // Root-reference detection must be scoped to the SELECTED def's
        // reachable subgraph: a sibling def with `$ref: "#"` that the selected
        // def never references must not divert the selection off the plain
        // wrapper (whose shape is emitted `resolve --def` output).
        let schema = json!({
            "type": "object",
            "required": ["handler_id"],
            "properties": { "handler_id": { "type": "string" } },
            "$defs": {
                "selected": {
                    "allOf": [
                        { "$ref": "#" },
                        { "properties": { "selected": { "type": "boolean" } } }
                    ]
                },
                "error_response": {
                    "type": "object",
                    "required": ["code"],
                    "properties": {
                        "code": { "type": "string" },
                        "detail": { "$ref": "#/$defs/detail" }
                    }
                },
                "detail": { "type": "string" }
            }
        });

        // error_response never reaches the root: plain wrapper, byte-identical
        // to the pre-#45 shape.
        let options = ResolveOptions::new(Direction::Request, "create")
            .def_name(Some("error_response".to_string()));
        let target = select_operation_schema(&schema, &options).unwrap();
        assert_eq!(target["$ref"], "#/$defs/error_response");
        assert_eq!(target["$defs"], schema["$defs"]);
        assert!(
            validate_against_schema(&target, &json!({ "code": "oops", "detail": "broke" })).is_ok()
        );
        assert!(matches!(
            validate_against_schema(&target, &json!({ "detail": 5 })),
            Err(ValidateError::Invalid { .. })
        ));

        // The sibling that DOES reference the root still embeds and validates.
        let options = ResolveOptions::new(Direction::Request, "create")
            .def_name(Some("selected".to_string()));
        let target = select_operation_schema(&schema, &options).unwrap();
        assert!(target["$defs"]["ucp_selected_def_source"].is_object());
        assert!(
            validate_against_schema(&target, &json!({ "handler_id": "h", "selected": true }))
                .is_ok()
        );
        assert!(matches!(
            validate_against_schema(&target, &json!({ "selected": true })),
            Err(ValidateError::Invalid { .. })
        ));
    }

    #[test]
    fn select_def_embeds_when_root_ref_is_reachable_transitively() {
        // The selected def has no root ref of its own, but references a
        // sibling that does: the root dependency is reachable, so the
        // embedded-source wrapper is required.
        let schema = json!({
            "type": "object",
            "required": ["handler_id"],
            "properties": { "handler_id": { "type": "string" } },
            "$defs": {
                "carrier": {
                    "type": "object",
                    "required": ["inner"],
                    "properties": { "inner": { "$ref": "#/$defs/selected" } }
                },
                "selected": {
                    "allOf": [
                        { "$ref": "#" },
                        { "properties": { "selected": { "type": "boolean" } } }
                    ]
                }
            }
        });
        let options =
            ResolveOptions::new(Direction::Request, "create").def_name(Some("carrier".to_string()));
        let target = select_operation_schema(&schema, &options).unwrap();
        assert!(
            target["$defs"]["ucp_selected_def_source"].is_object(),
            "root ref reachable via #/$defs/selected must trigger embedding"
        );

        // Root constraints apply INSIDE `inner` (carrier → selected → "#").
        assert!(validate_against_schema(
            &target,
            &json!({ "inner": { "handler_id": "h", "selected": true } })
        )
        .is_ok());
        assert!(matches!(
            validate_against_schema(&target, &json!({ "inner": { "selected": true } })),
            Err(ValidateError::Invalid { .. })
        ));
    }

    #[test]
    fn unique_root_id_avoids_ids_already_declared_by_the_source() {
        let candidate = "urn:ucp-schema:selected-def-root:0000000000000000".to_string();
        // Source already claims the candidate AND its first suffixed variant.
        let schema = json!({
            "$defs": {
                "a": { "$id": "urn:ucp-schema:selected-def-root:0000000000000000" },
                "b": { "$id": "urn:ucp-schema:selected-def-root:0000000000000000-2" }
            }
        });
        assert_eq!(
            unique_root_id(candidate.clone(), &schema),
            format!("{}-3", candidate)
        );

        // No collision: candidate is used as-is.
        assert_eq!(
            unique_root_id(candidate.clone(), &json!({ "$defs": {} })),
            candidate
        );
    }

    #[test]
    fn validate_allof_non_strict_allows_unknown_properties() {
        // allOf without strict mode should allow unknown properties (extensibility)
        let schema = json!({
            "allOf": [
                {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" }
                    }
                },
                {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" }
                    }
                }
            ]
        });
        // Payload has unknown property
        let payload = json!({ "id": "123", "name": "test", "unknown": "allowed" });
        let options = ResolveOptions::new(Direction::Request, "create").strict(false);

        let result = validate(&schema, &payload, &options);
        assert!(
            result.is_ok(),
            "should allow unknown properties in non-strict mode"
        );
    }
}
