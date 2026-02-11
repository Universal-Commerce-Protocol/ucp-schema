# ucp-schema

CLI and library for working with UCP-annotated JSON Schemas.

UCP schemas use `ucp_request` and `ucp_response` annotations to define field visibility per operation. This tool resolves those annotations into standard JSON Schema, letting you validate payloads for specific operations (create, read, update, etc.).

## Installation

```bash
# Install from crates.io
cargo install ucp-schema

# Or build from source
git clone https://github.com/Universal-Commerce-Protocol/ucp-schema
cd ucp-schema
cargo install --path .
```

## Quick Start

Given a UCP schema where `id` is omitted on create but required on update:

```json
{
  "type": "object",
  "properties": {
    "id": {
      "type": "string",
      "ucp_request": { "create": "omit", "update": "required" }
    },
    "name": { "type": "string" }
  }
}
```

Resolve it for different operations:

```bash
# For create: id is removed from the schema
ucp-schema resolve schema.json --request --op create --pretty

# For update: id is required
ucp-schema resolve schema.json --request --op update --pretty
```

Validate a payload:

```bash
# This fails - id not allowed on create
echo '{"id": "123", "name": "test"}' > payload.json
ucp-schema validate payload.json --schema schema.json --request --op create

# This passes - id required on update
ucp-schema validate payload.json --schema schema.json --request --op update
```

## CLI Reference

### `resolve` - Generate operation-specific schema

```bash
ucp-schema resolve <schema> --request|--response --op <operation> [options]

Options:
  --pretty           Pretty-print JSON output
  --bundle           Inline all external $ref pointers (see Bundling)
  --strict=true      Reject unknown fields (default: false, see Validation)
  --output           Write to file instead of stdout
```

Examples:

```bash
# Resolve for create request, pretty print
ucp-schema resolve checkout.json --request --op create --pretty

# Resolve for read response
ucp-schema resolve checkout.json --response --op read

# Resolve from URL
ucp-schema resolve https://ucp.dev/schemas/checkout.json --request --op create

# Save resolved schema to file
ucp-schema resolve checkout.json --request --op create --output resolved.json
```

### `validate` - Validate payload against resolved schema

UCP payloads are self-describing: they embed capability metadata that declares which schemas apply. The validator can use this metadata directly, or you can specify an explicit schema.

```bash
# Self-describing mode (response: ucp.capabilities, JSONRPC request: meta.profile)
ucp-schema validate <payload> --op <operation> [options]

# REST request mode (profile via flag, payload is raw object)
ucp-schema validate <payload> --profile <profile> --op <operation> [options]

# Explicit schema mode (overrides self-describing)
ucp-schema validate <payload> --schema <schema> --request|--response --op <operation> [options]

Options:
  --schema <path>              Explicit schema (overrides self-describing mode)
  --profile <path>             Agent profile URL/path (REST request pattern)
  --schema-local-base <dir>    Local directory to resolve schema URLs (see Validation Modes)
  --schema-remote-base <url>   URL prefix to strip when mapping to local (see URL Prefix Mapping)
  --request                    Direction is request (required with --schema, auto-detected otherwise)
  --response                   Direction is response (required with --schema, auto-detected otherwise)
  --json                       Output results as JSON (for automation)
  --strict=true                Reject unknown fields (default: false, see Validation)
```

Exit codes:

