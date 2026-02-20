//! Schema linting - static analysis of UCP schema files.
//!
//! Validates schema files for:
//! - JSON syntax errors
//! - Broken $ref references (file not found, anchor not found)
//! - Invalid ucp_* annotation values

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::loader::{load_schema, navigate_fragment};
use crate::types::{
    is_valid_schema_transition, json_type_name, Visibility, UCP_ANNOTATIONS, VALID_OPERATIONS,
};

/// Severity level for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

/// A single diagnostic message from linting.
#[derive(Debug, Clone, Serialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: String,
    pub file: PathBuf,
    /// JSON path to the issue (e.g., "/properties/id/ucp_request")
    pub path: String,
    pub message: String,
}

/// Result of linting a single file.
#[derive(Debug, Clone, Serialize)]
pub struct FileResult {
    pub file: PathBuf,
    pub status: FileStatus,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
}

/// Status of a linted file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FileStatus {
    Ok,
    Error,
    Warning,
}

/// Result of linting a directory or set of files.
#[derive(Debug, Clone, Serialize)]
pub struct LintResult {
    pub path: PathBuf,
    pub files_checked: usize,
    pub passed: usize,
    pub failed: usize,
    pub errors: usize,
    pub warnings: usize,
    pub results: Vec<FileResult>,
}

impl LintResult {
    /// Returns true if all files passed (no errors).
    pub fn is_ok(&self) -> bool {
        self.errors == 0
    }
}

/// Lint a file or directory.
///
/// If path is a directory, recursively finds all .json files.
/// If `strict` is true, warnings are treated as errors.
/// Returns aggregated results for all files.
pub fn lint(path: &Path, strict: bool) -> LintResult {
    let files = collect_schema_files(path);
    let mut results = Vec::new();
    let mut total_errors = 0;
    let mut total_warnings = 0;

    for file in &files {
        let file_result = lint_file(file, path);
        let file_errors = file_result
            .diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .count();
        let file_warnings = file_result
            .diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Warning)
            .count();

        total_errors += file_errors;
        total_warnings += file_warnings;
        results.push(file_result);
    }

    let failed = results
        .iter()
        .filter(|r| {
            if strict {
                r.status != FileStatus::Ok
            } else {
                r.status == FileStatus::Error
            }
        })
        .count();

    LintResult {
        path: path.to_path_buf(),
        files_checked: files.len(),
        passed: files.len() - failed,
        failed,
        errors: total_errors,
        warnings: total_warnings,
        results,
    }
}

/// Lint a single schema file.
pub fn lint_file(file: &Path, base_path: &Path) -> FileResult {
    let mut diagnostics = Vec::new();

    // Try to load the file (checks syntax)
    let schema = match load_schema(file) {
        Ok(s) => s,
        Err(e) => {
            diagnostics.push(Diagnostic {
                severity: Severity::Error,
                code: "E001".to_string(),
                file: file.to_path_buf(),
                path: "/".to_string(),
                message: format!("syntax error: {}", e),
            });
            return FileResult {
                file: file.strip_prefix(base_path).unwrap_or(file).to_path_buf(),
                status: FileStatus::Error,
                diagnostics,
            };
        }
    };

    // Check $refs
    let file_dir = file.parent().unwrap_or(Path::new("."));
    check_refs(&schema, file, file_dir, "", &schema, &mut diagnostics);

    // Check ucp_* annotations
    check_annotations(&schema, file, "", &mut diagnostics);

    // Check for missing $id (warning)
    if schema.get("$id").is_none() {
        diagnostics.push(Diagnostic {
            severity: Severity::Warning,
            code: "W002".to_string(),
            file: file.to_path_buf(),
            path: "/".to_string(),
            message: "schema missing $id field".to_string(),
        });
    }

    let has_errors = diagnostics.iter().any(|d| d.severity == Severity::Error);
    let has_warnings = diagnostics.iter().any(|d| d.severity == Severity::Warning);

    let status = if has_errors {
        FileStatus::Error
    } else if has_warnings {
        FileStatus::Warning
    } else {
        FileStatus::Ok
    };

    FileResult {
        file: file.strip_prefix(base_path).unwrap_or(file).to_path_buf(),
        status,
        diagnostics,
    }
}

