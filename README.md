# ucp-schema

CLI and library for [Universal Commerce Protocol](https://ucp.dev) (UCP) schemas.

UCP defines an open extensibility protocol on top of JSON Schema. Agents negotiate capabilities at runtime via self-describing payloads, extensions compose dynamically via `allOf`, and `ucp_request`/`ucp_response` annotations control field visibility per direction and operation. This tool implements UCP's composition and resolution pipeline: compose capability schemas, resolve annotations into standard JSON Schema, and validate payloads.

## How It Works

The CLI exposes a progressive pipeline. Each command runs it up to its named step:

```
  ┌───────────────────┐     ┌───────────────────┐     ┌───────────────────┐
  │      compose      │ ──▶ │      resolve      │ ──▶ │     validate      │
  └───────────────────┘     └───────────────────┘     └───────────────────┘
   merge capability          apply annotations           check payload
   schemas into one          for direction + op           against schema
```

Like `gcc -E` (preprocess only) vs `gcc` (full build), each command runs the pipeline to a different depth: `compose` stops after merging capability schemas, `resolve` applies annotations, and `validate` runs through to payload checking. When the input is a self-describing payload, earlier stages run automatically. `lint` is independent static analysis.

For example, a field annotated with `"ucp_request": {"create": "omit", "update": "required"}` disappears from create schemas but becomes required on update — one source schema, different views per operation. See [Visibility Rules](#visibility-rules) for the full worked example.

**"I want to..."**

| Goal                                                | Command                                                         |
| --------------------------------------------------- | --------------------------------------------------------------- |
| Inspect the composed schema (annotations preserved) | `compose payload.json --schema-local-base ./schemas --pretty`   |
| Get JSON Schema for an operation                    | `resolve payload.json --op read --schema-local-base ./schemas`  |
| Resolve a single schema file (no composition)       | `resolve schema.json --request --op create`                     |
| Validate a payload end-to-end                       | `validate payload.json --op read --schema-local-base ./schemas` |
| Check schemas for errors before runtime             | `lint schemas/`                                                 |
| Debug what the pipeline is doing                    | Add `--verbose` to any command                                  |

## Installation

```bash
# Install from crates.io
cargo install ucp-schema

# Or build from source
git clone https://github.com/Universal-Commerce-Protocol/ucp-schema
cd ucp-schema
cargo install --path .
```

## CLI Reference

### `compose` — Compose schemas from capabilities

Pure composition: merges capability schemas from a self-describing payload into one schema. Output preserves UCP annotations (no resolve step).

```bash
ucp-schema compose <payload> [options]

Options:
  --schema-local-base <dir>   Local directory for schema resolution
  --schema-remote-base <url>  URL prefix to strip when mapping to local (see Concepts > Local Resolution)
  --pretty                    Pretty-print JSON output
  --output <path>             Write to file instead of stdout
  --verbose, -v               Print pipeline stages to stderr
```

`compose` does not accept `--request`/`--response`/`--op` — those belong to `resolve` and `validate`.

```bash
# Inspect the merged schema before resolution
ucp-schema compose response.json --schema-local-base ./schemas --pretty

# Save for debugging
ucp-schema compose response.json --schema-local-base ./schemas --output composed.json
```

### `resolve` — Generate operation-specific schema

Accepts a schema file or a self-describing payload. When given a payload, automatically composes schemas from capabilities before resolving.

```bash
# Schema input (direction required)
ucp-schema resolve <schema> --request|--response --op <operation> [options]

# Payload input (direction auto-inferred)
ucp-schema resolve <payload> --op <operation> --schema-local-base <dir> [options]

Options:
  --request / --response      Direction (required for schema input, auto-inferred for payloads)
  --op <operation>            Operation: create, read, update, complete
  --pretty                    Pretty-print JSON output
  --output <path>             Write to file instead of stdout
  --bundle                    Inline external $ref pointers (schema input only; payloads bundle automatically)
  --schema-local-base <dir>   Local directory for schema resolution (payload input only)
  --schema-remote-base <url>  URL prefix to strip when mapping to local
  --strict                    Inject additionalProperties: false (see Concepts > Strict Mode)
  --verbose, -v               Print pipeline stages to stderr
```

```bash
# Schema file → resolved schema
ucp-schema resolve checkout.json --request --op create --pretty

# Self-describing payload → auto-compose, auto-detect direction, resolve
ucp-schema resolve response.json --op read --schema-local-base ./schemas

# Bundle external $refs into a self-contained schema
ucp-schema resolve checkout.json --request --op create --bundle --pretty

# Resolve from URL
ucp-schema resolve https://ucp.dev/schemas/checkout.json --request --op create
```

### `validate` — Validate payload against resolved schema

```bash
ucp-schema validate <payload> --op <operation> [options]

Options:
  --schema <path|url>          Explicit schema (skips self-describing detection)
  --profile <path|url>         Agent profile (REST request pattern)
  --request / --response       Direction (required with --schema, auto-detected otherwise)
  --op <operation>             Operation: create, read, update, complete
  --schema-local-base <dir>    Local directory to resolve schema URLs
  --schema-remote-base <url>   URL prefix to strip when mapping to local
  --strict                     Reject unknown fields (see Concepts > Strict Mode)
  --json                       Machine-readable JSON output
  --verbose, -v                Print pipeline stages to stderr
```

The validator auto-detects how to find the schema based on what flags you provide and what metadata the payload contains (see [Validation Modes](#validation-modes) in Concepts):

| Pattern                        | Command                                                       | Schema Source           | Direction |
| ------------------------------ | ------------------------------------------------------------- | ----------------------- | --------- |
| **Response** (self-describing) | `validate response.json --op read`                            | `ucp.capabilities` URLs | Auto      |
| **JSONRPC request**            | `validate envelope.json --op create`                          | `meta.profile` URL      | Auto      |
| **REST request**               | `validate payload.json --profile profile.json --op create`    | `--profile` URL         | Request   |
| **Explicit schema**            | `validate payload.json --schema s.json --request --op create` | `--schema`              | Specified |

```bash
# Self-describing response
ucp-schema validate response.json --op read --schema-local-base ./schemas

# Explicit schema
ucp-schema validate order.json --schema checkout.json --request --op create

# Machine-readable output for CI
ucp-schema validate order.json --schema checkout.json --request --op create --json
# → {"valid":true}
# → {"valid":false,"errors":[{"path":"","message":"..."}]}
```

Exit codes: `0` valid, `1` validation failed, `2` schema error, `3` file/network error.

### `lint` — Static analysis of schema files

Catch schema errors before runtime.

```bash
ucp-schema lint <path> [options]

Options:
  --format <text|json>  Output format (default: text)
  --strict              Treat warnings as errors
  --quiet, -q           Only show errors, suppress progress
```

| Category    | Issue                                                        | Severity |
| ----------- | ------------------------------------------------------------ | -------- |
| Syntax      | Invalid JSON                                                 | Error    |
| References  | `$ref` to missing file                                       | Error    |
| References  | `$ref` to missing anchor (`#/$defs/foo`)                     | Error    |
| Annotations | Invalid `ucp_*` type (must be string or object)              | Error    |
| Annotations | Invalid visibility value (must be omit/required/optional)    | Error    |
| Hygiene     | Missing `$id` field                                          | Warning  |
| Hygiene     | Unknown operation in annotation (e.g., `{"delete": "omit"}`) | Warning  |

```bash
# Lint a directory of schemas
ucp-schema lint schemas/

# CI-friendly: fail on warnings, JSON output
ucp-schema lint schemas/ --strict --format json
```

Exit codes: `0` passed, `1` errors found, `2` path not found.

<details>
<summary>JSON output format</summary>

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

</details>

## Concepts

### Visibility Rules

`ucp_request` and `ucp_response` annotations control which fields appear in the resolved schema. Given a schema where `id` is server-generated:

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

Resolving for `--request --op create` removes `id` — clients don't send server-generated fields:

```json
{
  "type": "object",
  "properties": {
    "name": { "type": "string" }
  }
}
```

Resolving for `--request --op update` makes `id` required — you must specify which resource to update:

```json
{
  "type": "object",
  "properties": {
    "id": { "type": "string" },
    "name": { "type": "string" }
  },
  "required": ["id"]
}
```

Annotations are stripped; output is standard JSON Schema.

**Resolution rules:**

| Value           | Effect on Properties | Effect on Required Array |
| --------------- | -------------------- | ------------------------ |
| `"omit"`        | Field removed        | Field removed            |
| `"required"`    | Field kept           | Field added              |
| `"optional"`    | Field kept           | Field removed            |
| (no annotation) | Field kept           | Unchanged                |

Annotations can be **shorthand** (all operations) or **per-operation**, and request/response are independent:

```json
{
  "id": {
    "type": "string",
    "ucp_request": { "create": "omit", "update": "required" },
    "ucp_response": "required"
  },
  "status": {
    "type": "string",
    "ucp_request": "omit"
  }
}
```

Valid operations: `create`, `read`, `update`, `complete`.

### Schema Composition

UCP payloads are self-describing — they embed `ucp.capabilities` metadata declaring which schemas apply. This lets multiple capability schemas compose into one:

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
  "id": "chk_123",
  "discounts": [...]
}
```

**How composition works:**

1. **Root capability** — one capability has no `extends`, providing the base schema
2. **Extensions** — capabilities with `extends` add fields to the root
3. **Merge** — extensions define their additions in `$defs[root_capability_name]`; the tool composes them via `allOf`

**Graph rules:** exactly one root capability (no `extends`), all `extends` targets must exist in capabilities, all extensions must transitively reach the root.

**Schema authoring for extensions:**

Extension schemas define their additions in `$defs` keyed by the root capability name:

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
            "discounts": { "type": "array" }
          }
        }
      ]
    }
  }
}
```

### Validation Modes

The validator supports four patterns for discovering which schema to validate against.

**Response (self-describing)** — The payload's `ucp.capabilities` declares schema URLs. Direction is auto-detected as response:

```bash
ucp-schema validate response.json --op read --schema-local-base ./schemas
```

**JSONRPC request** — The envelope has `meta.profile` at root, with the payload nested under the capability short name (e.g., `checkout`). The validator fetches the profile, extracts capabilities, extracts the nested payload, then composes and validates:

```json
{
  "meta": { "profile": "https://agent.example.com/.well-known/ucp" },
  "checkout": { "line_items": [...] }
}
```

```bash
ucp-schema validate envelope.json --op create
```

**REST request (`--profile`)** — The profile URL comes via flag (equivalent to an HTTP header in production). The payload is the raw object, not wrapped in an envelope:

```bash
ucp-schema validate raw-checkout.json --profile agent-profile.json --op create
```

The `--profile` flag implies `--request` direction.

**Explicit schema** — Bypass self-describing metadata entirely. Requires explicit `--request` or `--response`:

```bash
ucp-schema validate order.json --schema checkout.json --request --op create
```

#### Local Resolution

When working offline or testing schema changes, `--schema-local-base` maps schema URL paths to local files:

```bash
# Schema URL: https://ucp.dev/schemas/shopping/checkout.json
# Path extracted: /schemas/shopping/checkout.json
# Local file: ./local/schemas/shopping/checkout.json
ucp-schema validate response.json --schema-local-base ./local --op read
```

When schema URLs have a prefix that doesn't match your local directory layout, `--schema-remote-base` strips it:

```bash
# URL:   https://ucp.dev/draft/schemas/shopping/checkout.json
# Strip: https://ucp.dev/draft
# Local: ./site/schemas/shopping/checkout.json
ucp-schema validate response.json \
  --schema-local-base ./site \
  --schema-remote-base "https://ucp.dev/draft" \
  --op read
```

### Bundling

Schemas often use `$ref` to reference external files. The `--bundle` flag inlines all external references into a self-contained schema:

```bash
ucp-schema resolve checkout.json --request --op create --bundle --pretty
```

Bundling applies to **schema file input only**. When resolving payloads, composition already handles fetching and merging external schemas.

How it works:

- File refs (`"$ref": "types/buyer.json"`) are loaded and inlined
- Fragment refs (`"$ref": "types/common.json#/$defs/address"`) navigate to the target definition
- Internal refs in external files (`"$ref": "#/$defs/foo"`) resolve against their source file
- Self-referential types (`"$ref": "#"`) are preserved (can't be inlined)
- Circular references are detected and reported as errors

### Strict Mode

By default, validation allows unknown fields — payloads may contain fields from capabilities the validator hasn't seen, and forward compatibility requires tolerating them. For closed systems or catching typos, `--strict` injects `additionalProperties: false` into all object schemas:

```bash
ucp-schema validate order.json --schema schema.json --request --op create --strict
ucp-schema resolve schema.json --request --op create --strict --pretty
```

**Warning:** Strict mode conflicts with `allOf` composition. Each `allOf` branch validates independently and rejects properties from other branches. Use default (non-strict) mode for composed schemas.

## Debugging with `--verbose`

All commands accept `--verbose` (or `-v`) to print pipeline stages to stderr:

```bash
$ ucp-schema resolve response.json --op read --schema-local-base ./schemas --verbose
[load] reading response.json
[detect] payload with 3 capabilities (1 root, 2 extensions)
[detect]   root dev.ucp.shopping.checkout → https://ucp.dev/schemas/shopping/checkout.json
[detect]   ext dev.ucp.shopping.discount → https://ucp.dev/schemas/shopping/discount.json
[detect]   ext dev.ucp.shopping.fulfillment → https://ucp.dev/schemas/shopping/fulfillment.json
[compose] composing schemas from payload capabilities
[resolve] resolving for response/read
```

Verbose output goes to stderr; JSON output on stdout is unaffected.

## More Information

See [FAQ.md](./FAQ.md) for common questions about validator behavior, design decisions, and edge cases.

## License

Apache-2.0
