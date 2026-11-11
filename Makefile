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

# The eBPF crate only builds for the BPF target, with the nightly that the script pins.
EBPF_TOOLCHAIN = $(shell sed -n 's/^NIGHTLY=//p' scripts/install-ebpf-toolchain.sh)

.PHONY: test
test: ## Run all tests
	cargo test --locked

.PHONY: lint
lint: ## clippy (deny warnings) + rustfmt check
	cargo clippy --all-targets --locked -- -D warnings
	cargo +$(EBPF_TOOLCHAIN) clippy -p observer-ebpf --target bpfel-unknown-none -Z build-std=core --locked -- -D warnings
	cargo fmt --all --check

.PHONY: fmt
fmt: ## Apply rustfmt
	cargo fmt --all

.PHONY: ci
ci: lint test ## What CI runs

# Runs the test binary through sudo, unless make already runs as root.
SUDO_RUNNER = $(if $(filter 0,$(shell id -u)),,CARGO_TARGET_$(shell rustc -vV | sed -n 's/^host: //p' | tr 'a-z-' 'A-Z_')_RUNNER="sudo -E")

.PHONY: integration
integration: ## Run the eBPF integration test (Linux, as root through sudo)
	$(SUDO_RUNNER) cargo test --locked -p observer --test runqueue -- --ignored --nocapture

.PHONY: ebpf-toolchain
ebpf-toolchain: ## Install the pinned nightly and the prebuilt bpf-linker that build the eBPF object
	scripts/install-ebpf-toolchain.sh

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
