//! Active-only transitive extension selection must agree across the crawl,
//! helper hoisting and static object/container composition.
use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::{json, Map, Value};
use ucp_schema::{
    generate_types, CodegenError, ComposeError, GenerateTypesOptions, TypesBundleDoc,
};

const ROOT: &str = "dev.ucp.test.checkout";
const PARENT: &str = "dev.ucp.test.parent";
const OTHER_PARENT: &str = "dev.ucp.test.other_parent";
const CHILD: &str = "dev.ucp.test.child";
const OTHER_ROOT: &str = "dev.ucp.test.other_root";

struct Fixtures {
    dir: tempfile::TempDir,
    schemas: PathBuf,
    entries: BTreeMap<String, (String, Option<Value>)>,
}

impl Fixtures {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let schemas = dir.path().join("schemas");
        std::fs::create_dir_all(&schemas).unwrap();
        Self {
            dir,
            schemas,
            entries: BTreeMap::new(),
        }
    }

    fn write(&self, file: &str, schema: Value) {
        let path = self.schemas.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec_pretty(&schema).unwrap()).unwrap();
    }

    fn capability(&mut self, name: &str, file: &str, extends: Option<Value>, mut schema: Value) {
        schema["name"] = json!(name);
        self.write(file, schema);
        self.entries.insert(name.into(), (file.into(), extends));
    }

    fn object_root(&mut self, name: &str, file: &str) {
        self.capability(name, file, None, json!({
            "type": "object", "required": ["id"],
            "properties": { "id": {"type": "string", "ucp_request": {"create": "omit", "update": "required"}} }
        }));
    }

    fn extension(&mut self, name: &str, file: &str, extends: Value, defs: Value) {
        self.capability(name, file, Some(extends), json!({"$defs": defs}));
    }

    fn profile_options(&self, active: &[&str]) -> GenerateTypesOptions {
        let mut capabilities = Map::new();
        // Intentionally reverse declaration order: graph order is not profile order.
        for &name in active.iter().rev() {
            let (file, extends) = &self.entries[name];
            let mut entry = json!({"version": "2026-11-17", "schema": self.schemas.join(file)});
            if let Some(extends) = extends {
                entry["extends"] = extends.clone();
            }
            capabilities.insert(name.into(), json!([entry]));
        }
        let path = self.dir.path().join("discovery.json");
        std::fs::write(
            &path,
            json!({"ucp": {"capabilities": capabilities}}).to_string(),
        )
        .unwrap();
        GenerateTypesOptions::new().profile(path.to_string_lossy())
    }

    fn directory_options(&self, roots: &[&str], extensions: &[&str]) -> GenerateTypesOptions {
        GenerateTypesOptions::new()
            .schema_dir(&self.schemas)
            .capabilities(roots.iter().copied())
            .extensions(extensions.iter().copied())
    }
}

fn assert_closed(bundle: &TypesBundleDoc) {
    fn visit(value: &Value, defs: &BTreeMap<String, Value>) {
        match value {
            Value::Object(obj) => {
                if let Some(r) = obj.get("$ref").and_then(Value::as_str) {
                    assert!(
                        !obj.contains_key("properties"),
                        "hybrid reference and properties: {value}"
                    );
                    let target = r
                        .strip_prefix("#/$defs/")
                        .unwrap_or_else(|| panic!("non-local ref {r}"));
                    assert!(defs.contains_key(target), "dangling ref {r}");
                }
                assert!(
                    !obj.contains_key("ucp_request") && !obj.contains_key("ucp_response"),
                    "unsliced annotation: {value}"
                );
                for (key, child) in obj {
                    if !["const", "enum", "default", "examples"].contains(&key.as_str()) {
                        visit(child, defs);
                    }
                }
            }
            Value::Array(arr) => arr.iter().for_each(|v| visit(v, defs)),
            _ => {}
        }
    }
    for (key, schema) in &bundle.defs {
        assert_eq!(
            schema.get("title").and_then(Value::as_str),
            Some(key.as_str())
        );
        visit(schema, &bundle.defs);
    }
}