- `0` - Valid
- `1` - Validation failed (payload doesn't match schema)
- `2` - Schema error (invalid annotations, parse error, composition error)
- `3` - File/network error

### Validation Modes

The validator supports multiple modes based on which flags you provide:

| Mode                           | Command                                                            | Schema Source            | Direction     |
| ------------------------------ | ------------------------------------------------------------------ | ------------------------ | ------------- |
| **Response (self-describing)** | `validate response.json --op read`                                 | `ucp.capabilities` URLs  | Auto-detected |
| **JSONRPC request**            | `validate envelope.json --op create`                               | `meta.profile` URL       | Auto-detected |
| **REST request**               | `validate payload.json --profile profile.json --op create`         | Explicit `--profile` URL | Request       |
| **Explicit schema**            | `validate payload.json --schema schema.json --request --op create` | Specified schema         | Must specify  |

**Response Pattern (self-describing)**

Response payloads embed capability metadata declaring which schemas apply:

```json
{
  "ucp": {
    "capabilities": {
      "dev.ucp.shopping.checkout": [{"version": "2026-01-26", "schema": "https://..."}]
    }
  },
  "id": "checkout-123",
  "line_items": [...]
}
```

```bash
ucp-schema validate response.json --op read
```

**JSONRPC Request Pattern**

JSONRPC requests have `meta.profile` at root, with payload nested under the capability short name:

```json
{
  "meta": {
    "profile": "https://agent.example.com/.well-known/ucp"
  },
  "checkout": {
    "line_items": [...]
  }
}
```

```bash
ucp-schema validate envelope.json --op create
```

The validator:

1. Fetches the profile from `meta.profile`
2. Extracts capabilities from the profile
3. Extracts payload from the capability key (e.g., `checkout`)
4. Composes and validates

**REST Request Pattern (--profile flag)**

REST requests pass the profile via HTTP header (simulated with `--profile`), with payload being the raw object:

```bash
# Payload is just the checkout object (no envelope)
ucp-schema validate raw-checkout.json --profile agent-profile.json --op create
```

The `--profile` flag:

- Takes a profile URL or file path
- Extracts capabilities from the profile
- Treats the payload as the raw checkout object (no envelope extraction)
- Implies `--request` direction

**Mode 2: Self-describing + local resolution**

Same as above, but schema URLs are resolved to local files instead of fetched:

```bash
# Schema URL https://ucp.dev/schemas/shopping/checkout.json
# Maps to: ./local/schemas/shopping/checkout.json
ucp-schema validate response.json --schema-local-base ./local --op read
```

The `--schema-local-base` flag maps URL paths to local files:

- URL: `https://ucp.dev/schemas/shopping/checkout.json`
- Path extracted: `/schemas/shopping/checkout.json`
- Local file: `{schema-local-base}/schemas/shopping/checkout.json`

**URL Prefix Mapping**

When schema URLs have versioned prefixes that don't match your local directory structure, use `--schema-remote-base` to strip the prefix:

```bash
# Schema URL: https://ucp.dev/draft/schemas/shopping/checkout.json
# Local path: ./site/schemas/shopping/checkout.json (no "draft" directory)
ucp-schema validate response.json \
  --schema-local-base ./site \
  --schema-remote-base "https://ucp.dev/draft" \
  --op read
```

Mapping with `--schema-remote-base`:

- URL: `https://ucp.dev/draft/schemas/shopping/checkout.json`
- Strip prefix: `https://ucp.dev/draft` → `/schemas/shopping/checkout.json`
- Local file: `{schema-local-base}/schemas/shopping/checkout.json`

This is useful when published schemas have versioned `$id` URLs but your local files are organized without the version prefix.

Useful for: offline testing, local development, testing schema changes before deployment.

**Mode 3: Explicit schema**

Bypass self-describing metadata entirely by specifying `--schema`:

```bash
# Ignores any ucp.capabilities in payload, uses specified schema
ucp-schema validate order.json --schema checkout.json --request --op create

# Works with URLs too
ucp-schema validate order.json --schema https://ucp.dev/schemas/checkout.json --request --op create
```

Requires: explicit `--request` or `--response` flag (direction cannot be auto-detected).

**Error: No schema source**

If payload has no `ucp.capabilities`/`meta.profile` AND no `--schema`/`--profile` is specified:

```bash
ucp-schema validate payload.json --op read
# Error: cannot infer direction: payload has no ucp.capabilities (response) or meta.profile (request)
```

**JSON output for automation:**

```bash
ucp-schema validate order.json --schema checkout.json --request --op create --json
# Output: {"valid":true}
# Or:     {"valid":false,"errors":[{"path":"","message":"..."}]}
```

### `lint` - Static analysis of schema files

Catch schema errors before runtime. The linter checks for issues that would cause failures during resolution or validation.

```bash
ucp-schema lint <path> [options]

Options:
  --format <text|json>  Output format (default: text)
  --strict              Treat warnings as errors
  --quiet, -q           Only show errors, suppress progress
```

**What it checks:**

| Category    | Issue                                                        | Severity |
| ----------- | ------------------------------------------------------------ | -------- |
| Syntax      | Invalid JSON                                                 | Error    |
| References  | `$ref` to missing file                                       | Error    |
| References  | `$ref` to missing anchor (e.g., `#/$defs/foo`)               | Error    |
| Annotations | Invalid `ucp_*` type (must be string or object)              | Error    |
| Annotations | Invalid visibility value (must be omit/required/optional)    | Error    |
| Hygiene     | Missing `$id` field                                          | Warning  |
| Hygiene     | Unknown operation in annotation (e.g., `{"delete": "omit"}`) | Warning  |

**Examples:**

```bash
# Lint a directory of schemas
ucp-schema lint schemas/

# Lint single file, fail on warnings
ucp-schema lint checkout.json --strict

# CI-friendly JSON output
ucp-schema lint schemas/ --format json

# Quiet mode - only show errors
ucp-schema lint schemas/ --quiet
```

**Exit codes:**

- `0` - All files passed (or only warnings in non-strict mode)
- `1` - Errors found (or warnings in strict mode)
- `2` - Path not found

**JSON output format:**

```json
{
  "path": "schemas/",
  "files_checked": 5,
  "passed": 4,
  "failed": 1,
  "errors": 1,
  "warnings": 2,
  "results": [
    {
      "file": "checkout.json",
      "status": "error",
      "diagnostics": [
        {
          "severity": "error",
          "code": "E002",
          "path": "/properties/buyer/$ref",
          "message": "file not found: types/buyer.json"
        }
      ]
    }
  ]
}
```

## Schema Composition from Capabilities

UCP responses are self-describing - they embed `ucp.capabilities` declaring which schemas apply:

```json
{
  "ucp": {
    "capabilities": {
      "dev.ucp.shopping.checkout": [{
        "version": "2026-01-11",
        "schema": "https://ucp.dev/schemas/shopping/checkout.json"
      }],
      "dev.ucp.shopping.discount": [{
        "version": "2026-01-11",
        "schema": "https://ucp.dev/schemas/shopping/discount.json",
        "extends": "dev.ucp.shopping.checkout"
      }]
    }
  },
  "id": "...",
  "discounts": { ... }
}
```

**How composition works:**

1. **Root capability**: One capability has no `extends` - this is the base schema
2. **Extensions**: Capabilities with `extends` add fields to the root
3. **Composition**: Extensions define their additions in `$defs[root_capability_name]`
4. **allOf merge**: The composed schema uses `allOf` to combine all extensions

For the example above, the composed schema is:

```json
{
  "allOf": [
    {
      /* checkout's $defs["dev.ucp.shopping.checkout"] from discount.json */
    }
  ]
}
```

**Schema authoring for extensions:**

Extension schemas must define their additions in `$defs` under the root capability name:

```json
{
  "$id": "https://ucp.dev/schemas/shopping/discount.json",
  "name": "dev.ucp.shopping.discount",
  "$defs": {
    "dev.ucp.shopping.checkout": {
      "allOf": [
        { "$ref": "checkout.json" },
        {
          "type": "object",
          "properties": {
            "discounts": {
              /* discount-specific fields */
            }
          }
        }
      ]
    }
  }
}
```

**Graph validation:**

- Exactly one root capability (no `extends`)
- All `extends` references must exist in capabilities
- All extensions must transitively reach the root (no orphan extensions)

## Bundling External References

UCP schemas often use `$ref` to reference external files:

```json
{
  "properties": {
    "buyer": { "$ref": "types/buyer.json" },
    "shipping": { "$ref": "types/address.json#/$defs/postal" }
  }
}
```

The `--bundle` flag inlines all external references, producing a self-contained schema:

```bash
ucp-schema resolve checkout.json --request --op create --bundle --pretty
```

**When to use bundling:**

- Distributing schemas without file dependencies
- Feeding schemas to tools that don't support external refs
- Debugging to see the fully-expanded schema
- Pre-processing for faster repeated validation

**How it works:**

- External file refs (`"$ref": "types/buyer.json"`) are loaded and inlined
- Fragment refs (`"$ref": "types/common.json#/$defs/address"`) navigate to the specific definition
- Internal refs within external files (`"$ref": "#/$defs/foo"`) resolve correctly against their source file
- Self-referential recursive types (`"$ref": "#"`) are preserved (can't be inlined)
- Circular references between files are detected and reported as errors

## Validation

By default, the validator respects UCP's extensibility model:

- **Validates:** Payload conforms to spec shape (types, required fields, enums, nested structures)
- **Allows:** Additional/unknown fields (extensibility is intentional)

```bash
# Validates that known fields are correct, allows extra fields
ucp-schema validate response.json --op read
```

This works because UCP schemas use `additionalProperties: true` intentionally - extensions add new fields, and forward compatibility requires tolerating unknown fields.

**Enabling strict mode:**

For cases where you want to reject unknown fields (e.g., closed systems, catching typos):

```bash
# Reject any fields not defined in schema
ucp-schema validate payload.json --schema schema.json --request --op create --strict=true

# Resolved schema will have additionalProperties: false injected
ucp-schema resolve schema.json --request --op create --strict=true
```

**What strict mode does:**

- Adds `additionalProperties: false` to all object schemas (root, nested, in arrays, in definitions)
- Only injects `false` when `additionalProperties` is missing or explicitly `true`
- Preserves custom `additionalProperties` schemas (e.g., `{"type": "string"}`)
- Preserves explicit `additionalProperties: false`

**Note:** Strict mode does not work well with `allOf` composition (each branch validates independently and rejects properties from other branches). Use default non-strict mode for composed schemas.

## Visibility Rules

Annotations control how fields appear in the resolved schema:

| Value           | Effect on Properties | Effect on Required Array |
| --------------- | -------------------- | ------------------------ |
| `"omit"`        | Field removed        | Field removed            |
| `"required"`    | Field kept           | Field added              |
| `"optional"`    | Field kept           | Field removed            |
| (no annotation) | Field kept           | Unchanged                |

### Annotation Formats

**Shorthand** - applies to all operations:

```json
{ "ucp_request": "omit" }
```

**Per-operation** - different behavior per operation:

```json
{ "ucp_request": { "create": "omit", "update": "required", "read": "omit" } }
```

**Separate request/response:**

```json
{
  "ucp_request": { "create": "omit" },
  "ucp_response": "required"
}
```

## More Information

See **[FAQ.md](./FAQ.md)** for common questions about validator behavior and design decisions

## License

Apache-2.0