/// Recursively check $ref values in a schema.
fn check_refs(
    value: &Value,
    file: &Path,
    file_dir: &Path,
    path: &str,
    root: &Value,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(ref_val)) = map.get("$ref") {
                check_single_ref(ref_val, file, file_dir, path, root, diagnostics);
            }

            for (key, val) in map {
                let child_path = format!("{}/{}", path, key);
                check_refs(val, file, file_dir, &child_path, root, diagnostics);
            }
        }
        Value::Array(arr) => {
            for (i, item) in arr.iter().enumerate() {
                let child_path = format!("{}/{}", path, i);
                check_refs(item, file, file_dir, &child_path, root, diagnostics);
            }
        }
        _ => {}
    }
}

/// Check a single $ref value.
fn check_single_ref(
    ref_val: &str,
    file: &Path,
    file_dir: &Path,
    path: &str,
    root: &Value,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // External URLs can't be validated locally - skip silently
    if ref_val.starts_with("http://") || ref_val.starts_with("https://") {
        return;
    }

    if ref_val.starts_with('#') {
        // Internal reference - check anchor resolves
        if ref_val != "#" && navigate_fragment(root, ref_val).is_err() {
            diagnostics.push(Diagnostic {
                severity: Severity::Error,
                code: "E003".to_string(),
                file: file.to_path_buf(),
                path: path.to_string(),
                message: format!("anchor not found: {}", ref_val),
            });
        }
        return;
    }

    // File reference (possibly with anchor)
    let (file_part, fragment) = match ref_val.find('#') {
        Some(idx) => (&ref_val[..idx], Some(&ref_val[idx..])),
        None => (ref_val, None),
    };

    let ref_path = file_dir.join(file_part);
    if !ref_path.exists() {
        diagnostics.push(Diagnostic {
            severity: Severity::Error,
            code: "E002".to_string(),
            file: file.to_path_buf(),
            path: path.to_string(),
            message: format!("file not found: {}", file_part),
        });
        return;
    }

    // If there's a fragment, check it resolves in the referenced file
    if let Some(frag) = fragment {
        if frag != "#" {
            match load_schema(&ref_path) {
                Ok(ref_schema) => {
                    if navigate_fragment(&ref_schema, frag).is_err() {
                        diagnostics.push(Diagnostic {
                            severity: Severity::Error,
                            code: "E003".to_string(),
                            file: file.to_path_buf(),
                            path: path.to_string(),
                            message: format!("anchor not found in {}: {}", file_part, frag),
                        });
                    }
                }
                Err(_) => {
                    // If we can't load the ref'd file, that's already an error
                    // from a different check, so don't duplicate
                }
            }
        }
    }
}

/// Recursively check ucp_* annotation values.
fn check_annotations(value: &Value, file: &Path, path: &str, diagnostics: &mut Vec<Diagnostic>) {
    if let Value::Object(map) = value {
        // Check all UCP annotations
        for &annotation_key in UCP_ANNOTATIONS {
            if let Some(annotation) = map.get(annotation_key) {
                check_annotation_value(annotation, annotation_key, file, path, diagnostics);
            }
        }

        // Recurse
        for (key, val) in map {
            let child_path = format!("{}/{}", path, key);
            check_annotations(val, file, &child_path, diagnostics);
        }
    } else if let Value::Array(arr) = value {
        for (i, item) in arr.iter().enumerate() {
            let child_path = format!("{}/{}", path, i);
            check_annotations(item, file, &child_path, diagnostics);
        }
    }
}