fn object_chain() -> Fixtures {
    let mut f = Fixtures::new();
    f.object_root(ROOT, "checkout.json");
    // Child sorts before parent by file path, exposing accidental file-order merges.
    f.extension(PARENT, "z_parent.json", json!(ROOT), json!({
        ROOT: {"type": "object", "properties": {"parent": {"type": "string"}, "layer": {"type": "string", "description": "parent layer"}}}
    }));
    f.extension(CHILD, "a_child.json", json!([PARENT, OTHER_PARENT]), json!({
        "mixin_alias": {"$ref": "#/$defs/mixin"},
        "mixin": {"type": "object", "properties": {
            "child": {"type": "boolean"}, "layer": {"type": "string", "description": "child layer"},
            "detail": {"$ref": "#/$defs/detail", "ucp_request": {"create": "required", "update": "optional"}}
        }},
        "detail": {"type": "object", "properties": {"leaf": {"$ref": "types/leaf.json", "ucp_request": "optional"}}},
        "inactive_helper": {"$ref": "missing_inactive.json"},
        PARENT: {"$ref": "#/$defs/mixin_alias"},
        OTHER_PARENT: {"properties": {"wrong": {"$ref": "#/$defs/inactive_helper"}}}
    }));
    f.write(
        "types/leaf.json",
        json!({"type": "object", "properties": {
            "client": {"type": "string", "ucp_response": "omit"},
            "server": {"type": "string", "ucp_request": "omit"}
        }}),
    );
    f
}

#[test]
fn multilevel_objects_profile_and_directory_keep_transitive_helpers_and_directional_slices() {
    let f = object_chain();
    let directory = generate_types(&f.directory_options(&[ROOT], &[PARENT, CHILD])).unwrap();
    let profile = generate_types(&f.profile_options(&[ROOT, PARENT, CHILD])).unwrap();
    assert_eq!(directory.defs, profile.defs);
    assert_closed(&directory);
    for name in ["Checkout", "CheckoutCreateRequest", "CheckoutUpdateRequest"] {
        let props = &directory.defs[name]["properties"];
        assert_eq!(props["parent"]["type"], "string");
        assert_eq!(props["child"]["type"], "boolean");
        assert_eq!(
            props["layer"]["description"], "child layer",
            "child must merge after parent"
        );
        assert!(props.get("wrong").is_none());
    }
    assert_eq!(
        directory.defs["Checkout"]["properties"]["detail"]["$ref"],
        "#/$defs/Detail"
    );
    assert_eq!(
        directory.defs["CheckoutCreateRequest"]["properties"]["detail"]["$ref"],
        "#/$defs/DetailCreateRequest"
    );
    assert_eq!(
        directory.defs["DetailCreateRequest"]["properties"]["leaf"]["$ref"],
        "#/$defs/LeafCreateRequest"
    );
    assert!(directory.defs["LeafCreateRequest"]["properties"]
        .get("server")
        .is_none());
    assert!(directory.defs["Leaf"]["properties"].get("client").is_none());
    assert!(!directory.defs.contains_key("InactiveHelper"));
    assert!(!directory.defs.contains_key("MixinAlias"));
}

#[test]
fn inactive_parent_is_not_activated_by_directory_presence_or_external_ref() {
    let mut f = object_chain();
    f.extension(
        OTHER_PARENT,
        "other_parent.json",
        json!(ROOT),
        json!({
            ROOT: {"properties": {"inactive": {"type": "string"}}}
        }),
    );
    // An external reference may load the parent schema, but must never enable its graph edge.
    f.extension(CHILD, "a_child.json", json!(PARENT), json!({
        PARENT: {"allOf": [{"$ref": "z_parent.json"}], "properties": {"child": {"type": "boolean"}}}
    }));
    let bundle = generate_types(&f.directory_options(&[ROOT], &[CHILD])).unwrap();
    assert_closed(&bundle);
    let props = &bundle.defs["Checkout"]["properties"];
    assert!(props.get("child").is_none());
    assert!(props.get("parent").is_none());
    assert!(props.get("inactive").is_none());
    // The fully selected chain still works even with the inactive alternative present on disk.
    let selected = generate_types(&f.directory_options(&[ROOT], &[PARENT, CHILD])).unwrap();
    assert_eq!(
        selected.defs["Checkout"]["properties"]["child"]["type"],
        "boolean"
    );
}

