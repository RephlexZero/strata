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

fmt: ## Format code with rustfmt
	@cargo fmt --all

lint: ## Clippy (includes compilation check)
	@cargo clippy --workspace --all-targets -- -D warnings

test: ## Run unit tests
	@cargo test --workspace --lib

test-all: ## Run all tests including integration and doc
	@cargo test --workspace --lib
	@cargo test --workspace --doc

test-integration: ## Run network simulation tests (requires sudo)
	@sudo -E env "PATH=$$PATH" cargo test -p strata-sim --test tier3_netem -- --nocapture

pre-push: ## Run what CI runs (format + clippy + tests)
	@cargo fmt --all -- --check
	@cargo clippy --workspace --all-targets -- -D warnings
	@cargo test --workspace --lib

version-check: ## Check version consistency across crates
	@./scripts/check-version-consistency.sh

release-check: ## Full release verification
	@echo "=== Release Pre-flight ==="
	@echo ""
	@echo "1. Format..."
	@cargo fmt --all -- --check
	@echo "✓ OK"
	@echo ""
	@echo "2. Clippy..."
	@cargo clippy --workspace --all-targets -- -D warnings
	@echo "✓ OK"
	@echo ""
	@echo "3. Tests..."
	@cargo test --workspace --lib
	@echo "✓ OK"
	@echo ""
	@echo "4. Versions..."
	@./scripts/check-version-consistency.sh
	@echo ""
	@echo "=== ✓ Ready for release ==="

clean: ## Clean build artifacts
	@cargo clean
