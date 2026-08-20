.PHONY: help build check test fmt fmt-check clippy verify plan

help:
	@echo "boringbuilder"
	@echo ""
	@echo "Targets:"
	@echo "  build        Build the Rust binary"
	@echo "  check        Run cargo check"
	@echo "  test         Run cargo test"
	@echo "  fmt          Format Rust code"
	@echo "  fmt-check    Check Rust formatting"
	@echo "  clippy       Run Clippy with warnings denied"
	@echo "  verify       Run fmt-check, clippy, and test"
	@echo "  plan         Resolve the example build without running it"

build:
	cargo build

check:
	cargo check

test:
	cargo test

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all --check

clippy:
	cargo clippy --all-targets -- -D warnings

verify: fmt-check clippy test

plan:
	cargo run -- build -f examples/artifact.yml --dry-run