#[test]
fn root_override_wins_and_prunes_parent_only_external_refs_in_both_modes() {
    let mut f = object_chain();
    f.extension(
        CHILD,
        "a_child.json",
        json!(PARENT),
        json!({
            ROOT: {"properties": {"chosen": {"type": "boolean"}}},
            PARENT: {"properties": {"chosen": {"$ref": "#/$defs/unused"}}},
            "unused": {"$ref": "missing_override_loser.json"}
        }),
    );
    for options in [
        f.directory_options(&[ROOT], &[PARENT, CHILD]),
        f.profile_options(&[ROOT, PARENT, CHILD]),
    ] {
        let bundle = generate_types(&options).unwrap();
        assert_closed(&bundle);
        assert_eq!(
            bundle.defs["Checkout"]["properties"]["chosen"]["type"],
            "boolean"
        );
        assert!(!bundle.defs.contains_key("Unused"));
    }
}

#[test]
fn ambiguous_parent_diagnostics_are_sorted_and_order_independent() {
    let mut f = Fixtures::new();
    f.object_root(ROOT, "checkout.json");
    for (parent, file) in [(PARENT, "parent.json"), (OTHER_PARENT, "other_parent.json")] {
        f.extension(parent, file, json!(ROOT), json!({ROOT: {"properties": {}}}));
    }
    let mut errors = Vec::new();
    for parents in [json!([PARENT, OTHER_PARENT]), json!([OTHER_PARENT, PARENT])] {
        // Also reverse the source $defs insertion order, not merely profile extends.
        let mut defs = Map::new();
        for name in parents.as_array().unwrap() {
            defs.insert(
                name.as_str().unwrap().into(),
                json!({"properties": {"choice": {"type": "boolean"}}}),
            );
        }
        f.extension(CHILD, "child.json", parents, Value::Object(defs));
        for options in [
            f.directory_options(&[ROOT], &[CHILD, OTHER_PARENT, PARENT]),
            f.profile_options(&[ROOT, CHILD, OTHER_PARENT, PARENT]),
        ] {
            let err = generate_types(&options).unwrap_err();
            match &err {
                CodegenError::ComposeError(ComposeError::AmbiguousParentDefs {
                    extension,
                    root,
                    parents,
                }) => {
                    assert_eq!(extension, CHILD);
                    assert_eq!(root, ROOT);
                    assert_eq!(parents, &format!("{OTHER_PARENT}, {PARENT}"));
                }
                other => panic!("wrong error: {other}"),
            }
            errors.push(err.to_string());
        }
    }
    assert!(errors.iter().all(|e| e == &errors[0]));
}

#[test]
fn reachable_profile_extension_without_root_or_declared_parent_entry_is_missing_def() {
    let mut f = object_chain();
    f.extension(
        CHILD,
        "a_child.json",
        json!(PARENT),
        json!({
            OTHER_PARENT: {"properties": {"wrong": {"$ref": "missing_should_not_be_fetched.json"}}}
        }),
    );
    let err = generate_types(&f.profile_options(&[ROOT, PARENT, CHILD])).unwrap_err();
    assert!(
        matches!(err, CodegenError::ComposeError(ComposeError::MissingDefEntry {extension, expected_key})
        if extension == CHILD && expected_key == ROOT)
    );
}

#[test]
fn profile_declared_edges_are_authoritative_for_transitive_extensions_and_multiple_roots() {
    let mut f = object_chain();
    f.object_root(OTHER_ROOT, "other_root.json");
    f.extension(
        CHILD,
        "a_child.json",
        json!(PARENT),
        json!({
            ROOT: {"properties": {"chosen": {"type": "boolean"}}},
            OTHER_ROOT: {"properties": {"leak": {"$ref": "#/$defs/unrelated_helper"}}},
            "unrelated_helper": {"$ref": "missing_unrelated_root.json"}
        }),
    );
    let bundle = generate_types(&f.profile_options(&[ROOT, OTHER_ROOT, PARENT, CHILD])).unwrap();
    assert_closed(&bundle);
    assert_eq!(
        bundle.defs["Checkout"]["properties"]["chosen"]["type"],
        "boolean"
    );
    assert!(bundle.defs["OtherRoot"]["properties"].get("leak").is_none());
    assert!(!bundle.defs.contains_key("UnrelatedHelper"));
}

