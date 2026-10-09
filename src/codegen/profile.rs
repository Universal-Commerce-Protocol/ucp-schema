//! Stage 1 (Profile Mode): Self-contained `.well-known/ucp` profile parsing,
//! orphaned extension pruning, REST service extraction, and transitive `$ref` closure crawl.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use serde_json::Value;
use url::Url;

use crate::codegen::reachability::{
    collect_active_external_refs, compute_inactive_local_defs, normalize_lexical_path, LoadedSchema,
};
use crate::codegen::CodegenError;
use crate::compose::{extract_capabilities, verify_bindings, Capability, SchemaBaseConfig};
use crate::error::{ComposeError, ResolveError};
use crate::loader::{is_url, load_schema_auto};

const AMBIENT_UCP_REL_ROOTS: &[&str] = &["common/types/error_response.json", "profile.json"];

/// A `"transport": "rest"` service binding extracted from a UCP discovery profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestServiceBinding {
    pub service_name: String,
    pub version: String,
    pub endpoint: Option<String>,
    pub schema_url: String,
}

/// Validated capabilities and REST service bindings extracted from a UCP discovery profile.
#[derive(Debug, Clone)]
pub struct ParsedProfile {
    pub capabilities: Vec<Capability>,
    pub rest_services: Vec<RestServiceBinding>,
}

/// Load a UCP discovery profile from a local path or URL, validate all namespace authority
/// bindings, iteratively prune orphaned extensions (with warnings to `stderr`), and extract
/// active capabilities and `"transport": "rest"` service bindings.
pub fn parse_profile_source(source: &str) -> Result<ParsedProfile, CodegenError> {
    let profile_val = load_schema_auto(source)?;
    parse_profile_value(&profile_val)
}

fn parse_profile_value(profile_val: &Value) -> Result<ParsedProfile, CodegenError> {
    verify_bindings(profile_val)?;
    let mut capabilities = extract_capabilities(profile_val, &SchemaBaseConfig::default())?;
    prune_orphaned_extensions(&mut capabilities)?;
    let rest_services = extract_rest_services(profile_val);
    Ok(ParsedProfile {
        capabilities,
        rest_services,
    })
}

/// Iteratively prune any extension whose `extends` targets are all absent from the active
/// capability set (UCP Spec §Intersection Algorithm Steps 3–4), emitting a warning to `stderr`.
fn prune_orphaned_extensions(capabilities: &mut Vec<Capability>) -> Result<(), CodegenError> {
    loop {
        let active_names: BTreeSet<String> = capabilities.iter().map(|c| c.name.clone()).collect();
        let orphan_idx = capabilities.iter().position(|cap| {
            cap.extends
                .as_ref()
                .is_some_and(|parents| !parents.iter().any(|p| active_names.contains(p)))
        });
        let Some(idx) = orphan_idx else {
            break;
        };
        let removed = capabilities.remove(idx);
        let targets = removed.extends.as_deref().unwrap_or_default().join(", ");
        eprintln!(
            "warning: pruning orphaned extension '{}' (none of its 'extends' targets [{}] are active in profile)",
            removed.name, targets
        );
    }

    if !capabilities.iter().any(|c| c.extends.is_none()) {
        return Err(CodegenError::ComposeError(ComposeError::NoRootCapability));
    }
    Ok(())
}

fn extract_rest_services(profile_val: &Value) -> Vec<RestServiceBinding> {
    let ucp = profile_val.get("ucp").unwrap_or(profile_val);
    let Some(services_obj) = ucp.get("services").and_then(Value::as_object) else {
        return Vec::new();
    };

    let mut rest_services = Vec::new();
    for (service_name, versions) in services_obj {
        let entries: Vec<&Value> = match versions.as_array() {
            Some(arr) => arr.iter().collect(),
            None => vec![versions],
        };
        for entry in entries {
            if entry.get("transport").and_then(Value::as_str) != Some("rest") {
                continue;
            }
            let version = entry
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let endpoint = entry
                .get("endpoint")
                .and_then(Value::as_str)
                .map(str::to_string);
            let schema_url = entry
                .get("schema")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            rest_services.push(RestServiceBinding {
                service_name: service_name.clone(),
                version,
                endpoint,
                schema_url,
            });
        }
    }
    rest_services
}

