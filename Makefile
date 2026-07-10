.DEFAULT_GOAL := help
.PHONY: help build build-release check lint fmt fmt-check test image push build-aarch64 image-aarch64 push-aarch64 clean

IMAGE         ?= registry.helles.farm/crypto-collector:latest
IMAGE_AARCH64 ?= registry.helles.farm/crypto-collector:aarch64

# Deploy target (override for other clusters/namespaces).
KUBECTL    ?= kubectl
NAMESPACE  ?= finance
DEPLOYMENT ?= crypto-collector
ROLLOUT_TIMEOUT ?= 180s

# Container engine: prefer docker, fall back to podman (this project standardises
# on podman). `cross` reads CROSS_CONTAINER_ENGINE to pick its build container,
# defaulting to docker; exporting it keeps `build-aarch64` working on podman-only
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

# ── Container image ──────────────────────────────────────────────────────────

image: ## Build container image (native arch)
	$(CONTAINER_ENGINE) build -f Dockerfile -t $(IMAGE) .

push: image ## Build and push native image
	$(CONTAINER_ENGINE) push $(IMAGE)

# ── aarch64 cross-compilation ────────────────────────────────────────────────

build-aarch64: ## Cross-compile binary for aarch64 (requires `cross`)
	cross build --target aarch64-unknown-linux-gnu --release

image-aarch64: build-aarch64 ## Build aarch64 container image using pre-compiled binary (no QEMU required)
	$(CONTAINER_ENGINE) build -f Dockerfile.aarch64 -t $(IMAGE_AARCH64) .

push-aarch64: image-aarch64 ## Build and push aarch64 image
	$(CONTAINER_ENGINE) push $(IMAGE_AARCH64)

.PHONY: deploy
deploy: push-aarch64 ## Gated build+push, then rollout restart and wait (fail-fast)
	$(KUBECTL) -n $(NAMESPACE) rollout restart deploy/$(DEPLOYMENT)
	$(KUBECTL) -n $(NAMESPACE) rollout status deploy/$(DEPLOYMENT) --timeout=$(ROLLOUT_TIMEOUT)

# ── Clean ────────────────────────────────────────────────────────────────────

clean: ## Remove build artefacts
	cargo clean