#[test]
fn one_parent_can_connect_to_multiple_sdk_roots_without_touching_unrelated_root() {
    let mut f = Fixtures::new();
    const SECOND: &str = "dev.ucp.test.second";
    for (name, file) in [
        (ROOT, "checkout.json"),
        (SECOND, "second.json"),
        (OTHER_ROOT, "other_root.json"),
    ] {
        f.object_root(name, file);
    }
    f.extension(
        PARENT,
        "z_parent.json",
        json!([ROOT, SECOND]),
        json!({
            ROOT: {"properties": {"parent": {"type": "string"}}},
            SECOND: {"properties": {"parent": {"type": "string"}}}
        }),
    );
    f.extension(
        CHILD,
        "a_child.json",
        json!(PARENT),
        json!({
            PARENT: {"properties": {"child": {"type": "boolean"}}}
        }),
    );
    let directory =
        generate_types(&f.directory_options(&[ROOT, SECOND, OTHER_ROOT], &[PARENT, CHILD]))
            .unwrap();
    let profile =
        generate_types(&f.profile_options(&[ROOT, SECOND, OTHER_ROOT, PARENT, CHILD])).unwrap();
    assert_eq!(directory.defs, profile.defs);
    assert_closed(&directory);
    for name in ["Checkout", "Second"] {
        assert_eq!(
            directory.defs[name]["properties"]["child"]["type"],
            "boolean"
        );
        assert_eq!(
            directory.defs[name]["properties"]["parent"]["type"],
            "string"
        );
    }
    assert!(directory.defs["OtherRoot"]["properties"]
        .get("child")
        .is_none());
}

fn container_chain(operation_keys: bool) -> (Fixtures, &'static str, &'static str) {
    let mut f = Fixtures::new();
    let (base_key, added_key) = if operation_keys {
        ("lookup_response", "create_request")
    } else {
        ("manifest", "record")
    };
    let mut base_defs = Map::new();
    base_defs.insert(
        base_key.into(),
        json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}),
    );
    f.capability(ROOT, "feed.json", None, json!({"$defs": base_defs}));
    let mut parent_defs = Map::new();
    parent_defs.insert(added_key.into(), json!({"$ref": "#/$defs/parent_record"}));
    f.extension(PARENT, "z_parent.json", json!(ROOT), json!({
        ROOT: {"$defs": parent_defs},
        "parent_record": {"type": "object", "required": ["title"], "properties": {"title": {"type": "string"}}}
    }));
    let mut child_defs = Map::new();
    child_defs.insert(added_key.into(), json!({"$ref": "#/$defs/child_alias"}));
    f.extension(CHILD, "a_child.json", json!(PARENT), json!({
        PARENT: {"$defs": child_defs},
        "child_alias": {"$ref": "#/$defs/child_record"},
        "child_record": {"allOf": [
            {"$ref": "z_parent.json#/$defs/parent_record"},
            {"properties": {"eligible": {"type": "boolean"}, "detail": {"$ref": "types/detail.json"}}}
        ]}
    }));
    f.write(
        "types/detail.json",
        json!({"type": "object", "properties": {"code": {"type": "string"}}}),
    );
    (f, base_key, added_key)
}

#[test]
fn multilevel_container_operations_include_new_parent_operations_and_external_helpers() {
    let (f, _, _) = container_chain(true);
    let directory = generate_types(&f.directory_options(&[ROOT], &[PARENT, CHILD])).unwrap();
    let profile = generate_types(&f.profile_options(&[ROOT, PARENT, CHILD])).unwrap();
    assert_eq!(directory.defs, profile.defs);
    assert_closed(&directory);
    assert_eq!(
        directory.defs["FeedLookupResponse"]["properties"]["id"]["type"],
        "string"
    );
    let record = &directory.defs["FeedCreateRequest"];
    assert_eq!(record["properties"]["title"]["type"], "string");
    assert_eq!(record["properties"]["eligible"]["type"], "boolean");
    assert_eq!(record["properties"]["detail"]["$ref"], "#/$defs/Detail");
    assert!(record["required"]
        .as_array()
        .unwrap()
        .contains(&json!("title")));
}

#[test]
fn feed_like_arbitrary_container_keys_keep_parent_record_constraints() {
    let (f, _, _) = container_chain(false);
    let directory = generate_types(&f.directory_options(&[ROOT], &[PARENT, CHILD])).unwrap();
    let profile = generate_types(&f.profile_options(&[ROOT, PARENT, CHILD])).unwrap();
    assert_eq!(directory.defs, profile.defs);
    assert_closed(&directory);
    assert_eq!(
        directory.defs["Manifest"]["properties"]["id"]["type"],
        "string"
    );
    let record = &directory.defs["Record"];
    assert_eq!(record["properties"]["title"]["type"], "string");
    assert_eq!(record["properties"]["eligible"]["type"], "boolean");
    assert!(record["required"]
        .as_array()
        .unwrap()
        .contains(&json!("title")));
}

