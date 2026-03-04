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

cross-aarch64: ## Cross-compile for aarch64 (outputs to target/aarch64-unknown-linux-gnu/)
	@echo "Building aarch64 binaries via Docker..."
	@mkdir -p target/aarch64-unknown-linux-gnu/release
	@DOCKER_BUILDKIT=1 docker build \
		-f docker/Dockerfile.cross-aarch64 \
		--output type=local,dest=target/aarch64-unknown-linux-gnu/release \
		.
	@echo "✓ Artifacts in target/aarch64-unknown-linux-gnu/release/"

deploy-aarch64: cross-aarch64 ## Cross-compile and deploy to STRATA_DEPLOY_HOST via scp
	@test -n "$${STRATA_DEPLOY_HOST}" || { echo "Set STRATA_DEPLOY_HOST (SSH alias or user@host)"; exit 1; }
	@echo "Deploying to $${STRATA_DEPLOY_HOST}..."
	@scp target/aarch64-unknown-linux-gnu/release/strata-pipeline "$${STRATA_DEPLOY_HOST}:/tmp/strata-pipeline-new"
	@scp target/aarch64-unknown-linux-gnu/release/libgststrata.so "$${STRATA_DEPLOY_HOST}:~/.local/share/gstreamer-1.0/plugins/libgststrata.so"
	@ssh "$${STRATA_DEPLOY_HOST}" 'pkill strata-pipeline 2>/dev/null; sleep 1; mv /tmp/strata-pipeline-new /usr/local/bin/strata-pipeline && chmod 755 /usr/local/bin/strata-pipeline && setcap cap_net_raw+ep /usr/local/bin/strata-pipeline'
	@echo "✓ Deployed strata-pipeline + libgststrata.so to $${STRATA_DEPLOY_HOST}"

build: ## Build strata-pipeline (release)
	@cargo build --release -p strata-gst

install: build ## Build and install strata-pipeline with cap_net_raw for interface binding
	@echo "Installing strata-pipeline..."
	@sudo install -m 755 target/release/strata-pipeline /usr/local/bin/strata-pipeline
	@sudo setcap cap_net_raw+ep /usr/local/bin/strata-pipeline
	@echo "Installing libgststrata.so..."
	@mkdir -p ~/.local/share/gstreamer-1.0/plugins
	@cp target/release/libgststrata.so ~/.local/share/gstreamer-1.0/plugins/
	@echo "✓ strata-pipeline installed with cap_net_raw (SO_BINDTODEVICE enabled)"
	@echo "✓ libgststrata.so installed to ~/.local/share/gstreamer-1.0/plugins/"
