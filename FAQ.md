# FAQ

## Are additional fields allowed by default?

**The validator checks that specced fields are correct, but allows additional fields by design.**

This is intentional:

- The schema defines minimum requirements, not an exhaustive contract
- Extensions add fields that base schemas don't know about
- Forward compatibility requires tolerating unknown fields
- Clients shouldn't break when servers add new fields

If a payload has `{ "id": "123", "custom_field": "foo" }` and the schema only defines `id`, validation passes. The `custom_field` is ignored—not rejected.

Use `--strict` only when you need a closed contract:

- Catching field name typos during development
- Systems where the schema is the complete specification

---

## What's the difference between `--schema` and `--schema-local-base`?

**They're different validation modes:**

| Flag                        | Mode                    | Schema source                               |
| --------------------------- | ----------------------- | ------------------------------------------- |
| (none)                      | Self-describing         | Fetches URLs from `ucp.capabilities`        |
| `--schema-local-base ./dir` | Self-describing + local | Maps capability URLs to local files         |
| `--schema file.json`        | Explicit                | Uses specified schema, ignores capabilities |

`--schema-local-base` is useful for:

- Offline testing
- Local development before publishing schemas
- Testing schema changes against real payloads

**How it works:** The flag extracts the URL path and maps it to a local file. This works for any domain—not just `ucp.dev`:

| Schema URL                                       | Local path (`--schema-local-base ./local`) |
| ------------------------------------------------ | ------------------------------------------ |
| `https://ucp.dev/schemas/shopping/checkout.json` | `./local/schemas/shopping/checkout.json`   |
| `https://extensions.3p.com/schemas/loyalty.json` | `./local/schemas/loyalty.json`             |

This means you can develop and test third-party extensions locally before publishing.

---

## How does direction auto-detection work?

**The validator infers direction from payload structure:**

| Payload has        | Detected direction                               |
| ------------------ | ------------------------------------------------ |
| `ucp.capabilities` | Response                                         |
| `meta.profile`     | Request                                          |
| Neither            | Error (must specify `--request` or `--response`) |

This applies to both `validate` and `resolve` when the input is a self-describing payload. When resolving a plain schema file, explicit `--request` or `--response` is required.

---

## Why did validation fail with "unknown visibility"?

**The validator fails fast on invalid annotations.** Valid values are: `"omit"`, `"required"`, `"optional"`.

```json
"id": { "ucp_request": "readonly" }  // Error: unknown visibility
```

Typos and version mismatches should surface immediately, not silently degrade to "include everything" behavior. If you see this error, either fix the typo or update your tooling.

---

## I sent an "omitted" field and validation failed. Why?

**Omit means "don't send this"—not just "we won't validate it."**

When a schema has `additionalProperties: false` and a field is omitted:

```json
{
  "additionalProperties": false,
  "properties": {
    "id": { "ucp_request": "omit" },
    "name": { "type": "string" }
  }
}
```

Sending `{ "name": "foo", "id": "123" }` for a request fails. The `id` field was removed from `properties`, making it an "additional property" that gets rejected.

**Why:** If the server generates `id`, clients shouldn't send it. The schema enforces this contract.

---

## How do I write an extension schema?

**Extensions must define their additions in `$defs[root_capability_name]`.** Composition happens at validation time. Each extension owns its additions but references the base it extends.

If `dev.ucp.shopping.checkout` is the root capability, your extension schema should look like:

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
            "discounts": { ... }
          }
        }
      ]
    }
  }
}
```

The validator:

1. Finds the root capability (no `extends`)
2. Extracts `$defs[root_name]` from each extension
3. Composes them with `allOf`

---

## Can extensions remove required fields from the base schema?

**No. Extensions can tighten requirements, not loosen them.**

This is JSON Schema semantics. With `allOf`, ALL branches must validate:

| Base     | Extension | Result                   |
| -------- | --------- | ------------------------ |
| omit     | required  | required                 |
| optional | required  | required                 |
| required | omit      | **required** (base wins) |
| required | optional  | **required** (base wins) |

If the base schema says `id` is required, clients already depend on it. An extension can't hide it without breaking those clients. Extensions add requirements; they don't remove them.