#[test]
fn legacy_container_oneof_allof_remains_supported_for_transitive_extensions() {
    let (mut f, _, _) = container_chain(true);
    f.extension(
        CHILD,
        "a_child.json",
        json!(PARENT),
        json!({
            PARENT: {"oneOf": [{"allOf": [
                {"$ref": "feed.json#/$defs/create_request"},
                {"properties": {"eligible": {"type": "boolean"}}}
            ]}]}
        }),
    );
    for options in [
        f.directory_options(&[ROOT], &[PARENT, CHILD]),
        f.profile_options(&[ROOT, PARENT, CHILD]),
    ] {
        let bundle = generate_types(&options).unwrap();
        assert_closed(&bundle);
        assert_eq!(
            bundle.defs["FeedCreateRequest"]["properties"]["eligible"]["type"],
            "boolean"
        );
        assert_eq!(
            bundle.defs["FeedCreateRequest"]["properties"]["title"]["type"],
            "string"
        );
    }
}

#[test]
fn cycles_terminate_with_and_without_an_active_root_exit() {
    let mut f = Fixtures::new();
    f.object_root(ROOT, "checkout.json");
    f.extension(
        PARENT,
        "z_parent.json",
        json!([ROOT, CHILD]),
        json!({
            ROOT: {"properties": {"parent": {"type": "string"}}},
            CHILD: {"properties": {"ignored": {"$ref": "missing_root_override.json"}}}
        }),
    );
    f.extension(
        CHILD,
        "a_child.json",
        json!(PARENT),
        json!({
            PARENT: {"properties": {"child": {"type": "boolean"}}}
        }),
    );
    for options in [
        f.directory_options(&[ROOT], &[PARENT, CHILD]),
        f.profile_options(&[ROOT, PARENT, CHILD]),
    ] {
        let bundle = generate_types(&options).unwrap();
        assert_closed(&bundle);
        assert_eq!(
            bundle.defs["Checkout"]["properties"]["parent"]["type"],
            "string"
        );
        assert_eq!(
            bundle.defs["Checkout"]["properties"]["child"]["type"],
            "boolean"
        );
    }
    // Disconnected active cycle has no active path to the SDK root and contributes nothing.
    f.extension(
        PARENT,
        "z_parent.json",
        json!(CHILD),
        json!({
            CHILD: {"properties": {"parent": {"$ref": "missing_disconnected.json"}}}
        }),
    );
    let bundle = generate_types(&f.directory_options(&[ROOT], &[PARENT, CHILD])).unwrap();
    assert_closed(&bundle);
    assert!(bundle.defs["Checkout"]["properties"]
        .get("parent")
        .is_none());
    assert!(bundle.defs["Checkout"]["properties"].get("child").is_none());
}

#[test]
fn local_mixin_cycles_do_not_recurse_forever() {
    let mut f = Fixtures::new();
    f.object_root(ROOT, "checkout.json");
    f.extension(PARENT, "parent.json", json!(ROOT), json!({
        ROOT: {"allOf": [{"$ref": "#/$defs/recursive_mixin"}, {"properties": {"parent": {"type": "string"}}}]},
        "recursive_mixin": {"$ref": "#/$defs/recursive_mixin"}
    }));
    let bundle = generate_types(&f.directory_options(&[ROOT], &[PARENT])).unwrap();
    assert_closed(&bundle);
    assert_eq!(
        bundle.defs["Checkout"]["properties"]["parent"]["type"],
        "string"
    );
}

#[test]
fn direct_siblings_keep_legacy_file_order_when_names_sort_differently() {
    let mut f = Fixtures::new();
    f.object_root(ROOT, "checkout.json");
    for (name, file, description) in [
        ("dev.ucp.test.z_first", "a_first.json", "first"),
        ("dev.ucp.test.a_last", "z_last.json", "last"),
    ] {
        f.extension(
            name,
            file,
            json!(ROOT),
            json!({ROOT: {"properties": {
                "label": {"type": "string", "description": description}
            }}}),
        );
    }
    for options in [
        f.directory_options(&[ROOT], &["dev.ucp.test.a_last", "dev.ucp.test.z_first"]),
        f.profile_options(&[ROOT, "dev.ucp.test.a_last", "dev.ucp.test.z_first"]),
    ] {
        let bundle = generate_types(&options).unwrap();
        assert_eq!(
            bundle.defs["Checkout"]["properties"]["label"]["description"],
            "last"
        );
    }
}

