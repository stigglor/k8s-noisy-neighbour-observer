#!/usr/bin/env bash
# Lints the Grafana dashboard with the pinned dashboard-linter and checks that its JSON is
# normalised by `jq -S`. Usage: dashboard-lint.sh [fmt], also as `make dashboard-lint|dashboard-fmt`.
set -euo pipefail
cd "$(dirname "$0")/.."

DASHBOARD="deploy/dashboard/noisy-neighbour.json"
LINTER_VERSION="0.5.2"

# Prints the sha256 of the release archive for "<os>_<arch>", from the checksums.txt of the release.
linter_sha256() {
    case "$1" in
        linux_amd64) echo 108004b1020eac3f578752d3245def9180ea08667fe25d54a09a422039b13aab ;;
        linux_arm64) echo 7dae2f1882dc1dcedd9d65a54115b4d9a579579d8f7a4667b169fcb8508487b3 ;;
        darwin_amd64) echo 8b9f8e243ceadbab57f324044f160a2465dca661a20c69d4ffbd4c39783271d9 ;;
        darwin_arm64) echo 5650f91773b0fa96a0f9ad7cd778853f0848b63cfd80f30e467dcc22cc468af7 ;;
    esac
}

command -v jq >/dev/null 2>&1 || { echo "error: 'jq' is required but not installed."; exit 1; }

if [ "${1:-}" = "fmt" ]; then
    normalised="$(jq -S . "$DASHBOARD")"
    printf '%s\n' "$normalised" >"$DASHBOARD"
    exit 0
fi

echo "==> $DASHBOARD is normalised by jq -S"
if ! jq -S . "$DASHBOARD" | diff -u "$DASHBOARD" - >/dev/null; then
    jq -S . "$DASHBOARD" | diff -u "$DASHBOARD" - | head -40 || true
    echo "error: run 'make dashboard-fmt'."
    exit 1
fi

echo "==> the cAdvisor pod regex matches the pods of workload web and no other workload"
pod_regex="$(jq -r '.panels[].targets[]?.expr | select(test("container_cpu_cfs")) | capture("pod=~\"(?<re>[^\"]+)\"").re' \
    "$DASHBOARD" | head -1)"
pod_regex="^${pod_regex//\$workload/web}\$"
# Each line is "<want> <pod>": a Deployment, DaemonSet, StatefulSet, CronJob and bare Pod of web,
# then pods of web-api, webapp and a Deployment web-api.
while read -r want pod; do
    got=no
    grep -qE "$pod_regex" <<<"$pod" && got=yes
    [ "$got" = "$want" ] || { echo "error: '$pod' match $got, want $want (regex $pod_regex)"; exit 1; }
done <<'EOF'
yes web-7d9f8b6c5d-x7k2p
yes web-x7k2p
yes web-0
yes web-28471234-x7k2p
yes web
no web-api-0
no web-api-x7k2p
no webapp-x7k2p
no web-api-7d9f8b6c5d-x7k2p
EOF

os="$(uname -s | tr '[:upper:]' '[:lower:]')"
case "$(uname -m)" in
    x86_64 | amd64) arch=amd64 ;;
    aarch64 | arm64) arch=arm64 ;;
    *) echo "error: unsupported architecture $(uname -m)"; exit 1 ;;
esac
sha="$(linter_sha256 "${os}_${arch}")"
[ -n "$sha" ] || { echo "error: no dashboard-linter build for ${os}_${arch}"; exit 1; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
archive="dashboard-linter_${LINTER_VERSION}_${os}_${arch}.tar.gz"
curl -fsSLo "$tmp/$archive" \
    "https://github.com/grafana/dashboard-linter/releases/download/v${LINTER_VERSION}/${archive}"
if command -v sha256sum >/dev/null 2>&1; then
    echo "$sha  $tmp/$archive" | sha256sum -c --quiet -
else
    echo "$sha  $tmp/$archive" | shasum -a 256 -c --quiet -
fi
tar -xzf "$tmp/$archive" -C "$tmp" dashboard-linter

# The linter reads its exclusions from deploy/dashboard/.lint. The grep drops the header line
# that it prints for each rule, so only the findings remain.
echo "==> dashboard-linter v$LINTER_VERSION lint --strict"
status=0
output="$("$tmp/dashboard-linter" lint --strict "$DASHBOARD" 2>&1)" || status=$?
grep -v '^Checks that ' <<<"$output" || true
[ "$status" -eq 0 ] || exit "$status"
echo "==> PASS"
