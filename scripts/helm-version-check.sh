#!/usr/bin/env bash
# Require a higher Chart.yaml version when chart files other than *.md change against a base
# ref. `make helm-version-check [BASE=origin/main]` and CI run this same script.
# Env overrides: CHART (default helm/noisy-neighbour-observer), BASE (default origin/main).
set -euo pipefail
cd "$(dirname "$0")/.."

CHART="${CHART:-helm/noisy-neighbour-observer}"
BASE="${BASE:-origin/main}"

# Fetch the base ref if it is not known locally.
if ! git rev-parse --verify --quiet "$BASE" >/dev/null; then
    git fetch --no-tags origin "${BASE#origin/}"
fi

changed="$(git diff --name-only "$BASE"...HEAD -- "$CHART" | grep -v '\.md$' || true)"
if [ -z "$changed" ]; then
    echo "No chart changes that affect the render vs $BASE; version bump not required."
    exit 0
fi

ver() { awk -F'[ "]+' '/^version:/{print $2; exit}'; }
head_ver="$(ver < "$CHART/Chart.yaml")"

if git cat-file -e "$BASE:$CHART/Chart.yaml" 2>/dev/null; then
    base_ver="$(git show "$BASE:$CHART/Chart.yaml" | ver)"
else
    echo "Chart is new vs $BASE ($head_ver); version bump check skipped."
    exit 0
fi

semver='^[0-9]+\.[0-9]+\.[0-9]+$'
highest="$(printf '%s\n%s\n' "$base_ver" "$head_ver" | sort -V | tail -n 1)"
if ! [[ "$head_ver" =~ $semver ]] || [ "$head_ver" = "$base_ver" ] || [ "$highest" != "$head_ver" ]; then
    msg="chart files changed, so 'version:' in $CHART/Chart.yaml must be a SemVer above $base_ver, not $head_ver."
    # A GitHub Actions annotation in CI, plain text locally.
    if [ -n "${GITHUB_ACTIONS:-}" ]; then
        echo "::error file=$CHART/Chart.yaml::$msg"
    fi
    echo "ERROR: $msg" >&2
    exit 1
fi
echo "Chart version bumped: $base_ver -> $head_ver"
