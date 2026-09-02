//! Namespace authority binding validation (UCP §Authority Binding).
//!
//! Validates that an entity's `schema` URL origin (a capability, service, or
//! payment handler) matches the reverse-domain authority encoded in its name.
//! The check is name-agnostic — [`validate_binding`] takes any reverse-domain
//! name and a URL.
//!
//! # Why the *inverted* strategy
//!
//! A capability name like `com.example.pay` follows the grammar
//! `{reverse-domain}.{service}.{capability}`. Every component is dot-separated
//! and DNS domains are variable-depth, so the boundary between the authority
//! domain and the service/capability suffix is **not** recoverable from the
//! name alone — `com.example.pay` could be authority `example.com` + `pay`, or a
//! three-label domain, and disambiguating requires the Public Suffix List and
//! is *still* ambiguous for single-suffix vendor names (`org.example.catalog`).
//!
//! So we do not derive the domain from the name. We read it from the
//! authoritative source we already hold: the declared `schema` URL host.
//! We reverse the host's labels into a prefix and assert the capability name is
//! namespaced under it. This is deterministic, PSL-free, and the
//! service/capability suffix stays **opaque** (never parsed).
//!
//! # The rule
//!
//! For a URL `U` declared by capability `name`:
//! 1. `U` MUST parse, use scheme `https`, and carry no userinfo (`user:pass@`).
//! 2. `U`'s host MUST be a registered domain name: at least two labels, not an
//!    IP literal.
//! 3. `reverse_labels(host)` MUST match `name` as an authority: either `name`
//!    EQUALS `reverse_labels(host)` — an entity whose name is itself a bare
//!    controlled domain, e.g. `com.example.pay` served from `pay.example.com` —
//!    or `name` starts with `reverse_labels(host) + "."` (a label-boundary
//!    prefix). The separating `.` in the prefix case keeps the match on a label
//!    boundary, preventing sibling-prefix spoofing (`examplepay.com` vs
//!    `example.com`). A local service/capability
//!    segment is NOT required: that is a separate naming convention for
//!    capabilities and services (overview §Naming Convention), not part of the
//!    provenance binding, and it does not apply to payment handlers.
//!
//! This binding applies to the **`schema`** URL — the machine-fetched artifact
//! that defines the wire contract and the thing compose dereferences. The
//! **`spec`** URL (human documentation) is not in the machine trust path and is
//! not authority-bound by this module.
//!
//! # Scope (what this does NOT prove)
//!
//! Binding proves the schema is *hosted on the domain that owns the namespace*.
//! It does not prove content authenticity, survive DNS/domain hijack, or re-check
//! anything once a schema is cached. It is anti-spoofing of namespace provenance,
//! not capability authorization or trust.
//!
//! # Internationalized domains
//!
//! IDN hosts normalize to punycode (`xn--…`) via the URL parser, and the binding
//! is a pure label-reversal + prefix check, so internationalized authorities
//! — including IDN TLDs (e.g. `.рф` → `xn--p1ai`) — bind correctly with no
//! special handling.

use url::{Host, Url};

/// Reason a `(name, url)` binding failed validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingError {
    /// The URL string could not be parsed.
    Unparsable { url: String, reason: String },
    /// Scheme is not `https` (downgrade / non-fetchable origin).
    NonHttpsScheme { url: String, scheme: String },
    /// URL carries userinfo (`user:pass@host`) — classic parser-confusion vector.
    Userinfo { url: String },
    /// Host is missing, an IP literal, or a single-label (non-registered) name.
    NonDomainHost { url: String },
    /// Host origin does not match the capability's namespace authority.
    AuthorityMismatch {
        name: String,
        url: String,
        host: String,
        /// The reverse-domain prefix the name was required to start with.
        expected_prefix: String,
    },
}

impl std::fmt::Display for BindingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BindingError::Unparsable { url, reason } => {
                write!(f, "unparsable URL '{url}': {reason}")
            }
            BindingError::NonHttpsScheme { url, scheme } => {
                write!(f, "URL '{url}' must use https, found '{scheme}'")
            }
            BindingError::Userinfo { url } => {
                write!(f, "URL '{url}' must not contain userinfo (user:pass@)")
            }
            BindingError::NonDomainHost { url } => write!(
                f,
                "URL '{url}' host must be a registered domain name (>=2 labels, not an IP)"
            ),
            BindingError::AuthorityMismatch {
                name,
                url,
                host,
                expected_prefix,
            } => write!(
                f,
                "'{name}' is not namespaced under host '{host}' \
                 (expected name to equal '{expected_prefix}' or start with \
                 '{expected_prefix}.') for URL '{url}'"
            ),
        }
    }
}

impl std::error::Error for BindingError {}

