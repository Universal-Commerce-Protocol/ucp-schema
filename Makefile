.PHONY: all build test test-unit test-integration lint fmt fmt-check clean release install smoke-test help

# Default target
all: fmt-check lint test build

# Build debug binary
build:
	cargo build

# Build release binary
release:
	cargo build --release

# Run all tests
test:
	cargo test

# Run only unit tests (faster, no CLI invocations)
test-unit:
	cargo test --lib

# Run only integration tests
test-integration:
	cargo test --test cli_test

# Lint with clippy
lint:
	cargo clippy -- -D warnings

# Format code
fmt:
	cargo fmt

# Check formatting (CI-friendly, fails if not formatted)
fmt-check:
	cargo fmt -- --check

# Clean build artifacts
clean:
	cargo clean

# Install to ~/.cargo/bin
install: release
	cargo install --path .

# Quick smoke test with checkout fixture
smoke-test: build
	@echo "=== Resolve checkout schema for create ==="
	./target/debug/ucp-schema resolve tests/fixtures/checkout.json --request --op create --pretty
	@echo "\n=== Resolve checkout schema for update ==="
	./target/debug/ucp-schema resolve tests/fixtures/checkout.json --request --op update --pretty

# Show help
help:
	@echo "Available targets:"
	@echo "  all              - fmt-check, lint, test, build (default)"
	@echo "  build            - Build debug binary"
	@echo "  release          - Build optimized release binary"
	@echo "  test             - Run all tests"
	@echo "  test-unit        - Run unit tests only"
	@echo "  test-integration - Run CLI integration tests only"
	@echo "  lint             - Run clippy linter"
	@echo "  fmt              - Format code with rustfmt"
	@echo "  fmt-check        - Check code formatting (CI)"
	@echo "  clean            - Remove build artifacts"
	@echo "  install          - Install release binary to ~/.cargo/bin"
	@echo "  smoke-test       - Quick manual test with checkout fixture"