#[test]
fn active_parents_on_different_sdk_roots_are_still_ambiguous_without_root_override() {
    let mut f = Fixtures::new();
    f.object_root(ROOT, "checkout.json");
    f.object_root(OTHER_ROOT, "other_root.json");
    f.extension(
        PARENT,
        "parent.json",
        json!(ROOT),
        json!({ROOT: {"properties": {}}}),
    );
    f.extension(
        OTHER_PARENT,
        "other_parent.json",
        json!(OTHER_ROOT),
        json!({OTHER_ROOT: {"properties": {}}}),
    );
    f.extension(
        CHILD,
        "child.json",
        json!([PARENT, OTHER_PARENT]),
        json!({
            PARENT: {"properties": {"from_first": {"type": "boolean"}}},
            OTHER_PARENT: {"properties": {"from_second": {"type": "boolean"}}}
        }),
    );
    for options in [
        f.directory_options(&[ROOT, OTHER_ROOT], &[PARENT, OTHER_PARENT, CHILD]),
        f.profile_options(&[ROOT, OTHER_ROOT, PARENT, OTHER_PARENT, CHILD]),
    ] {
        assert!(matches!(
            generate_types(&options).unwrap_err(),
            CodegenError::ComposeError(ComposeError::AmbiguousParentDefs { .. })
        ));
    }
    f.extension(
        CHILD,
        "child.json",
        json!([PARENT, OTHER_PARENT]),
        json!({
            ROOT: {"properties": {"chosen_first": {"type": "boolean"}}},
            OTHER_ROOT: {"properties": {"chosen_second": {"type": "boolean"}}},
            PARENT: {"properties": {"ignored": {"$ref": "missing_loser.json"}}},
            OTHER_PARENT: {"properties": {}}
        }),
    );
    let bundle =
        generate_types(&f.profile_options(&[ROOT, OTHER_ROOT, PARENT, OTHER_PARENT, CHILD]))
            .unwrap();
    assert_closed(&bundle);
    assert_eq!(
        bundle.defs["Checkout"]["properties"]["chosen_first"]["type"],
        "boolean"
    );
    assert_eq!(
        bundle.defs["OtherRoot"]["properties"]["chosen_second"]["type"],
        "boolean"
    );
}

#[test]
fn arbitrary_container_shapes_slice_transitive_properties_without_overwriting_helpers() {
    let (mut f, _, _) = container_chain(false);
    f.extension(
        CHILD,
        "a_child.json",
        json!(PARENT),
        json!({
            PARENT: {"$defs": {"record": {"properties": {
                "client": {"type": "string", "ucp_response": "omit"},
                "server": {"type": "string", "ucp_request": "omit"},
                "detail": {"$ref": "types/detail.json"}
            }}}}
        }),
    );
    f.write(
        "types/detail.json",
        json!({"type": "object", "properties": {
            "client_detail": {"type": "string", "ucp_response": "omit"},
            "server_detail": {"type": "string", "ucp_request": "omit"}
        }}),
    );
    for options in [
        f.directory_options(&[ROOT], &[PARENT, CHILD]),
        f.profile_options(&[ROOT, PARENT, CHILD]),
    ] {
        let bundle = generate_types(&options).unwrap();
        assert_closed(&bundle);
        let response = &bundle.defs["Record"]["properties"];
        let request = &bundle.defs["RecordCreateRequest"]["properties"];
        assert_eq!(response["title"]["type"], "string");
        assert_eq!(request["title"]["type"], "string");
        assert!(response.get("client").is_none());
        assert!(request.get("server").is_none());
        assert_eq!(response["server"]["type"], "string");
        assert_eq!(request["client"]["type"], "string");
        assert_eq!(request["detail"]["$ref"], "#/$defs/DetailCreateRequest");
    }
}

