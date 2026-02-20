# Strata Development Makefile
# Quick commands for common development tasks

.PHONY: help check test fmt lint pre-commit pre-push install-hooks

help: ## Show this help message
	@echo "Strata Development Commands:"
	@echo ""
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2}'

install-hooks: ## Install git hooks
	@echo "Installing git hooks..."
	@git config core.hooksPath .githooks
	@chmod +x .githooks/pre-commit .githooks/pre-push
	@echo "✓ Git hooks installed"

check: ## Run cargo check (fast compilation check)
	@echo "Checking compilation..."
	@cargo check --workspace

fmt: ## Format code with rustfmt
	@echo "Formatting code..."
	@cargo fmt --all

fmt-check: ## Check code formatting without modifying files
	@echo "Checking format..."
	@cargo fmt --all -- --check

lint: ## Run clippy lints
	@echo "Running clippy..."
	@cargo clippy --workspace --all-targets -- -D warnings

test: ## Run unit and doc tests
	@echo "Running tests..."
	@cargo test --workspace --lib
	@cargo test --workspace --doc

test-integration: ## Run integration tests (requires sudo/NET_ADMIN)
	@echo "Running integration tests..."
	@cargo test -p strata-sim --test tier3_netem

test-all: ## Run all tests including ignored ones
	@cargo test --workspace

pre-commit: check lint ## Run pre-commit checks (fast)

pre-push: fmt-check check lint test ## Run pre-push checks (comprehensive)

version-check: ## Check version consistency across crates
	@./scripts/check-version-consistency.sh

ci: fmt-check check lint test ## Run full CI checks locally

clean: ## Clean build artifacts
	@cargo clean

fresh: clean ## Clean and rebuild everything
	@cargo build --workspace

# Release helpers
release-check: ## Verify everything is ready for release
	@echo "=== Release Pre-flight Checks ==="
	@echo ""
	@echo "1. Format check..."
	@cargo fmt --all -- --check
	@echo "✓ Format OK"
	@echo ""
	@echo "2. Clippy..."
	@cargo clippy --workspace --all-targets -- -D warnings
	@echo "✓ Clippy OK"
	@echo ""
	@echo "3. Tests..."
	@cargo test --workspace --lib
	@cargo test --workspace --doc
	@echo "✓ Tests OK"
	@echo ""
	@echo "4. Version consistency..."
	@./scripts/check-version-consistency.sh
	@echo ""
	@echo "=== ✓ Ready for release ==="