#[derive(Default)]
struct ProfileCrawler {
    items: Vec<LoadedSchema>,
    sources: Vec<String>,
    source_to_idx: BTreeMap<String, usize>,
    local_schema_roots: Vec<PathBuf>,
    queue: VecDeque<usize>,
}

impl ProfileCrawler {
    fn load_or_get(&mut self, raw_source: &str) -> Result<usize, CodegenError> {
        let canonical = canonicalize_source(raw_source);
        if let Some(&existing_idx) = self.source_to_idx.get(&canonical) {
            return Ok(existing_idx);
        }

        let schema = load_schema_auto(&canonical)?;
        self.record_local_schema_root(&canonical, &schema);
        let idx = self.items.len();
        self.items.push(LoadedSchema::from_file(&canonical, schema));
        self.sources.push(canonical.clone());
        self.source_to_idx.insert(canonical, idx);
        self.queue.push_back(idx);
        Ok(idx)
    }

    fn record_local_schema_root(&mut self, canonical_source: &str, schema: &Value) {
        if is_url(canonical_source) {
            return;
        }
        let source_path = Path::new(canonical_source);
        if let Some((_, rel_after_schemas)) = schema
            .get("$id")
            .and_then(Value::as_str)
            .and_then(|id| id.split_once("/schemas/"))
        {
            let depth = rel_after_schemas
                .split('/')
                .filter(|s| !s.is_empty())
                .count();
            if let Some(root) = source_path
                .ancestors()
                .nth(depth)
                .filter(|p| !p.as_os_str().is_empty())
            {
                let root_buf = root.to_path_buf();
                if !self.local_schema_roots.contains(&root_buf) {
                    self.local_schema_roots.push(root_buf);
                }
            }
        }
        for ancestor in source_path.ancestors() {
            if ancestor.file_name().and_then(|s| s.to_str()) != Some("schemas") {
                continue;
            }
            let root = ancestor.to_path_buf();
            if !self.local_schema_roots.contains(&root) {
                self.local_schema_roots.push(root);
            }
        }
    }

    fn resolve_target(&self, base_source: &str, target_ref: &str) -> Result<String, CodegenError> {
        resolve_schema_target(base_source, target_ref, &self.local_schema_roots)
    }
}

/// Load a profile from `profile_source`, resolve all active capability and extension schemas
/// from their declared `"schema"` URLs/paths, crawl transitive relative and absolute `$ref`s,
/// seed ambient protocol roots (`common/types/error_response.json` and `profile.json`) when
/// `ucp.json` is discovered, and return the Stage 1 closure tuple for `compile_types()`.
#[allow(clippy::type_complexity)]
pub(crate) fn load_and_compute_profile_closure(
    profile_source: &str,
) -> Result<
    (
        Vec<LoadedSchema>,
        BTreeSet<usize>,
        BTreeSet<usize>,
        BTreeSet<usize>,
        BTreeSet<String>,
    ),
    CodegenError,