#[test]
fn arbitrary_container_shape_names_are_qualified_per_root_including_parent_introduced_shapes() {
    let (mut f, _, _) = container_chain(false);
    f.capability(
        OTHER_ROOT,
        "other_feed.json",
        None,
        json!({"$defs": {
            "manifest": {"type": "object", "properties": {"other_id": {"type": "integer"}}}
        }}),
    );
    f.extension(
        OTHER_PARENT,
        "other_parent.json",
        json!(OTHER_ROOT),
        json!({
            OTHER_ROOT: {"$defs": {"record": {"properties": {"other_title": {"type": "integer"}}}}}
        }),
    );
    // A child can refer to the root shape introduced by its parent; that target
    // must be named before any external refs are normalized.
    f.extension(
        CHILD,
        "a_child.json",
        json!(PARENT),
        json!({
            PARENT: {"$defs": {"record": {"allOf": [
                {"$ref": "feed.json#/$defs/record"},
                {"properties": {"eligible": {"type": "boolean"}}}
            ]}}}
        }),
    );
    for options in [
        f.directory_options(&[ROOT, OTHER_ROOT], &[PARENT, CHILD, OTHER_PARENT]),
        f.profile_options(&[ROOT, OTHER_ROOT, PARENT, CHILD, OTHER_PARENT]),
    ] {
        let bundle = generate_types(&options).unwrap();
        assert_closed(&bundle);
        assert_eq!(
            bundle.defs["FeedManifest"]["properties"]["id"]["type"],
            "string"
        );
        assert_eq!(
            bundle.defs["OtherFeedManifest"]["properties"]["other_id"]["type"],
            "integer"
        );
        assert_eq!(
            bundle.defs["FeedRecord"]["properties"]["title"]["type"],
            "string"
        );
        assert_eq!(
            bundle.defs["OtherFeedRecord"]["properties"]["other_title"]["type"],
            "integer"
        );
        assert_eq!(
            bundle.defs["FeedRecord"]["properties"]["eligible"]["type"],
            "boolean"
        );
        assert!(!bundle.defs.contains_key("Record") && !bundle.defs.contains_key("Manifest"));
    }
}

#[test]
fn descriptive_schema_name_does_not_turn_a_helper_namespace_into_a_capability() {
    let mut f = Fixtures::new();
    f.object_root(ROOT, "checkout.json");
    f.write(
        "helper_namespace.json",
        json!({"name": "Display name", "$defs": {
            "detail": {"type": "object", "properties": {"code": {"type": "string"}}}
        }}),
    );
    let compiled =
        ucp_schema::compile_types(&GenerateTypesOptions::new().schema_dir(&f.schemas)).unwrap();
    assert!(!compiled.active_capabilities.contains("helper_namespace"));
}

#[test]
fn explicit_helper_fragment_dependency_survives_without_activating_its_extension() {
    let mut f = Fixtures::new();
    f.capability(
        ROOT,
        "checkout.json",
        None,
        json!({"type": "object", "properties": {
            "detail": {"$ref": "parent.json#/$defs/parent_record", "ucp_request": "optional"}
        }}),
    );
    f.extension(
        PARENT,
        "parent.json",
        json!(ROOT),
        json!({
            ROOT: {"properties": {"inactive_field": {"$ref": "#/$defs/parent_record"},
                "unrelated": {"$ref": "missing_inactive_contribution.json"}}},
            "parent_record": {"type": "object", "properties": {
                "leaf": {"$ref": "#/$defs/parent_leaf", "ucp_request": "optional"}
            }},
            "parent_leaf": {"$ref": "types/leaf.json"}
        }),
    );
    f.write(
        "types/leaf.json",
        json!({"type": "object", "properties": {
            "client": {"type": "string", "ucp_response": "omit"},
            "server": {"type": "string", "ucp_request": "omit"}
        }}),
    );
    for options in [
        f.directory_options(&[ROOT], &[]),
        f.profile_options(&[ROOT]),
    ] {
        let bundle = generate_types(&options).unwrap();
        assert_closed(&bundle);
        assert_eq!(
            bundle.defs["Checkout"]["properties"]["detail"]["$ref"],
            "#/$defs/ParentRecord"
        );
        assert!(bundle.defs["Checkout"]["properties"]
            .get("inactive_field")
            .is_none());
        assert_eq!(
            bundle.defs["ParentRecord"]["properties"]["leaf"]["$ref"],
            "#/$defs/ParentLeaf"
        );
        assert!(bundle.defs.contains_key("Leaf"));
    }
}

