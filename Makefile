# Noisy neighbour observer, developer and CI ergonomics.
# `make help` lists targets.

IMAGE    ?= noisy-neighbour-observer:dev
# Single platform local build. Override with PLATFORM=linux/amd64.
PLATFORM ?= linux/arm64

.DEFAULT_GOAL := help

.PHONY: help
help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-22s\033[0m %s\n", $$1, $$2}'

.PHONY: build
build: ## cargo build --release
	cargo build --release

.PHONY: run
run: ## Run the agent locally
	cargo run

.PHONY: test
test: ## Run all tests
	cargo test --workspace --locked

.PHONY: lint
lint: ## clippy (deny warnings) + rustfmt check
	cargo clippy --workspace --all-targets --locked -- -D warnings
	cargo fmt --check

.PHONY: fmt
fmt: ## Apply rustfmt
	cargo fmt

.PHONY: ci
ci: lint test ## What CI runs

.PHONY: docker-build
docker-build: ## Build the container image for one platform ($(IMAGE)); override PLATFORM=linux/amd64
	docker build --platform $(PLATFORM) --build-arg NNO_GIT_SHA=$$(git rev-parse --short HEAD) -t $(IMAGE) .

.PHONY: preflight
preflight: ## Check that the docker engine kernel has BTF and cgroup v2 (needed by the eBPF agent)
	@docker run --rm alpine sh -c '\
		ok=0; \
		if [ -r /sys/kernel/btf/vmlinux ]; then echo "pass: /sys/kernel/btf/vmlinux present"; \
		else echo "fail: /sys/kernel/btf/vmlinux missing"; ok=1; fi; \
		if [ -e /sys/fs/cgroup/cgroup.controllers ]; then echo "pass: cgroup v2"; \
		else echo "fail: cgroup v2 not mounted"; ok=1; fi; \
		exit $$ok'