> {
    let parsed = parse_profile_source(profile_source)?;
    let mut crawler = ProfileCrawler::default();
    let mut raw_active_caps = BTreeSet::new();
    let mut raw_active_exts = BTreeSet::new();
    let mut active_capabilities = BTreeSet::new();

    for cap in &parsed.capabilities {
        let resolved_source = crawler.resolve_target(profile_source, &cap.schema_url)?;
        let idx = crawler.load_or_get(&resolved_source)?;
        if crawler.items[idx].name.is_none() {
            crawler.items[idx].name = Some(cap.name.clone());
        }
        if let Some(parents) = &cap.extends {
            crawler.items[idx]
                .declared_extends
                .get_or_insert_with(BTreeSet::new)
                .extend(parents.iter().cloned());
        }
        if cap.extends.is_some() || crawler.items[idx].is_extension {
            crawler.items[idx].is_extension = true;
            raw_active_exts.insert(idx);
            continue;
        }
        raw_active_caps.insert(idx);
        active_capabilities.insert(crawler.items[idx].stem.clone());
        active_capabilities.insert(cap.name.clone());
        if let Some(schema_name) = &crawler.items[idx].name {
            active_capabilities.insert(schema_name.clone());
        }
    }

    let mut seeded_ucp_indices = BTreeSet::new();
    while let Some(idx) = crawler.queue.pop_front() {
        let base_source = crawler.sources[idx].clone();
        if crawler.items[idx].stem == "ucp" && seeded_ucp_indices.insert(idx) {
            for rel_ambient in AMBIENT_UCP_REL_ROOTS {
                let ambient_target = crawler.resolve_target(&base_source, rel_ambient)?;
                if !is_url(&ambient_target) && !Path::new(&ambient_target).exists() {
                    continue;
                }
                crawler.load_or_get(&ambient_target)?;
            }
        }

        let inactive_locals =
            compute_inactive_local_defs(&crawler.items[idx], &active_capabilities);
        let ext_refs = collect_active_external_refs(&crawler.items[idx], &inactive_locals);
        for ref_str in ext_refs {
            let file_part = ref_str.split('#').next().unwrap_or("");
            if file_part.is_empty() {
                continue;
            }
            let target_source = crawler.resolve_target(&base_source, file_part)?;
            crawler.load_or_get(&target_source)?;
        }
    }

    let mut order: Vec<usize> = (0..crawler.items.len()).collect();
    order.sort_by(|&a, &b| crawler.items[a].path.cmp(&crawler.items[b].path));

    let mut old_to_new = vec![0usize; crawler.items.len()];
    let mut loaded = Vec::with_capacity(crawler.items.len());
    let mut existing_items: Vec<Option<LoadedSchema>> =
        crawler.items.into_iter().map(Some).collect();
    for (new_idx, old_idx) in order.into_iter().enumerate() {
        old_to_new[old_idx] = new_idx;
        if let Some(item) = existing_items[old_idx].take() {
            loaded.push(item);
        }
    }

    let reachable_indices: BTreeSet<usize> = (0..loaded.len()).collect();
    let active_cap_indices: BTreeSet<usize> = raw_active_caps
        .into_iter()
        .map(|old_idx| old_to_new[old_idx])
        .collect();
    let active_ext_indices: BTreeSet<usize> = raw_active_exts
        .into_iter()
        .map(|old_idx| old_to_new[old_idx])
        .collect();

    Ok((
        loaded,
        reachable_indices,
        active_cap_indices,
        active_ext_indices,
        active_capabilities,
    ))
}

fn canonicalize_source(source: &str) -> String {
    let without_fragment = source.split('#').next().unwrap_or(source);
    if is_url(without_fragment) {
        return Url::parse(without_fragment)
            .map(|u| u.to_string())
            .unwrap_or_else(|_| without_fragment.to_string());
    }
    normalize_lexical_path(Path::new(without_fragment))
        .to_string_lossy()
        .into_owned()
}

fn resolve_schema_target(
    base_source: &str,
    target_ref: &str,
    local_schema_roots: &[PathBuf],
) -> Result<String, CodegenError> {
    let file_part = target_ref.split('#').next().unwrap_or("");
    if is_url(file_part) {
        if let Some(local_hit) = relocate_url_to_local_schemas(file_part, local_schema_roots) {
            return Ok(local_hit);
        }
        return Ok(canonicalize_source(file_part));
    }

    if is_url(base_source) {
        let base_url = Url::parse(base_source).map_err(|e| {
            CodegenError::ResolveError(ResolveError::InvalidSchema {
                message: format!("invalid base URL '{base_source}': {e}"),
            })
        })?;
        let joined = base_url.join(file_part).map_err(|e| {
            CodegenError::ResolveError(ResolveError::InvalidSchema {
                message: format!("cannot resolve '{file_part}' against '{base_source}': {e}"),
            })
        })?;
        return Ok(joined.to_string());
    }

    let parent_dir = Path::new(base_source)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let joined = normalize_lexical_path(&parent_dir.join(file_part));
    Ok(joined.to_string_lossy().into_owned())
}