#[test]
fn qualified_container_reexports_and_inplace_overlays_emit_their_reserved_names() {
    for record in [
        json!({"$ref": "types/record.json"}),
        json!({
            "allOf": [{"$ref": "types/record.json"}, {"properties": {"extra": {"type": "boolean"}}}]
        }),
    ] {
        let mut f = Fixtures::new();
        f.capability(ROOT, "feed.json", None, json!({"$defs": {
            "record": record,
            "manifest": {"type": "object", "properties": {"records": {"type": "array", "items": {"$ref": "#/$defs/record"}}}}
        }}));
        f.write(
            "types/record.json",
            json!({"type": "object", "properties": {"title": {"type": "string"}}}),
        );
        for options in [
            f.directory_options(&[ROOT], &[]),
            f.profile_options(&[ROOT]),
        ] {
            let bundle = generate_types(&options).unwrap();
            assert_closed(&bundle);
            assert_eq!(
                bundle.defs["Manifest"]["properties"]["records"]["items"]["$ref"],
                "#/$defs/FeedRecord"
            );
            assert!(bundle.defs.contains_key("FeedRecord"));
            assert_eq!(
                bundle.defs["Record"]["properties"]["title"]["type"],
                "string"
            );
        }
    }
}

#[test]
fn new_transitive_container_shapes_preserve_arrays_scalars_and_unions() {
    let mut f = Fixtures::new();
    f.capability(
        ROOT,
        "feed.json",
        None,
        json!({"$defs": {
            "manifest": {"type": "object", "properties": {"id": {"type": "string"}}}
        }}),
    );
    f.extension(
        PARENT,
        "z_parent.json",
        json!(ROOT),
        json!({ROOT: {"$defs": {
            "record": {"type": "array", "items": {"type": "string"}, "minItems": 1},
            "status": {"type": "string", "enum": ["ready", "pending"]}
        }}}),
    );
    f.extension(
        CHILD,
        "a_child.json",
        json!(PARENT),
        json!({PARENT: {"$defs": {
            "choice": {"oneOf": [{"type": "string"}, {"type": "integer"}]},
            "alternative": {"anyOf": [{"type": "null"}, {"type": "boolean"}]}
        }}}),
    );
    for options in [
        f.directory_options(&[ROOT], &[PARENT, CHILD]),
        f.profile_options(&[ROOT, PARENT, CHILD]),
    ] {
        let bundle = generate_types(&options).unwrap();
        assert_closed(&bundle);
        assert_eq!(bundle.defs["Record"]["type"], "array");
        assert_eq!(bundle.defs["Record"]["items"]["type"], "string");
        assert_eq!(bundle.defs["Record"]["minItems"], 1);
        assert_eq!(bundle.defs["Status"]["enum"], json!(["ready", "pending"]));
        assert_eq!(bundle.defs["Choice"]["oneOf"].as_array().unwrap().len(), 2);
        assert_eq!(
            bundle.defs["Alternative"]["anyOf"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }
}

#[test]
fn sparse_direct_multiroot_extension_never_borrows_another_root_contribution() {
    let mut f = Fixtures::new();
    for (name, file) in [(ROOT, "checkout.json"), (OTHER_ROOT, "other_root.json")] {
        let detail = if name == ROOT {
            json!({"$ref": "types/base_detail.json"})
        } else {
            json!({"type": "object", "properties": {"base": {"type": "string"}}})
        };
        f.capability(
            name,
            file,
            None,
            json!({"type": "object", "properties": {"detail": detail}}),
        );
    }
    f.write(
        "types/base_detail.json",
        json!({"type": "object", "properties": {"base": {"type": "string"}}}),
    );
    f.extension(
        PARENT,
        "parent.json",
        json!([ROOT, OTHER_ROOT]),
        json!({
            ROOT: {"properties": {"detail": {"$ref": "#/$defs/extension_detail"}}},
            "extension_detail": {"type": "object", "properties": {"extension": {"type": "boolean"}}}
        }),
    );
    let directory = generate_types(&f.directory_options(&[ROOT, OTHER_ROOT], &[PARENT])).unwrap();
    let profile = generate_types(&f.profile_options(&[ROOT, OTHER_ROOT, PARENT])).unwrap();
    assert_eq!(directory.defs, profile.defs);
    assert_closed(&profile);
    assert_eq!(
        profile.defs["Checkout"]["properties"]["detail"]["$ref"],
        "#/$defs/ExtensionDetail"
    );
    assert!(profile.defs["OtherRoot"]["properties"]["detail"]
        .get("$ref")
        .is_none());
    assert_eq!(
        profile.defs["OtherRoot"]["properties"]["detail"]["properties"]["base"]["type"],
        "string"
    );
}