/// Reverse a host's dot-separated labels: `shopping.ucp.dev` -> `dev.ucp.shopping`.
///
/// A trailing root dot is ignored so `example.com.` and `example.com` reverse identically.
pub fn reverse_labels(host: &str) -> String {
    host.trim_end_matches('.')
        .split('.')
        .rev()
        .collect::<Vec<_>>()
        .join(".")
}

/// Check if a string is a valid reverse-domain capability identifier (e.g. `dev.ucp.shopping.checkout`).
///
/// A reverse-domain name has at least two dot-separated labels, and each label
/// is a non-empty alphanumeric/hyphen/underscore identifier.
pub fn is_reverse_domain_name(s: &str) -> bool {
    if s.ends_with(".json") {
        return false;
    }
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() < 2 {
        return false;
    }
    parts.iter().all(|p| {
        !p.is_empty()
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    })
}

/// Validate that `name`'s namespace authority matches the origin of its `schema`
/// `url`.
///
/// See the module docs for the full rule. Returns `Ok(())` when the binding is
/// sound, or a [`BindingError`] describing the first failing condition.
pub fn validate_binding(name: &str, url: &str) -> Result<(), BindingError> {
    let parsed = Url::parse(url).map_err(|e| BindingError::Unparsable {
        url: url.to_string(),
        reason: e.to_string(),
    })?;

    if parsed.scheme() != "https" {
        return Err(BindingError::NonHttpsScheme {
            url: url.to_string(),
            scheme: parsed.scheme().to_string(),
        });
    }

    // Reject userinfo before host inspection: `https://example.com@evil.com` parses
    // host as `evil.com`, but a naive substring check would see `example.com`.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(BindingError::Userinfo {
            url: url.to_string(),
        });
    }

    // `Host::Domain` is already lowercased and IDNA->punycode normalized by the
    // parser; IP literals surface as `Host::Ipv4`/`Ipv6` and are rejected.
    let host = match parsed.host() {
        Some(Host::Domain(d)) => d.trim_end_matches('.').to_ascii_lowercase(),
        _ => {
            return Err(BindingError::NonDomainHost {
                url: url.to_string(),
            })
        }
    };

    // A registered authority domain has at least two labels (rejects `localhost`).
    if !host.contains('.') {
        return Err(BindingError::NonDomainHost {
            url: url.to_string(),
        });
    }

    let expected_prefix = reverse_labels(&host);
    // Valid when the name IS the authority (exact match — e.g. `com.example.pay`
    // served from `pay.example.com`) or is namespaced under it (label-boundary
    // prefix; the separating `.` defeats `com.examplepay` matching `com.example`).
    // Provenance only: a local capability segment is a separate naming
    // convention, not required here.
    if name == expected_prefix || name.starts_with(&format!("{expected_prefix}.")) {
        Ok(())
    } else {
        Err(BindingError::AuthorityMismatch {
            name: name.to_string(),
            url: url.to_string(),
            host,
            expected_prefix,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverses_labels() {
        assert_eq!(reverse_labels("example.com"), "com.example");
        assert_eq!(reverse_labels("shopping.ucp.dev"), "dev.ucp.shopping");
        assert_eq!(reverse_labels("example.co.uk"), "uk.co.example");
        assert_eq!(reverse_labels("example.com."), "com.example");
    }

    #[test]
    fn accepts_apex_authority() {
        assert!(validate_binding("com.example.pay", "https://example.com/spec.json").is_ok());
        assert!(validate_binding(
            "dev.ucp.shopping.checkout",
            "https://ucp.dev/draft/schemas/shopping/checkout.json"
        )
        .is_ok());
        // Single-label remainder (vendor capability without an explicit service).
        assert!(
            validate_binding("org.example.catalog", "https://example.org/catalog.json").is_ok()
        );
        // Deeper authority domain.
        assert!(validate_binding(
            "uk.co.example.shopping.cart",
            "https://example.co.uk/cart.json"
        )
        .is_ok());
    }

    #[test]
    fn accepts_name_aligned_subdomain() {
        // Subdomain whose labels match the namespace path is the legitimate
        // domain owner and is allowed without a PSL.
        assert!(validate_binding(
            "dev.ucp.shopping.checkout",
            "https://shopping.ucp.dev/checkout.json"
        )
        .is_ok());
    }

    #[test]
    fn rejects_origin_mismatch() {
        // The motivating case: com.example.pay pointing at an unrelated origin.
        let err = validate_binding("com.example.pay", "https://foo.com/spec.json").unwrap_err();
        assert!(matches!(err, BindingError::AuthorityMismatch { .. }));
    }

    #[test]
    fn rejects_reserved_namespace_squat() {
        // Only ucp.dev's owner can serve a passing dev.ucp.* capability.
        let err =
            validate_binding("dev.ucp.shopping.checkout", "https://evil.com/x.json").unwrap_err();
        assert!(matches!(err, BindingError::AuthorityMismatch { .. }));
    }

    #[test]
    fn rejects_sibling_prefix_spoof() {
        // com.examplepay is a *string* prefix of com.example but not a *label* prefix.
        let err = validate_binding("com.example.pay", "https://examplepay.com/x.json").unwrap_err();
        assert!(matches!(err, BindingError::AuthorityMismatch { .. }));
    }

    #[test]
    fn rejects_unrelated_subdomain() {
        // S1 (no PSL): a CDN subdomain whose labels don't match the path is rejected.
        let err =
            validate_binding("com.example.pay", "https://cdn.example.com/x.json").unwrap_err();
        assert!(matches!(err, BindingError::AuthorityMismatch { .. }));
    }

    #[test]
    fn rejects_userinfo_confusion() {
        let err =
            validate_binding("com.example.pay", "https://example.com@evil.com/x.json").unwrap_err();
        assert_eq!(
            err,
            BindingError::Userinfo {
                url: "https://example.com@evil.com/x.json".into()
            }
        );
    }

    #[test]
    fn rejects_non_https() {
        let err = validate_binding("com.example.pay", "http://example.com/x.json").unwrap_err();
        assert!(matches!(err, BindingError::NonHttpsScheme { .. }));
    }

    #[test]
    fn rejects_ip_literal() {
        let err = validate_binding("com.example.pay", "https://93.184.216.34/x.json").unwrap_err();
        assert!(matches!(err, BindingError::NonDomainHost { .. }));
        let err6 = validate_binding("com.example.pay", "https://[2001:db8::1]/x.json").unwrap_err();
        assert!(matches!(err6, BindingError::NonDomainHost { .. }));
    }

    #[test]
    fn rejects_single_label_host() {
        let err = validate_binding("localhost.pay", "https://localhost/x.json").unwrap_err();
        assert!(matches!(err, BindingError::NonDomainHost { .. }));
    }

    #[test]
    fn accepts_exact_authority_match() {
        // Host reverses to exactly the name: the publisher provably controls the
        // whole namespace, so the binding holds with no local segment. Provenance
        // is the only concern; a `{service}.{capability}` remainder is a separate
        // naming convention (and does not apply to payment handlers).
        assert!(validate_binding("com.example", "https://example.com/x.json").is_ok());
        // The bare-domain handler shape: `com.example.pay` served from
        // `pay.example.com` (reversed host == the full name). A strict-remainder
        // rule wrongly rejected this; exact match accepts it.
        assert!(validate_binding("com.example.pay", "https://pay.example.com/config.json").is_ok());
    }

    #[test]
    fn name_binds_from_exact_or_parent_authority() {
        // A parent domain's reversed labels are also a prefix, so the same name
        // binds from its exact host or a parent authority — both prove control.
        assert!(validate_binding("com.example.pay", "https://pay.example.com/x.json").is_ok());
        assert!(validate_binding("com.example.pay", "https://example.com/x.json").is_ok());
    }

    #[test]
    fn host_case_is_normalized() {
        assert!(validate_binding("com.example.pay", "https://EXAMPLE.COM/x.json").is_ok());
    }

    #[test]
    fn hyphenated_and_punycode_domains() {
        // Interior hyphens (real merchant domains) bind correctly — the check is
        // label-boundary, so hyphens are ordinary intra-label characters.
        assert!(validate_binding(
            "com.example-shop.shopping.checkout",
            "https://example-shop.com/checkout.json"
        )
        .is_ok());
        assert!(validate_binding(
            "com.example-corp.loyalty",
            "https://example-corp.com/x.json"
        )
        .is_ok());
        // Punycode (IDN) SLD under an ASCII TLD binds the same way.
        assert!(validate_binding(
            "com.xn--mnchen-3ya.checkout",
            "https://xn--mnchen-3ya.com/x.json"
        )
        .is_ok());
        // IDN TLD (e.g. .рф -> xn--p1ai) as the reversed first segment also binds.
        assert!(validate_binding(
            "xn--p1ai.example.checkout",
            "https://example.xn--p1ai/x.json"
        )
        .is_ok());
        // An unrelated subdomain of a hyphenated domain still fails (S1 rule,
        // not a hyphen issue): shop.example-co.com -> com.example-co.shop.
        assert!(matches!(
            validate_binding(
                "com.example-co.shopping.cart",
                "https://shop.example-co.com/cart.json"
            )
            .unwrap_err(),
            BindingError::AuthorityMismatch { .. }
        ));
    }
}
