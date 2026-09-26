.DEFAULT_GOAL := help
.PHONY: help build build-release check lint fmt fmt-check test image push rust-build rust-build-amd64 rust-build-arm64 clean

TAG       ?= latest
# Image platforms. aarch64 is opt-in: `make push PLATFORMS=linux/amd64,linux/arm64`.
PLATFORMS ?= linux/amd64
IMAGE     ?= registry.helles.farm/crypto-collector:$(TAG)

# Deploy target (override for other clusters/namespaces).
KUBECTL    ?= kubectl
NAMESPACE  ?= finance
DEPLOYMENT ?= crypto-collector
ROLLOUT_TIMEOUT ?= 180s

# Container engine: prefer docker, fall back to podman (this project standardises
# on podman). `cross` reads CROSS_CONTAINER_ENGINE to pick its build container,
# defaulting to docker; exporting it keeps `rust-build-arm64` working on podman-only
# hosts without manual configuration.
CONTAINER_ENGINE ?= $(shell command -v docker >/dev/null 2>&1 && echo docker || echo podman)
export CROSS_CONTAINER_ENGINE ?= $(CONTAINER_ENGINE)

# ── Help ─────────────────────────────────────────────────────────────────────

help: ## Show available targets
	@grep -E '^[a-zA-Z0-9_-]+:.*## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-22s\033[0m %s\n", $$1, $$2}'

# ── Source ───────────────────────────────────────────────────────────────────

build: ## Compile (debug)
	cargo build

build-release: ## Compile (release)
	cargo build --release

check: ## Cargo check (no codegen)
	cargo check --all-targets --all-features

lint: ## fmt-check + clippy -D warnings + helm lint --strict
	cargo fmt --check
	cargo clippy --all-targets --all-features -- -D warnings
	helm lint --strict charts/crypto-collector

fmt: ## Format source code
	cargo fmt --all

fmt-check: ## Check formatting (CI)
	cargo fmt --all -- --check

upgrade: ## Upgrade crates
	cargo upgrade --incompatible
	cargo update
	$(MAKE) check lint test

# ── Unit tests ───────────────────────────────────────────────────────────────

test: ## Run unit tests
	cargo test

# ── Release binaries (one per target platform) ───────────────────────────────
# Both binaries are compiled on the host so the image build stays COPY-only and
# never needs QEMU.

rust-build-amd64: ## Compile release binary for x86_64 (native, no `cross` needed)
	cargo build --release --target x86_64-unknown-linux-gnu

rust-build-arm64: ## Cross-compile release binary for aarch64 (requires `cross`)
	cross build --release --target aarch64-unknown-linux-gnu

rust-build: rust-build-amd64 $(if $(findstring arm64,$(PLATFORMS)),rust-build-arm64) ## Compile release binaries for $(PLATFORMS)

# ── Container image ──────────────────────────────────────────────────────────

image: ## Build image for $(PLATFORMS) (default amd64) into a local manifest
	$(CONTAINER_ENGINE) build --platform $(PLATFORMS) \
		--manifest $(IMAGE) -f Dockerfile .

push: lint test rust-build image ## Gated build for $(PLATFORMS), then push the manifest
	$(CONTAINER_ENGINE) manifest push --all $(IMAGE) docker://$(IMAGE)

.PHONY: deploy
deploy: push ## Gated build+push, then rollout restart and wait (fail-fast)
	$(KUBECTL) -n $(NAMESPACE) rollout restart deploy/$(DEPLOYMENT)
	$(KUBECTL) -n $(NAMESPACE) rollout status deploy/$(DEPLOYMENT) --timeout=$(ROLLOUT_TIMEOUT)

# ── Clean ────────────────────────────────────────────────────────────────────

clean: ## Remove build artefacts
	cargo clean