/// Check a single ucp_* annotation value is valid.
fn check_annotation_value(
    annotation: &Value,
    key: &str,
    file: &Path,
    path: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let annotation_path = format!("{}/{}", path, key);

    match annotation {
        Value::String(s) => {
            if Visibility::parse(s).is_none() {
                diagnostics.push(Diagnostic {
                    severity: Severity::Error,
                    code: "E004".to_string(),
                    file: file.to_path_buf(),
                    path: annotation_path,
                    message: format!(
                        "invalid {} value \"{}\": expected omit, required, or optional",
                        key, s
                    ),
                });
            }
        }
        Value::Object(map) => {
            // Object form: { "create": "omit", "update": "required" }
            for (op, val) in map {
                let op_path = format!("{}/{}", annotation_path, op);

                // Handle shorthand transition key
                if op == "transition" {
                    check_transition_object(val, key, file, &op_path, diagnostics);
                    continue;
                }

                // Warn on unknown operations
                if !VALID_OPERATIONS.contains(&op.as_str()) {
                    diagnostics.push(Diagnostic {
                        severity: Severity::Warning,
                        code: "W003".to_string(),
                        file: file.to_path_buf(),
                        path: op_path.clone(),
                        message: format!(
                            "unknown operation \"{}\": expected {}",
                            op,
                            VALID_OPERATIONS.join(", ")
                        ),
                    });
                }

                // Check value is valid
                match val {
                    Value::String(s) => {
                        if Visibility::parse(s).is_none() {
                            diagnostics.push(Diagnostic {
                                severity: Severity::Error,
                                code: "E004".to_string(),
                                file: file.to_path_buf(),
                                path: op_path,
                                message: format!(
                                    "invalid {} value \"{}\": expected omit, required, or optional",
                                    key, s
                                ),
                            });
                        }
                    }
                    Value::Object(obj) => {
                        // Per-operation transition: { "update": { "transition": { ... } } }
                        if let Some(t) = obj.get("transition") {
                            check_transition_object(t, key, file, &op_path, diagnostics);
                        } else {
                            diagnostics.push(Diagnostic {
                                severity: Severity::Error,
                                code: "E005".to_string(),
                                file: file.to_path_buf(),
                                path: op_path,
                                message: format!(
                                    "invalid {} value type: expected string or transition object, got {}",
                                    key,
                                    json_type_name(val)
                                ),
                            });
                        }
                    }
                    _ => {
                        diagnostics.push(Diagnostic {
                            severity: Severity::Error,
                            code: "E005".to_string(),
                            file: file.to_path_buf(),
                            path: op_path,
                            message: format!(
                                "invalid {} value type: expected string or transition object, got {}",
                                key,
                                json_type_name(val)
                            ),
                        });
                    }
                }
            }
        }
        other => {
            diagnostics.push(Diagnostic {
                severity: Severity::Error,
                code: "E005".to_string(),
                file: file.to_path_buf(),
                path: annotation_path,
                message: format!(
                    "invalid {} type: expected string or object, got {}",
                    key,
                    json_type_name(other)
                ),
            });
        }
    }
}

/// Validate a schema transition object { "from", "to", "description" }.
fn check_transition_object(
    value: &Value,
    key: &str,
    file: &Path,
    path: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(obj) = value.as_object() else {
        diagnostics.push(Diagnostic {
            severity: Severity::Error,
            code: "E005".to_string(),
            file: file.to_path_buf(),
            path: path.to_string(),
            message: format!(
                "invalid {} transition: expected object, got {}",
                key,
                json_type_name(value)
            ),
        });
        return;
    };

    let from = obj.get("from").and_then(|v| v.as_str()).unwrap_or("");
    let to = obj.get("to").and_then(|v| v.as_str()).unwrap_or("");
    let description = obj
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if description.is_empty() {
        diagnostics.push(Diagnostic {
            severity: Severity::Error,
            code: "E004".to_string(),
            file: file.to_path_buf(),
            path: path.to_string(),
            message: format!(
                "invalid {} transition: missing required field \"description\"",
                key
            ),
        });
    }

    if !is_valid_schema_transition(from, to) {
        diagnostics.push(Diagnostic {
            severity: Severity::Error,
            code: "E004".to_string(),
            file: file.to_path_buf(),
            path: path.to_string(),
            message: format!(
                "invalid {} schema transition: \"from\" ({}) and \"to\" ({}) must be distinct visibility values (omit, required, optional)",
                key, from, to
            ),
        });
    }
}

/// Collect all .json files in a path (file or directory).
fn collect_schema_files(path: &Path) -> Vec<PathBuf> {
    if path.is_file() {
        if path.extension().map(|e| e == "json").unwrap_or(false) {
            return vec![path.to_path_buf()];
        }
        return vec![];
    }

    let mut files = Vec::new();
    collect_files_recursive(path, &mut files);
    files.sort();
    files
}