fn relocate_url_to_local_schemas(url_str: &str, local_schema_roots: &[PathBuf]) -> Option<String> {
    let (_, rel_after_schemas) = url_str.split_once("/schemas/")?;
    for root in local_schema_roots {
        let candidate = normalize_lexical_path(&root.join(rel_after_schemas));
        if candidate.exists() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

/// Resolve a REST service binding's `schema` URL or relative path against `profile_source`,
/// relocating `.../services/<rel>` URLs to a sibling `services/` directory of any local
/// `schemas/` root discovered from the profile's capabilities when available on disk.
pub(crate) fn resolve_service_schema_target(
    profile_source: &str,
    service_schema_url: &str,
    capabilities: &[Capability],
) -> Result<String, CodegenError> {
    let local_schema_roots = collect_local_schema_roots(profile_source, capabilities);
    let file_part = service_schema_url.split('#').next().unwrap_or("");
    if is_url(file_part) {
        if let Some(local_hit) =
            relocate_url_to_local_services(profile_source, file_part, &local_schema_roots)
        {
            return Ok(local_hit);
        }
    }
    resolve_schema_target(profile_source, service_schema_url, &local_schema_roots)
}

fn collect_local_schema_roots(profile_source: &str, capabilities: &[Capability]) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for cap in capabilities {
        let Ok(resolved) = resolve_schema_target(profile_source, &cap.schema_url, &[]) else {
            continue;
        };
        if is_url(&resolved) {
            continue;
        }
        for ancestor in Path::new(&resolved).ancestors() {
            if ancestor.file_name().and_then(|s| s.to_str()) != Some("schemas") {
                continue;
            }
            let root = ancestor.to_path_buf();
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
    }
    roots
}

fn relocate_url_to_local_services(
    profile_source: &str,
    url_str: &str,
    local_schema_roots: &[PathBuf],
) -> Option<String> {
    let (_, rel_after_services) = url_str.split_once("/services/")?;
    if !is_url(profile_source) {
        for ancestor in Path::new(profile_source).ancestors().skip(1) {
            let candidate =
                normalize_lexical_path(&ancestor.join("services").join(rel_after_services));
            if candidate.exists() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    for root in local_schema_roots {
        let Some(source_root) = root.parent() else {
            continue;
        };
        let candidate =
            normalize_lexical_path(&source_root.join("services").join(rel_after_services));
        if candidate.exists() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_profile_value_extracts_rest_services_and_prunes_chained_orphaned_extensions() {
        let profile = json!({
            "ucp": {
                "version": "2026-08-25",
                "services": {
                    "dev.ucp.shopping": [
                        {
                            "version": "2026-08-25",
                            "transport": "rest",
                            "endpoint": "https://merchant.example.com/ucp",
                            "schema": "https://ucp.dev/2026-08-25/services/shopping/rest.openapi.json"
                        },
                        {
                            "version": "2026-08-25",
                            "transport": "mcp",
                            "endpoint": "https://merchant.example.com/mcp",
                            "schema": "https://ucp.dev/2026-08-25/services/shopping/mcp.openrpc.json"
                        }
                    ]
                },
                "capabilities": {
                    "dev.ucp.shopping.checkout": [
                        {
                            "version": "2026-08-25",
                            "schema": "https://ucp.dev/2026-08-25/schemas/shopping/checkout.json"
                        }
                    ],
                    "dev.ucp.shopping.discount": [
                        {
                            "version": "2026-08-25",
                            "schema": "https://ucp.dev/2026-08-25/schemas/shopping/discount.json",
                            "extends": ["dev.ucp.shopping.checkout", "dev.ucp.shopping.cart"]
                        }
                    ],
                    "dev.ucp.shopping.orphan_parent": [
                        {
                            "version": "2026-08-25",
                            "schema": "https://ucp.dev/2026-08-25/schemas/shopping/orphan_parent.json",
                            "extends": "dev.ucp.shopping.order"
                        }
                    ],
                    "dev.ucp.shopping.orphan_child": [
                        {
                            "version": "2026-08-25",
                            "schema": "https://ucp.dev/2026-08-25/schemas/shopping/orphan_child.json",
                            "extends": "dev.ucp.shopping.orphan_parent"
                        }
                    ]
                }
            }
        });

        let parsed = parse_profile_value(&profile).unwrap();
        let cap_names: Vec<&str> = parsed
            .capabilities
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(
            cap_names,
            vec!["dev.ucp.shopping.checkout", "dev.ucp.shopping.discount"]
        );
        assert_eq!(
            parsed.rest_services,
            vec![RestServiceBinding {
                service_name: "dev.ucp.shopping".to_string(),
                version: "2026-08-25".to_string(),
                endpoint: Some("https://merchant.example.com/ucp".to_string()),
                schema_url: "https://ucp.dev/2026-08-25/services/shopping/rest.openapi.json"
                    .to_string(),
            }]
        );
    }

    #[test]
    fn parse_profile_value_rejects_namespace_binding_violation_and_all_orphan_profiles() {
        let bad_binding = json!({
            "ucp": {
                "capabilities": {
                    "dev.ucp.shopping.checkout": [
                        {
                            "version": "2026-08-25",
                            "schema": "https://evil.example.com/checkout.json"
                        }
                    ]
                }
            }
        });
        let err = parse_profile_value(&bad_binding).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::ComposeError(ComposeError::NamespaceBindingViolation { .. })
        ));

        let only_orphans = json!({
            "ucp": {
                "capabilities": {
                    "dev.ucp.shopping.discount": [
                        {
                            "version": "2026-08-25",
                            "schema": "https://ucp.dev/2026-08-25/schemas/shopping/discount.json",
                            "extends": "dev.ucp.shopping.checkout"
                        }
                    ]
                }
            }
        });
        let err = parse_profile_value(&only_orphans).unwrap_err();
        assert!(matches!(
            err,
            CodegenError::ComposeError(ComposeError::NoRootCapability)
        ));
    }

    #[test]
    fn resolve_schema_target_handles_relative_urls_and_local_paths() {
        let resolved_url = resolve_schema_target(
            "https://ucp.dev/2026-08-25/schemas/shopping/checkout.json",
            "../ucp.json#/$defs/response_checkout_schema",
            &[],
        )
        .unwrap();
        assert_eq!(resolved_url, "https://ucp.dev/2026-08-25/schemas/ucp.json");

        let resolved_local = resolve_schema_target(
            "/work/schemas/shopping/checkout.json",
            "types/line_item.json",
            &[],
        )
        .unwrap();
        assert_eq!(
            resolved_local,
            "/work/schemas/shopping/types/line_item.json"
        );

        let resolved_rel_parent =
            resolve_schema_target("profile.json", "../schemas/shopping/checkout.json", &[])
                .unwrap();
        assert_eq!(resolved_rel_parent, "../schemas/shopping/checkout.json");

        let mut crawler = ProfileCrawler::default();
        crawler.record_local_schema_root(
            "/tmp/custom_root/shopping/checkout.json",
            &json!({ "$id": "https://ucp.dev/schemas/shopping/checkout.json" }),
        );
        assert_eq!(
            crawler.local_schema_roots,
            vec![PathBuf::from("/tmp/custom_root")]
        );
    }
}
