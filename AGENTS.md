# AGENTS.md

Operational guide for work on the observer, by an AI coding agent or by hand. For the design see [`CLAUDE.md`](./CLAUDE.md). For usage see [`README.md`](./README.md).

## Commands

The `Makefile` is the source of truth for which checks must pass.

| Command | Use |
|---|---|
| `make ci` | Run before every push: lint and test |
| `make lint` | clippy with `-D warnings`, and `rustfmt --check` |
| `make fmt` | Apply rustfmt |
| `make test` | Run all tests |
| `make build` / `make run` | Build in release mode, or run the agent locally |
| `make docker-build` | Build the image for one platform (`PLATFORM=` `IMAGE=`) |
| `make preflight` | Check that the docker engine kernel has BTF and cgroup v2 |

## Repository map

```
Cargo.toml                # workspace and release profile
observer/                 # agent binary (noisy-neighbour-observer)
  src/main.rs             # entrypoint: config, serve, graceful shutdown
  src/config.rs           # NNO_* environment config
  src/telemetry.rs        # JSON logs
  src/metrics.rs          # Prometheus registry
  src/routes.rs           # /metrics, /healthz, /version
Dockerfile                # static musl image on distroless
```

## Invariants

- Cardinality comes first. Every new metric and label needs the review in `CLAUDE.md`.
- Config comes from `NNO_*` environment variables. An invalid value must stop the agent at startup.
- Committed prose must contain no em dash, no en dash and no spaced hyphen aside.
- `tasks/` is local scratch and must never be committed.
- Commits are signed and follow Conventional Commits.
