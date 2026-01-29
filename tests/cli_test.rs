//! CLI integration tests for ucp-schema binary.

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::TempDir;

fn cmd() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("ucp-schema"))
}

// Helper to create a temp schema file
fn write_temp_file(dir: &TempDir, name: &str, content: &str) -> std::path::PathBuf {
    let path = dir.path().join(name);
    fs::write(&path, content).unwrap();
    path
}

mod resolve_command {
    use super::*;

    #[test]
    fn basic_resolve() {
        let dir = TempDir::new().unwrap();
        let schema = write_temp_file(
            &dir,
            "schema.json",
            r#"{
                "type": "object",
                "properties": {
                    "id": { "type": "string", "ucp_request": "required" },
                    "name": { "type": "string" }
                }
            }"#,
        );

        cmd()
            .args([
                "resolve",
                schema.to_str().unwrap(),
                "--request",
                "--op",
                "create",
            ])
            .assert()
            .success()
            .stdout(predicate::str::contains(r#""required":["id"]"#));
    }

    #[test]
    fn resolve_with_pretty() {
        let dir = TempDir::new().unwrap();
        let schema = write_temp_file(
            &dir,
            "schema.json",
            r#"{"type":"object","properties":{"id":{"type":"string"}}}"#,
        );

        cmd()
            .args([
                "resolve",
                schema.to_str().unwrap(),
                "--request",
                "--op",
                "create",
                "--pretty",
            ])
            .assert()
            .success()
            // Pretty output has newlines and indentation
            .stdout(predicate::str::contains("{\n"));
    }

    #[test]
    fn resolve_with_output_file() {
        let dir = TempDir::new().unwrap();
        let schema = write_temp_file(
            &dir,
            "schema.json",
            r#"{"type":"object","properties":{"id":{"type":"string"}}}"#,
        );
        let output = dir.path().join("output.json");

        cmd()
            .args([
                "resolve",
                schema.to_str().unwrap(),
                "--request",
                "--op",
                "create",
                "--output",
                output.to_str().unwrap(),
            ])
            .assert()
            .success()
            .stdout(predicate::str::is_empty());

        // Verify file was written
        let content = fs::read_to_string(&output).unwrap();
        assert!(content.contains(r#""type":"object""#));
    }

    #[test]
    fn resolve_strips_annotations() {
        let dir = TempDir::new().unwrap();
        let schema = write_temp_file(
            &dir,
            "schema.json",
            r#"{
                "type": "object",
                "properties": {
                    "id": { "type": "string", "ucp_request": "required" }
                }
            }"#,
        );

        cmd()
            .args([
                "resolve",
                schema.to_str().unwrap(),
                "--request",
                "--op",
                "create",
            ])
            .assert()
            .success()
            // Should not contain UCP annotations in output
            .stdout(predicate::str::contains("ucp_request").not());
    }

    #[test]
    fn resolve_omits_field() {
        let dir = TempDir::new().unwrap();
        let schema = write_temp_file(
            &dir,
            "schema.json",
            r#"{
                "type": "object",
                "properties": {
                    "id": { "type": "string", "ucp_request": "omit" },
                    "name": { "type": "string" }
                }
            }"#,
        );

        cmd()
            .args([
                "resolve",
                schema.to_str().unwrap(),
                "--request",
                "--op",
                "create",
            ])
            .assert()
            .success()
            // Should not contain "id" property in output
            .stdout(predicate::str::contains(r#""id""#).not())
            .stdout(predicate::str::contains(r#""name""#));
    }

    #[test]
    fn resolve_response_direction() {
        let dir = TempDir::new().unwrap();
        let schema = write_temp_file(
            &dir,
            "schema.json",
            r#"{
                "type": "object",
                "properties": {
                    "id": { "type": "string", "ucp_response": "required" }
                }
            }"#,
        );

        cmd()
            .args([
                "resolve",
                schema.to_str().unwrap(),
                "--response",
                "--op",
                "read",
            ])
            .assert()
            .success()
            .stdout(predicate::str::contains(r#""required":["id"]"#));
    }

    #[test]
    fn resolve_operation_case_insensitive() {
        let dir = TempDir::new().unwrap();
        let schema = write_temp_file(
            &dir,
            "schema.json",
            r#"{
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "ucp_request": { "create": "required", "update": "omit" }
                    }
                }
            }"#,
        );

        // Using uppercase CREATE should work and match "create"
        cmd()
            .args([
                "resolve",
                schema.to_str().unwrap(),
                "--request",
                "--op",
                "CREATE",
            ])
            .assert()
            .success()
            .stdout(predicate::str::contains(r#""required":["id"]"#));
    }
}

mod validate_command {
    use super::*;

    #[test]
    fn validate_valid_payload() {
        let dir = TempDir::new().unwrap();
        let schema = write_temp_file(
            &dir,
            "schema.json",
            r#"{
                "type": "object",
                "properties": {
                    "name": { "type": "string", "ucp_request": "required" }
                }
            }"#,
        );
        let payload = write_temp_file(&dir, "payload.json", r#"{"name": "test"}"#);

        cmd()
            .args([
                "validate",
                payload.to_str().unwrap(),
                "--schema",
                schema.to_str().unwrap(),
                "--request",
                "--op",
                "create",
            ])
            .assert()
            .success()
            .stdout(predicate::str::contains("Valid"));
    }

    #[test]
    fn validate_missing_required_field() {
        let dir = TempDir::new().unwrap();
        let schema = write_temp_file(
            &dir,
            "schema.json",
            r#"{
                "type": "object",
                "properties": {
                    "name": { "type": "string", "ucp_request": "required" }
                }
            }"#,
        );
        let payload = write_temp_file(&dir, "payload.json", r#"{}"#);

        cmd()
            .args([
                "validate",
                payload.to_str().unwrap(),
                "--schema",
                schema.to_str().unwrap(),
                "--request",
                "--op",
                "create",
            ])
            .assert()
            .code(1)
            .stderr(predicate::str::contains("Validation failed"));
    }

    #[test]
    fn validate_wrong_type() {
        let dir = TempDir::new().unwrap();
        let schema = write_temp_file(
            &dir,
            "schema.json",
            r#"{
                "type": "object",
                "properties": {
                    "age": { "type": "number" }
                }
            }"#,
        );
        let payload = write_temp_file(&dir, "payload.json", r#"{"age": "not-a-number"}"#);

        cmd()
            .args([
                "validate",
                payload.to_str().unwrap(),
                "--schema",
                schema.to_str().unwrap(),
                "--request",
                "--op",
                "create",
            ])
            .assert()
            .code(1)
            .stderr(predicate::str::contains("Validation failed"));
    }

    #[test]
    fn validate_additional_property_rejected() {
        let dir = TempDir::new().unwrap();
        let schema = write_temp_file(
            &dir,
            "schema.json",
            r#"{
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "id": { "type": "string", "ucp_request": "omit" },
                    "name": { "type": "string" }
                }
            }"#,
        );
        // Try to send "id" which is omitted - should be rejected as additional property
        let payload = write_temp_file(&dir, "payload.json", r#"{"name": "test", "id": "123"}"#);

        cmd()
            .args([
                "validate",
                payload.to_str().unwrap(),
                "--schema",
                schema.to_str().unwrap(),
                "--request",
                "--op",
                "create",
            ])
            .assert()
            .code(1);
    }

    #[test]
    fn validate_json_output_valid() {
        let dir = TempDir::new().unwrap();
        let schema = write_temp_file(
            &dir,
            "schema.json",
            r#"{
                "type": "object",
                "properties": {
                    "name": { "type": "string" }
                }
            }"#,
        );
        let payload = write_temp_file(&dir, "payload.json", r#"{"name": "test"}"#);

        cmd()
            .args([
                "validate",
                payload.to_str().unwrap(),
                "--schema",
                schema.to_str().unwrap(),
                "--request",
                "--op",
                "create",
                "--json",
            ])
            .assert()
            .success()
            .stdout(predicate::str::contains(r#"{"valid":true}"#));
    }

    #[test]
    fn validate_json_output_invalid() {
        let dir = TempDir::new().unwrap();
        let schema = write_temp_file(
            &dir,
            "schema.json",
            r#"{
                "type": "object",
                "properties": {
                    "name": { "type": "string", "ucp_request": "required" }
                }
            }"#,
        );
        let payload = write_temp_file(&dir, "payload.json", r#"{}"#);

        cmd()
            .args([
                "validate",
                payload.to_str().unwrap(),
                "--schema",
                schema.to_str().unwrap(),
                "--request",
                "--op",
                "create",
                "--json",
            ])
            .assert()
            .code(1)
            .stdout(predicate::str::contains(r#""valid":false"#))
            .stdout(predicate::str::contains(r#""errors":"#));
    }

    #[test]
    fn validate_json_output_file_error() {
        let dir = TempDir::new().unwrap();
        let payload = write_temp_file(&dir, "payload.json", r#"{}"#);

        cmd()
            .args([
                "validate",
                payload.to_str().unwrap(),
                "--schema",
                "/nonexistent/schema.json",
                "--request",
                "--op",
                "create",
                "--json",
            ])
            .assert()
            .code(3)
            .stdout(predicate::str::contains(r#""valid":false"#))
            .stdout(predicate::str::contains(r#""errors":"#));
    }
}

mod error_handling {
    use super::*;

    #[test]
    fn file_not_found() {
        cmd()
            .args([
                "resolve",
                "/nonexistent/schema.json",
                "--request",
                "--op",
                "create",
            ])
            .assert()
            .code(3)
            .stderr(
                predicate::str::contains("not found").or(predicate::str::contains("No such file")),
            );
    }

    #[test]
    fn invalid_json_schema() {
        let dir = TempDir::new().unwrap();
        let schema = write_temp_file(&dir, "bad.json", r#"{ not valid json"#);

        cmd()
            .args([
                "resolve",
                schema.to_str().unwrap(),
                "--request",
                "--op",
                "create",
            ])
            .assert()
            .code(2);
    }

    #[test]
    fn invalid_annotation_type() {
        let dir = TempDir::new().unwrap();
        let schema = write_temp_file(
            &dir,
            "schema.json",
            r#"{
                "type": "object",
                "properties": {
                    "id": { "type": "string", "ucp_request": 123 }
                }
            }"#,
        );

        cmd()
            .args([
                "resolve",
                schema.to_str().unwrap(),
                "--request",
                "--op",
                "create",
            ])
            .assert()
            .code(2)
            .stderr(predicate::str::contains("annotation"));
    }

    #[test]
    fn unknown_visibility_value() {
        let dir = TempDir::new().unwrap();
        let schema = write_temp_file(
            &dir,
            "schema.json",
            r#"{
                "type": "object",
                "properties": {
                    "id": { "type": "string", "ucp_request": "readonly" }
                }
            }"#,
        );

        cmd()
            .args([
                "resolve",
                schema.to_str().unwrap(),
                "--request",
                "--op",
                "create",
            ])
            .assert()
            .code(2)
            .stderr(predicate::str::contains("unknown visibility"));
    }
}

mod required_args {
    use super::*;

    #[test]
    fn missing_direction_flag() {
        let dir = TempDir::new().unwrap();
        let schema = write_temp_file(&dir, "schema.json", r#"{"type":"object"}"#);

        cmd()
            .args(["resolve", schema.to_str().unwrap(), "--op", "create"])
            .assert()
            .failure()
            .stderr(
                predicate::str::contains("--request").or(predicate::str::contains("--response")),
            );
    }

    #[test]
    fn missing_op_flag() {
        let dir = TempDir::new().unwrap();
        let schema = write_temp_file(&dir, "schema.json", r#"{"type":"object"}"#);

        cmd()
            .args(["resolve", schema.to_str().unwrap(), "--request"])
            .assert()
            .failure()
            .stderr(predicate::str::contains("--op"));
    }

    #[test]
    fn conflicting_direction_flags() {
        let dir = TempDir::new().unwrap();
        let schema = write_temp_file(&dir, "schema.json", r#"{"type":"object"}"#);

        cmd()
            .args([
                "resolve",
                schema.to_str().unwrap(),
                "--request",
                "--response",
                "--op",
                "create",
            ])
            .assert()
            .failure()
            .stderr(predicate::str::contains("cannot be used with"));
    }

    #[test]
    fn missing_schema_path() {
        cmd()
            .args(["resolve", "--request", "--op", "create"])
            .assert()
            .failure();
    }

    #[test]
    fn missing_payload_for_validate() {
        // Payload is now required positional argument
        cmd()
            .args(["validate", "--request", "--op", "create"])
            .assert()
            .failure()
            .stderr(predicate::str::contains("PAYLOAD"));
    }
}

mod help_and_version {
    use super::*;

    #[test]
    fn help_flag() {
        cmd()
            .arg("--help")
            .assert()
            .success()
            .stdout(predicate::str::contains("Resolve and validate UCP schema"));
    }

    #[test]
    fn version_flag() {
        cmd()
            .arg("--version")
            .assert()
            .success()
            .stdout(predicate::str::contains("ucp-schema"));
    }

    #[test]
    fn resolve_help() {
        cmd()
            .args(["resolve", "--help"])
            .assert()
            .success()
            .stdout(predicate::str::contains("--request"))
            .stdout(predicate::str::contains("--response"))
            .stdout(predicate::str::contains("--op"));
    }

    #[test]
    fn validate_help() {
        cmd()
            .args(["validate", "--help"])
            .assert()
            .success()
            .stdout(predicate::str::contains("--request"))
            .stdout(predicate::str::contains("--response"))
            .stdout(predicate::str::contains("--op"));
    }
}

mod fixtures {
    use super::*;

    #[test]
    fn resolve_checkout_fixture_create() {
        let fixture = "tests/fixtures/checkout.json";

        cmd()
            .args(["resolve", fixture, "--request", "--op", "create"])
            .assert()
            .success()
            // line_items is required for create
            .stdout(predicate::str::contains("line_items"));
    }

    #[test]
    fn resolve_checkout_fixture_update() {
        let fixture = "tests/fixtures/checkout.json";

        cmd()
            .args(["resolve", fixture, "--request", "--op", "update"])
            .assert()
            .success()
            // id is required for update
            .stdout(predicate::str::contains(r#""required":["id"]"#));
    }

    #[test]
    fn validate_checkout_create_valid() {
        let dir = TempDir::new().unwrap();
        let payload = write_temp_file(
            &dir,
            "payload.json",
            r#"{
                "line_items": [
                    { "sku": "ABC123", "quantity": 2 }
                ]
            }"#,
        );

        cmd()
            .args([
                "validate",
                payload.to_str().unwrap(),
                "--schema",
                "tests/fixtures/checkout.json",
                "--request",
                "--op",
                "create",
            ])
            .assert()
            .success()
            .stdout(predicate::str::contains("Valid"));
    }

    #[test]
    fn validate_checkout_create_missing_required() {
        let dir = TempDir::new().unwrap();
        // Missing line_items which is required for create
        let payload = write_temp_file(&dir, "payload.json", r#"{}"#);

        cmd()
            .args([
                "validate",
                payload.to_str().unwrap(),
                "--schema",
                "tests/fixtures/checkout.json",
                "--request",
                "--op",
                "create",
            ])
            .assert()
            .code(1)
            .stderr(predicate::str::contains("Validation failed"));
    }
}

/// Bundle flag tests - resolve external $refs
mod bundle {
    use super::*;

    #[test]
    fn bundle_resolves_external_ref() {
        let dir = TempDir::new().unwrap();

        // Create a referenced type schema
        fs::create_dir_all(dir.path().join("types")).unwrap();
        fs::write(
            dir.path().join("types/buyer.json"),
            r#"{"type":"object","properties":{"email":{"type":"string"}}}"#,
        )
        .unwrap();

        // Create main schema with $ref
        let schema = write_temp_file(
            &dir,
            "schema.json",
            r#"{
                "type": "object",
                "properties": {
                    "buyer": { "$ref": "types/buyer.json" }
                }
            }"#,
        );

        cmd()
            .args([
                "resolve",
                schema.to_str().unwrap(),
                "--request",
                "--op",
                "create",
                "--bundle",
            ])
            .assert()
            .success()
            // $ref should be resolved, email property should be present
            .stdout(predicate::str::contains(r#""email""#))
            // No $ref should remain (except self-refs)
            .stdout(predicate::str::contains(r#""$ref":"types/buyer.json""#).not());
    }

    #[test]
    fn bundle_resolves_fragment_ref() {
        let dir = TempDir::new().unwrap();

        // Create schema with $defs
        fs::create_dir_all(dir.path().join("types")).unwrap();
        fs::write(
            dir.path().join("types/common.json"),
            r#"{
                "$defs": {
                    "address": {
                        "type": "object",
                        "properties": {
                            "street": { "type": "string" }
                        }
                    }
                }
            }"#,
        )
        .unwrap();

        // Reference specific $def with fragment
        let schema = write_temp_file(
            &dir,
            "schema.json",
            r#"{
                "type": "object",
                "properties": {
                    "shipping": { "$ref": "types/common.json#/$defs/address" }
                }
            }"#,
        );

        cmd()
            .args([
                "resolve",
                schema.to_str().unwrap(),
                "--request",
                "--op",
                "create",
                "--bundle",
            ])
            .assert()
            .success()
            // Fragment should be resolved, street property should be present
            .stdout(predicate::str::contains(r#""street""#));
    }

    #[test]
    fn bundle_preserves_self_root_ref() {
        let dir = TempDir::new().unwrap();

        // Create schema with self-root ref (recursive type)
        fs::create_dir_all(dir.path().join("types")).unwrap();
        fs::write(
            dir.path().join("types/node.json"),
            r##"{
                "type": "object",
                "properties": {
                    "value": { "type": "string" },
                    "children": {
                        "type": "array",
                        "items": { "$ref": "#" }
                    }
                }
            }"##,
        )
        .unwrap();

        let schema = write_temp_file(
            &dir,
            "schema.json",
            r#"{
                "type": "object",
                "properties": {
                    "tree": { "$ref": "types/node.json" }
                }
            }"#,
        );

        cmd()
            .args([
                "resolve",
                schema.to_str().unwrap(),
                "--request",
                "--op",
                "create",
                "--bundle",
            ])
            .assert()
            .success()
            // Self-root ref should be preserved (can't inline recursive)
            .stdout(predicate::str::contains(r##""$ref":"#""##));
    }

    #[test]
    fn bundle_resolves_internal_refs_in_external_files() {
        let dir = TempDir::new().unwrap();

        // External file with internal $defs reference
        fs::create_dir_all(dir.path().join("types")).unwrap();
        fs::write(
            dir.path().join("types/wrapper.json"),
            r##"{
                "$defs": {
                    "inner": {
                        "type": "string",
                        "minLength": 1
                    }
                },
                "type": "object",
                "properties": {
                    "data": { "$ref": "#/$defs/inner" }
                }
            }"##,
        )
        .unwrap();

        let schema = write_temp_file(
            &dir,
            "schema.json",
            r#"{
                "type": "object",
                "properties": {
                    "wrapped": { "$ref": "types/wrapper.json" }
                }
            }"#,
        );

        cmd()
            .args([
                "resolve",
                schema.to_str().unwrap(),
                "--request",
                "--op",
                "create",
                "--bundle",
            ])
            .assert()
            .success()
            // Internal ref should be resolved
            .stdout(predicate::str::contains(r#""minLength""#));
    }

    #[test]
    fn bundle_detects_circular_refs() {
        let dir = TempDir::new().unwrap();

        // Create circular reference: a.json -> b.json -> a.json
        fs::create_dir_all(dir.path().join("types")).unwrap();
        fs::write(
            dir.path().join("types/a.json"),
            r#"{"type":"object","properties":{"b":{"$ref":"b.json"}}}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("types/b.json"),
            r#"{"type":"object","properties":{"a":{"$ref":"a.json"}}}"#,
        )
        .unwrap();

        let schema = write_temp_file(
            &dir,
            "schema.json",
            r#"{
                "type": "object",
                "properties": {
                    "start": { "$ref": "types/a.json" }
                }
            }"#,
        );

        cmd()
            .args([
                "resolve",
                schema.to_str().unwrap(),
                "--request",
                "--op",
                "create",
                "--bundle",
            ])
            .assert()
            .failure()
            .stderr(predicate::str::contains("circular"));
    }

    #[test]
    fn bundle_output_is_valid_json() {
        let dir = TempDir::new().unwrap();

        fs::create_dir_all(dir.path().join("types")).unwrap();
        fs::write(
            dir.path().join("types/item.json"),
            r#"{"type":"object","properties":{"id":{"type":"string"}}}"#,
        )
        .unwrap();

        let schema = write_temp_file(
            &dir,
            "schema.json",
            r#"{
                "type": "object",
                "properties": {
                    "item": { "$ref": "types/item.json" }
                }
            }"#,
        );

        let output = dir.path().join("bundled.json");

        cmd()
            .args([
                "resolve",
                schema.to_str().unwrap(),
                "--request",
                "--op",
                "create",
                "--bundle",
                "--output",
                output.to_str().unwrap(),
            ])
            .assert()
            .success();

        // Verify output is valid JSON
        let content = fs::read_to_string(&output).unwrap();
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&content);
        assert!(parsed.is_ok(), "Bundle output should be valid JSON");
    }
}

/// Remote schema loading tests - require network access
mod remote {
    use super::*;

    #[test]
    fn resolve_from_url() {
        // Use httpbin.org which returns valid JSON
        // The resolve command should fetch and process it
        cmd()
            .args([
                "resolve",
                "https://httpbin.org/json",
                "--request",
                "--op",
                "create",
            ])
            .assert()
            .success()
            // httpbin returns {"slideshow": {...}} which should pass through
            .stdout(predicate::str::contains("slideshow"));
    }

    #[test]
    fn resolve_url_404() {
        cmd()
            .args([
                "resolve",
                "https://httpbin.org/status/404",
                "--request",
                "--op",
                "create",
            ])
            .assert()
            .code(3) // Network errors are exit code 3
            .stderr(
                predicate::str::contains("failed to fetch").or(predicate::str::contains("404")),
            );
    }

    #[test]
    fn resolve_url_invalid_host() {
        cmd()
            .args([
                "resolve",
                "https://this-domain-does-not-exist-12345.invalid/schema.json",
                "--request",
                "--op",
                "create",
            ])
            .assert()
            .code(3);
    }

    #[test]
    fn validate_with_remote_schema() {
        let dir = TempDir::new().unwrap();
        // httpbin.org/json returns {"slideshow": {"author": "...", ...}}
        // Create a payload that matches that structure
        let payload = write_temp_file(
            &dir,
            "payload.json",
            r#"{"slideshow": {"author": "Test Author", "title": "Test"}}"#,
        );

        cmd()
            .args([
                "validate",
                payload.to_str().unwrap(),
                "--schema",
                "https://httpbin.org/json",
                "--request",
                "--op",
                "create",
            ])
            .assert()
            .success();
    }
}

/// Schema composition tests - self-describing payloads
mod compose {
    use super::*;

    #[test]
    fn self_describing_checkout_only() {
        // Validate a self-describing response against local schemas
        // Note: --strict=false because strict mode + allOf composition conflict
        cmd()
            .args([
                "validate",
                "tests/fixtures/compose/response_checkout_only.json",
                "--schema-local-base",
                "tests/fixtures/compose",
                "--response",
                "--op",
                "read",
                "--strict=false",
            ])
            .assert()
            .success()
            .stdout(predicate::str::contains("Valid"));
    }

    #[test]
    fn self_describing_with_extensions() {
        // Validate a self-describing response with discount + fulfillment extensions
        // Note: --strict=false because strict mode + allOf composition conflict
        cmd()
            .args([
                "validate",
                "tests/fixtures/compose/response_with_extensions.json",
                "--schema-local-base",
                "tests/fixtures/compose",
                "--response",
                "--op",
                "read",
                "--strict=false",
            ])
            .assert()
            .success()
            .stdout(predicate::str::contains("Valid"));
    }

    #[test]
    fn direction_auto_inferred_response() {
        // Direction should be auto-inferred from ucp.capabilities
        // Note: --strict=false because strict mode + allOf composition conflict
        cmd()
            .args([
                "validate",
                "tests/fixtures/compose/response_checkout_only.json",
                "--schema-local-base",
                "tests/fixtures/compose",
                "--op",
                "read",
                "--strict=false",
            ])
            .assert()
            .success()
            .stdout(predicate::str::contains("Valid"));
    }

    #[test]
    fn schema_remote_base_maps_url_prefix() {
        // Test that --schema-remote-base strips URL prefix when mapping to local
        // Fixture has schema URL like https://ucp.dev/schemas/shopping/checkout.json
        // With remote base, this maps to tests/fixtures/compose/schemas/shopping/checkout.json
        let dir = TempDir::new().unwrap();
        let payload = write_temp_file(
            &dir,
            "payload.json",
            r#"{
                "ucp": {
                    "capabilities": {
                        "dev.ucp.shopping.checkout": [{
                            "version": "2026-01-11",
                            "schema": "https://ucp.dev/versioned/schemas/shopping/checkout.json"
                        }]
                    },
                    "payment_handlers": {}
                },
                "id": "123",
                "line_items": [],
                "status": "incomplete",
                "currency": "USD",
                "totals": [],
                "links": []
            }"#,
        );

        // Without remote base, would try to extract /versioned/schemas/... path
        // With remote base https://ucp.dev/versioned, strips prefix leaving /schemas/...
        cmd()
            .args([
                "validate",
                payload.to_str().unwrap(),
                "--schema-local-base",
                "tests/fixtures/compose",
                "--schema-remote-base",
                "https://ucp.dev/versioned",
                "--response",
                "--op",
                "read",
                "--strict=false",
            ])
            .assert()
            .success()
            .stdout(predicate::str::contains("Valid"));
    }

    #[test]
    fn schema_remote_base_requires_local_base() {
        // --schema-remote-base requires --schema-local-base
        cmd()
            .args([
                "validate",
                "tests/fixtures/compose/response_checkout_only.json",
                "--schema-remote-base",
                "https://ucp.dev/draft",
                "--op",
                "read",
            ])
            .assert()
            .failure()
            .stderr(predicate::str::contains("schema-local-base"));
    }

    #[test]
    fn not_self_describing_requires_schema() {
        let dir = TempDir::new().unwrap();
        // Payload without UCP metadata
        let payload = write_temp_file(&dir, "payload.json", r#"{"name": "test"}"#);

        cmd()
            .args(["validate", payload.to_str().unwrap(), "--op", "create"])
            .assert()
            .code(2)
            .stderr(predicate::str::contains("cannot infer direction"));
    }

    #[test]
    fn explicit_schema_overrides_self_describing() {
        let dir = TempDir::new().unwrap();
        // Self-describing payload but we override with explicit schema
        let schema = write_temp_file(
            &dir,
            "schema.json",
            r#"{"type": "object", "properties": {"custom": {"type": "string"}}}"#,
        );
        let payload = write_temp_file(&dir, "payload.json", r#"{"custom": "value"}"#);

        cmd()
            .args([
                "validate",
                payload.to_str().unwrap(),
                "--schema",
                schema.to_str().unwrap(),
                "--request",
                "--op",
                "create",
            ])
            .assert()
            .success()
            .stdout(predicate::str::contains("Valid"));
    }

    #[test]
    fn missing_schema_base_error() {
        // Self-describing payload without --schema-local-base, no network (simulated by invalid path)
        cmd()
            .args([
                "validate",
                "tests/fixtures/compose/response_checkout_only.json",
                "--schema-local-base",
                "/nonexistent/schemas",
                "--response",
                "--op",
                "read",
            ])
            .assert()
            .failure()
            .stderr(predicate::str::contains("failed to fetch schema"));
    }

    #[test]
    fn empty_capabilities_error() {
        let dir = TempDir::new().unwrap();
        let payload = write_temp_file(
            &dir,
            "payload.json",
            r#"{
                "ucp": {
                    "capabilities": {}
                },
                "id": "123"
            }"#,
        );

        cmd()
            .args([
                "validate",
                payload.to_str().unwrap(),
                "--schema-local-base",
                "tests/fixtures/compose",
                "--response",
                "--op",
                "read",
            ])
            .assert()
            .code(2)
            .stderr(predicate::str::contains("no capabilities"));
    }

    #[test]
    fn unknown_parent_error() {
        let dir = TempDir::new().unwrap();
        // Extension references parent not in capabilities (but has a root)
        let payload = write_temp_file(
            &dir,
            "payload.json",
            r#"{
                "ucp": {
                    "capabilities": {
                        "dev.ucp.shopping.checkout": [{
                            "version": "2026-01-11",
                            "schema": "https://ucp.dev/schemas/shopping/checkout.json"
                        }],
                        "dev.ucp.shopping.discount": [{
                            "version": "2026-01-11",
                            "schema": "https://ucp.dev/schemas/shopping/discount.json",
                            "extends": "dev.ucp.shopping.nonexistent"
                        }]
                    }
                }
            }"#,
        );

        cmd()
            .args([
                "validate",
                payload.to_str().unwrap(),
                "--schema-local-base",
                "tests/fixtures/compose",
                "--response",
                "--op",
                "read",
            ])
            .assert()
            .code(2)
            .stderr(predicate::str::contains("unknown parent"));
    }

    #[test]
    fn json_output_compose_error() {
        let dir = TempDir::new().unwrap();
        let payload = write_temp_file(
            &dir,
            "payload.json",
            r#"{
                "ucp": {
                    "capabilities": {}
                }
            }"#,
        );

        cmd()
            .args([
                "validate",
                payload.to_str().unwrap(),
                "--schema-local-base",
                "tests/fixtures/compose",
                "--response",
                "--op",
                "read",
                "--json",
            ])
            .assert()
            .code(2)
            .stdout(predicate::str::contains(r#""valid":false"#))
            .stdout(predicate::str::contains(r#""errors":"#));
    }
}