fn collect_files_recursive(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files_recursive(&path, files);
        } else if path.extension().map(|e| e == "json").unwrap_or(false) {
            files.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::{tempdir, NamedTempFile};

    #[test]
    fn lint_valid_schema() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{
            "$id": "https://example.com/test.json",
            "type": "object",
            "properties": {{
                "id": {{ "type": "string" }}
            }}
        }}"#
        )
        .unwrap();

        let result = lint_file(file.path(), file.path().parent().unwrap());
        assert_eq!(result.status, FileStatus::Ok);
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn lint_invalid_json_syntax() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "{{ not valid json }}").unwrap();

        let result = lint_file(file.path(), file.path().parent().unwrap());
        assert_eq!(result.status, FileStatus::Error);
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, "E001");
    }

    #[test]
    fn lint_broken_internal_ref() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r##"{{
            "$id": "https://example.com/test.json",
            "type": "object",
            "properties": {{
                "data": {{ "$ref": "#/$defs/missing" }}
            }}
        }}"##
        )
        .unwrap();

        let result = lint_file(file.path(), file.path().parent().unwrap());
        assert_eq!(result.status, FileStatus::Error);
        assert!(result.diagnostics.iter().any(|d| d.code == "E003"));
    }

    #[test]
    fn lint_broken_file_ref() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{
            "$id": "https://example.com/test.json",
            "properties": {{
                "data": {{ "$ref": "nonexistent.json" }}
            }}
        }}"#
        )
        .unwrap();

        let result = lint_file(file.path(), file.path().parent().unwrap());
        assert_eq!(result.status, FileStatus::Error);
        assert!(result.diagnostics.iter().any(|d| d.code == "E002"));
    }

    #[test]
    fn lint_invalid_ucp_request_value() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{
            "$id": "https://example.com/test.json",
            "properties": {{
                "id": {{
                    "type": "string",
                    "ucp_request": "invalid_value"
                }}
            }}
        }}"#
        )
        .unwrap();

        let result = lint_file(file.path(), file.path().parent().unwrap());
        assert_eq!(result.status, FileStatus::Error);
        assert!(result.diagnostics.iter().any(|d| d.code == "E004"));
    }

    #[test]
    fn lint_valid_ucp_annotations() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{
            "$id": "https://example.com/test.json",
            "properties": {{
                "id": {{
                    "type": "string",
                    "ucp_request": {{
                        "create": "omit",
                        "update": "required"
                    }},
                    "ucp_response": "omit"
                }}
            }}
        }}"#
        )
        .unwrap();

        let result = lint_file(file.path(), file.path().parent().unwrap());
        assert_eq!(result.status, FileStatus::Ok);
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn lint_valid_schema_transition_object() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{
            "$id": "https://example.com/test.json",
            "properties": {{
                "legacy_id": {{
                    "type": "string",
                    "ucp_request": {{
                        "update": {{
                            "transition": {{
                                "from": "required",
                                "to": "omit",
                                "description": "Will be removed in v2."
                            }}
                        }}
                    }}
                }}
            }}
        }}"#
        )
        .unwrap();

        let result = lint_file(file.path(), file.path().parent().unwrap());
        assert_eq!(result.status, FileStatus::Ok);
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn lint_invalid_schema_transition() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{
            "$id": "https://example.com/test.json",
            "properties": {{
                "x": {{
                    "type": "string",
                    "ucp_request": {{
                        "transition": {{
                            "from": "required",
                            "to": "required",
                            "description": "from and to must be distinct"
                        }}
                    }}
                }}
            }}
        }}"#
        )
        .unwrap();

        let result = lint_file(file.path(), file.path().parent().unwrap());
        assert_eq!(result.status, FileStatus::Error);
        assert!(result.diagnostics.iter().any(|d| d.code == "E004"));
    }

    #[test]
    fn lint_schema_transition_missing_description() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{
            "$id": "https://example.com/test.json",
            "properties": {{
                "x": {{
                    "type": "string",
                    "ucp_request": {{
                        "transition": {{
                            "from": "required",
                            "to": "omit"
                        }}
                    }}
                }}
            }}
        }}"#
        )
        .unwrap();

        let result = lint_file(file.path(), file.path().parent().unwrap());
        assert_eq!(result.status, FileStatus::Error);
        assert!(result
            .diagnostics
            .iter()
            .any(|d| d.code == "E004" && d.message.contains("description")));
    }

    #[test]
    fn lint_invalid_ucp_type() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{
            "$id": "https://example.com/test.json",
            "properties": {{
                "id": {{
                    "type": "string",
                    "ucp_request": 123
                }}
            }}
        }}"#
        )
        .unwrap();

        let result = lint_file(file.path(), file.path().parent().unwrap());
        assert_eq!(result.status, FileStatus::Error);
        assert!(result.diagnostics.iter().any(|d| d.code == "E005"));
    }

    #[test]
    fn lint_missing_id_warning() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{
            "type": "object",
            "properties": {{}}
        }}"#
        )
        .unwrap();

        let result = lint_file(file.path(), file.path().parent().unwrap());
        assert_eq!(result.status, FileStatus::Warning);
        assert!(result.diagnostics.iter().any(|d| d.code == "W002"));
    }

    #[test]
    fn lint_directory() {
        let dir = tempdir().unwrap();

        // Create valid schema
        let valid_path = dir.path().join("valid.json");
        std::fs::write(
            &valid_path,
            r#"{"$id": "https://example.com/valid.json", "type": "object"}"#,
        )
        .unwrap();

        // Create invalid schema
        let invalid_path = dir.path().join("invalid.json");
        std::fs::write(&invalid_path, "{ not json }").unwrap();

        let result = lint(dir.path(), false);
        assert_eq!(result.files_checked, 2);
        assert_eq!(result.passed, 1);
        assert_eq!(result.failed, 1);
        assert!(!result.is_ok());
    }

    #[test]
    fn lint_strict_mode() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.json");
        // Schema with warning only (missing $id)
        std::fs::write(&file_path, r#"{"type": "object"}"#).unwrap();

        // Non-strict: warnings don't cause failure
        let result = lint(&file_path, false);
        assert_eq!(result.files_checked, 1);
        assert_eq!(result.passed, 1);
        assert_eq!(result.failed, 0);

        // Strict: warnings cause failure
        let result = lint(&file_path, true);
        assert_eq!(result.files_checked, 1);
        assert_eq!(result.passed, 0);
        assert_eq!(result.failed, 1);
    }

    #[test]
    fn lint_valid_ref_with_anchor() {
        let dir = tempdir().unwrap();

        // Create referenced schema with $defs
        let ref_path = dir.path().join("types.json");
        std::fs::write(
            &ref_path,
            r#"{"$id": "https://example.com/types.json", "$defs": {"thing": {"type": "string"}}}"#,
        )
        .unwrap();

        // Create schema that references it
        let main_path = dir.path().join("main.json");
        std::fs::write(
            &main_path,
            r#"{"$id": "https://example.com/main.json", "properties": {"x": {"$ref": "types.json#/$defs/thing"}}}"#,
        )
        .unwrap();

        let result = lint_file(&main_path, dir.path());
        assert_eq!(result.status, FileStatus::Ok);
    }

    #[test]
    fn lint_broken_ref_anchor() {
        let dir = tempdir().unwrap();

        // Create referenced schema without the expected $def
        let ref_path = dir.path().join("types.json");
        std::fs::write(
            &ref_path,
            r#"{"$id": "https://example.com/types.json", "$defs": {}}"#,
        )
        .unwrap();

        // Create schema that references missing anchor
        let main_path = dir.path().join("main.json");
        std::fs::write(
            &main_path,
            r#"{"$id": "https://example.com/main.json", "properties": {"x": {"$ref": "types.json#/$defs/missing"}}}"#,
        )
        .unwrap();

        let result = lint_file(&main_path, dir.path());
        assert_eq!(result.status, FileStatus::Error);
        assert!(result.diagnostics.iter().any(|d| d.code == "E003"));
    }
}
