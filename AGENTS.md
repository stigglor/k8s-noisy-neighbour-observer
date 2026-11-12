# AGENTS.md

Operational guide for work on the observer, by an AI coding agent or by hand. For the design see [`CLAUDE.md`](./CLAUDE.md). For usage see [`README.md`](./README.md).

## Setup

The eBPF object needs a pinned nightly with `rust-src`, and `bpf-linker`. `observer/build.rs` builds the object as part of the agent build.

1. Install rustup and `zstd`.
2. Run `make ebpf-toolchain`. It installs the pinned nightly and the prebuilt `bpf-linker`, and checks its sha256.

Do not run `cargo install bpf-linker`. It needs a matching LLVM and fails on macOS (bpf-linker#305).

## Commands

The `Makefile` is the source of truth for which checks must pass.

| Command | Use |
|---|---|
| `make ci` | Run before every push: lint and test |
| `make lint` | clippy with `-D warnings`, and `rustfmt --check` |
| `make fmt` | Apply rustfmt |
| `make test` | Run all tests |
| `make integration` | Load the probes and run the run queue test (Linux, root through sudo) |
| `make ebpf-toolchain` | Install the pinned nightly and `bpf-linker` |
| `make build` / `make run` | Build in release mode, or run the agent locally |
| `make docker-build` | Build the image for one platform (`PLATFORM=` `IMAGE=`) |
| `make preflight` | Check that the docker engine kernel has BTF and cgroup v2 |

## Repository map

```
Cargo.toml                # workspace and release profile
observer-common/          # no_std #[repr(C)] types shared by the programs and the agent
observer-ebpf/            # no_std eBPF programs, built by observer/build.rs
observer/                 # agent library and binary (noisy-neighbour-observer)
  build.rs                # builds observer-ebpf with the pinned nightly
  src/main.rs             # entrypoint: config, attach, serve, graceful shutdown
  src/lib.rs              # modules, so integration tests can drive them
  src/config.rs           # NNO_* environment config
  src/offsets.rs          # kernel struct offsets from vmlinux BTF
  src/bpf.rs              # load, attach and read the BPF maps (Linux only)
  src/cgroup.rs           # cgroup id to pod UID and container id, from the cgroup tree
  src/k8s.rs              # pod watch on the node, and the join of cgroups to entities
  src/workload.rs         # workload of a pod from its owner reference, and the entity labels
  src/cardinality.rs      # series budget and the sticky _overflow assignment
  src/runq.rs             # monotonic totals for each entity from map reads
  src/telemetry.rs        # JSON logs
  src/metrics.rs          # Prometheus registry and the histogram collector
  src/routes.rs           # /metrics, /healthz, /version
  tests/runqueue.rs       # integration test, ignored by default
scripts/                  # install-ebpf-toolchain.sh
Dockerfile                # static musl image on distroless
```

## Invariants

- Cardinality comes first. Every new metric and label needs the review in `CLAUDE.md`.
- Never move an entity between label sets. A move makes a counter fall.
- Synthetic label values (`_system`, `_overflow`, `_pod`) start with `_`, which no Kubernetes name can.
- Config comes from `NNO_*` environment variables. An invalid value must stop the agent at startup.
- The agent attaches the probes before it binds the listener. A failure must stop the agent with a non-zero exit.
- `NIGHTLY` in `scripts/install-ebpf-toolchain.sh` is the only nightly pin. `observer/build.rs` and the `Makefile` read it.
- `aya` builds only on Linux, so a lint on macOS skips `bpf.rs` and the probe thread in `main.rs`. CI lints them on Linux.
- The eBPF programs read kernel memory only through offsets from `src/offsets.rs`.
- Committed prose must contain no em dash, no en dash and no spaced hyphen aside.
- `tasks/` is local scratch and must never be committed.
- Commits are signed and follow Conventional Commits.
