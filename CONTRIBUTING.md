# Contributing to ucp-schema

Thank you for your interest in contributing to **ucp-schema**! This guide will
help you get started.

## Table of Contents

- [Code of Conduct](#code-of-conduct)
- [Getting Started](#getting-started)
- [Development Workflow](#development-workflow)
- [Pull Request Process](#pull-request-process)
- [Coding Standards](#coding-standards)
- [Testing](#testing)
- [Contributor License Agreement](#contributor-license-agreement)

## Code of Conduct

This project follows the
[Universal Commerce Protocol Code of Conduct](https://github.com/Universal-Commerce-Protocol/ucp/blob/main/CODE_OF_CONDUCT.md).
By participating, you are expected to uphold this code.

## Getting Started

### Prerequisites

- **Rust** 1.70 or later (see `rust-version` in `Cargo.toml`)
- **Cargo** (included with Rust)
- **Git**

Install Rust via [rustup](https://rustup.rs/):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### Clone and Build

```bash
git clone https://github.com/Universal-Commerce-Protocol/ucp-schema.git
cd ucp-schema
cargo build
```

### Verify Your Setup

```bash
make all    # Runs fmt-check, lint, test, build
```

## Development Workflow

### Makefile Targets

| Target             | Description                             |
| ------------------ | --------------------------------------- |
| `make all`         | Format check, lint, test, build         |
| `make build`       | Build debug binary                      |
| `make release`     | Build optimized release binary          |
| `make test`        | Run all tests                           |
| `make test-unit`   | Run unit tests only (`--lib`)           |
| `make test-integration` | Run CLI integration tests          |
| `make lint`        | Run `cargo clippy` with `-D warnings`   |
| `make fmt`         | Auto-format code with `rustfmt`         |
| `make fmt-check`   | Check formatting without modifying      |
| `make smoke-test`  | Quick test with checkout fixture        |
| `make install`     | Install release binary to `~/.cargo/bin`|
| `make clean`       | Remove build artifacts                  |

### Feature Flags

- **`remote`** (default): Enables HTTP-based schema loading via `reqwest`.
  Disable with `--no-default-features` for offline-only builds.

```bash
# Build without remote support
cargo build --no-default-features

# Run tests without remote support
cargo test --no-default-features
```

### Project Layout

```
src/
├── bin/
│   └── ucp-schema.rs   # CLI entry point (clap)
├── compose.rs           # Schema composition from capabilities
├── error.rs             # Error types (thiserror)
├── lib.rs               # Public library API
├── linter.rs            # Static analysis / diagnostics
├── loader.rs            # Schema loading (file, URL, string)
├── resolver.rs          # UCP annotation resolution
├── types.rs             # Core types (Direction, Visibility, etc.)
└── validator.rs         # Payload validation against schemas
tests/
├── cli_test.rs          # CLI integration tests
├── resolve_test.rs      # Resolver unit tests
└── fixtures/            # Test schemas and payloads
```

## Pull Request Process

1. **Fork** the repository and create a feature branch from `main`.
2. **Follow conventional commits** for your commit messages:
   - `feat:` – new features
   - `fix:` – bug fixes
   - `docs:` – documentation only
   - `chore:` – maintenance, CI, deps
   - `test:` – adding or updating tests
   - `refactor:` – code restructuring without behavior change
3. **Ensure all checks pass** before submitting:
   ```bash
   make all
   ```
4. **Open a Pull Request** against `main` with a clear description of what
   changed and why.
5. **Address review feedback** promptly. Maintainers may request changes before
   merging.

### PR Categories

When opening a PR, indicate which area your change affects:

- **Core Protocol** – `src/` changes (resolver, composer, validator, linter)
- **Infrastructure** – CI workflows, Makefile, Cargo.toml
- **Documentation** – README, FAQ, contributing guides
- **UCP Schema** – Test fixtures, schema definitions
- **Community Health** – `.github/` configuration, templates

## Coding Standards

- **Format** all code with `cargo fmt` before committing.
- **No warnings** – `cargo clippy -- -D warnings` must pass.
- **Error handling** – Use `thiserror` derive macros; avoid `.unwrap()` in
  library code.
- **Documentation** – Add `///` doc comments to all public functions and types.
- **Dependencies** – Minimize new dependencies. Discuss additions in the PR.

## Testing

- **Unit tests** go in the same file as the code they test, inside a
  `#[cfg(test)] mod tests` block.
- **Integration tests** go in `tests/` and exercise the CLI via `assert_cmd`.
- **Fixtures** go in `tests/fixtures/` with descriptive names.
- All new features and bug fixes should include tests.

```bash
# Run everything
cargo test

# Run a specific test
cargo test test_name

# Run with output
cargo test -- --nocapture
```

## Contributor License Agreement

Contributions to this project must be accompanied by a
[Contributor License Agreement](https://cla.developers.google.com/about) (CLA).
You (or your employer) retain the copyright to your contribution; the CLA gives
us permission to use and redistribute your contributions as part of the project.

Visit <https://cla.developers.google.com/> to see your current agreements on
file or to sign a new one. You generally only need to submit a CLA once.
