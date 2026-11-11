# Static musl binary on distroless, for linux/amd64 and linux/arm64. The builder runs on
# $BUILDPLATFORM and cross-compiles with cargo-zigbuild, so no QEMU is needed.

# ---- builder ----
FROM --platform=$BUILDPLATFORM rust:1.94-bookworm@sha256:6ae102bdbf528294bc79ad6e1fae682f6f7c2a6e6621506ba959f9685b308a55 AS builder

ARG TARGETARCH
RUN apt-get update \
    && apt-get install -y --no-install-recommends python3-pip \
    && pip3 install --break-system-packages --no-cache-dir cargo-zigbuild ziglang \
    && rm -rf /var/lib/apt/lists/* \
    && case "$TARGETARCH" in \
         amd64) echo x86_64-unknown-linux-musl > /rust_target ;; \
         arm64) echo aarch64-unknown-linux-musl > /rust_target ;; \
         *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
       esac \
    && rustup target add "$(cat /rust_target)"

WORKDIR /build

COPY . .
# The commit served at /version. Declared after COPY so a new sha keeps the toolchain layers cached.
ARG NNO_GIT_SHA
# On arm64, jemalloc assumes 64 KiB pages, so it also runs on 64 KiB page kernels. The registry
# cache serves both platform builds, so cargo must hold a lock to unpack into it.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/build/target,id=target-$TARGETARCH \
    if [ "$TARGETARCH" = arm64 ]; then export JEMALLOC_SYS_WITH_LG_PAGE=16; fi \
    && RUST_TARGET="$(cat /rust_target)" \
    && cargo zigbuild --locked --release --bin noisy-neighbour-observer --target "$RUST_TARGET" \
    && cp "target/$RUST_TARGET/release/noisy-neighbour-observer" /usr/local/bin/noisy-neighbour-observer

# ---- runtime ----
# Root variant on purpose: the agent needs CAP_BPF, and Kubernetes gives added capabilities
# no effect for a non-root user.
FROM gcr.io/distroless/static-debian12@sha256:d75cdd72874d4790092fcb1b058493ecf6bb5bf2b2b897045b00ff01d91843f2
COPY --from=builder /usr/local/bin/noisy-neighbour-observer /usr/local/bin/noisy-neighbour-observer

EXPOSE 9400
ENTRYPOINT ["/usr/local/bin/noisy-neighbour-observer"]
