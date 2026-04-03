# Security Policy

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| 1.x     | :white_check_mark: |
| < 1.0   | :x:                |

## Reporting a Vulnerability

If you discover a security vulnerability in **ucp-schema**, please report it
responsibly.

**Do NOT open a public GitHub issue for security vulnerabilities.**

Instead, please use one of the following methods:

1. **GitHub Security Advisories** (preferred): Navigate to the
   [Security Advisories](https://github.com/Universal-Commerce-Protocol/ucp-schema/security/advisories/new)
   page and create a new private advisory.

2. **Email**: Contact the maintainers at the email addresses listed in the
   [CODEOWNERS](.github/CODEOWNERS) file or through the
   [Universal Commerce Protocol](https://github.com/Universal-Commerce-Protocol)
   organization.

## What to Include

When reporting a vulnerability, please include:

- A description of the vulnerability and its potential impact.
- Steps to reproduce the issue.
- Any relevant logs, screenshots, or proof-of-concept code.
- Affected versions (if known).

## Response Timeline

- **Acknowledgment**: Within 3 business days of receiving the report.
- **Assessment**: Within 10 business days, we will provide an initial assessment
  of the vulnerability.
- **Resolution**: We aim to release a fix within 30 days for confirmed
  vulnerabilities, depending on complexity.

## Scope

This security policy covers the `ucp-schema` CLI tool and Rust library,
including:

- JSON Schema resolution and composition logic
- Schema validation
- File and URL loading (when the `remote` feature is enabled)
- CLI argument handling

## Recognition

We appreciate responsible disclosure and will acknowledge reporters in the
release notes (unless anonymity is preferred).
