#!/bin/sh
# Installs the eBPF toolchain: the pinned nightly with rust-src and clippy, and the prebuilt
# bpf-linker (`cargo install` needs a matching LLVM, bpf-linker#305).
# observer/build.rs and the Makefile read NIGHTLY from here. Needs rustup, curl and zstd.
set -eu

NIGHTLY=nightly-2026-11-11
BPF_LINKER=v0.13.0

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)
    asset=x86_64-unknown-linux-musl
    sha256=0e9b2b30f68898177f1cb6f0ecbfa909a82479473a49a0ef66529fd088e3e410 ;;
  Linux-aarch64)
    asset=aarch64-unknown-linux-musl
    sha256=248e25f5c6a7242b054bd8f705470abb7a5a79b2b7c07df347c5d7049b783452 ;;
  Darwin-arm64)
    asset=aarch64-apple-darwin
    sha256=a4882533f5d423f8397ba431256d1cef45a3e13dbfc359decafee3800ee90e55 ;;
  *)
    echo "no prebuilt bpf-linker for $(uname -s)-$(uname -m)" >&2
    exit 1 ;;
esac

rustup toolchain install "$NIGHTLY" --profile minimal --component rust-src,clippy

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL -o "$tmp/bpf-linker.tar.zst" \
  "https://github.com/aya-rs/bpf-linker/releases/download/$BPF_LINKER/bpf-linker-$asset.tar.zst"
if command -v sha256sum >/dev/null; then
  echo "$sha256  $tmp/bpf-linker.tar.zst" | sha256sum -c -
else
  echo "$sha256  $tmp/bpf-linker.tar.zst" | shasum -a 256 -c -
fi
bin="${CARGO_HOME:-$HOME/.cargo}/bin"
zstd -dc "$tmp/bpf-linker.tar.zst" | tar -x -C "$bin" bpf-linker
"$bin/bpf-linker" --version
