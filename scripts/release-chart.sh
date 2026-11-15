#!/usr/bin/env bash
# Release checks and the chart push of the release workflow. The local dry run uses the same script.
# Usage: release-chart.sh versions <chart dir> <X.Y.Z> | check <chart dir or package dir> <oci registry>
#        | push <package dir> <oci registry>. Env: GITHUB_OUTPUT, GH_TOKEN (both optional).
set -euo pipefail

mode="${1:?mode: versions, check or push}"

# The chart directory itself, or the one package in a directory.
source_of() {
    if [ -f "$1/Chart.yaml" ]; then
        echo "$1"
        return
    fi
    local packages=("$1"/*.tgz)
    if [ "${#packages[@]}" -ne 1 ] || [ ! -f "${packages[0]}" ]; then
        echo "::error::$1 must hold exactly one chart package" >&2
        exit 1
    fi
    echo "${packages[0]}"
}

field() { awk -v key="$1:" '$1 == key {gsub(/"/, "", $2); print $2; exit}'; }

# A new GHCR package answers denied, not not found, so ask the GitHub API whether it exists.
ghcr_package_missing() {
    local path="${1#oci://ghcr.io/}/$2" owner type
    owner="${path%%/*}"
    [ -n "${GH_TOKEN:-}" ] || return 1
    type="$(curl -fsS -H "Authorization: Bearer $GH_TOKEN" "https://api.github.com/users/$owner" | field '"type"' | tr -d ',')"
    local scope=users
    [ "$type" = Organization ] && scope=orgs
    local package="${path#*/}"
    local code
    code="$(curl -sS -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $GH_TOKEN" \
        "https://api.github.com/$scope/$owner/packages/container/${package//\//%2F}")"
    [ "$code" = 404 ]
}

case "$mode" in
versions)
    dir="${2:?chart directory}"
    want="${3:?release version X.Y.Z}"
    chart="$(field version <"$dir/Chart.yaml")"
    app="$(field appVersion <"$dir/Chart.yaml")"
    agent="$(awk -F'"' '/^version = /{print $2; exit}' observer/Cargo.toml)"
    for pair in "Chart.yaml version:$chart" "Chart.yaml appVersion:$app" "observer/Cargo.toml version:$agent"; do
        if [ "${pair##*:}" != "$want" ]; then
            echo "::error::release $want needs ${pair%:*} $want, not ${pair##*:}"
            exit 1
        fi
    done
    echo "Versions match $want."
    ;;
check)
    registry="${3:?oci registry, for example oci://ghcr.io/owner/charts}"
    meta="$(helm show chart "$(source_of "${2:?chart or package directory}")")"
    name="$(field name <<<"$meta")"
    version="$(field version <<<"$meta")"
    if out="$(helm show chart "$registry/$name" --version "$version" 2>&1)"; then
        echo "::error::chart version $version already exists in $registry, bump version in Chart.yaml"
        exit 1
    fi
    if grep -qiE 'no such host|connection refused|i/o timeout|tls handshake' <<<"$out"; then
        echo "::error::cannot reach $registry: $out"
        exit 1
    elif grep -qiE 'not found|name unknown|manifest unknown' <<<"$out"; then
        :
    elif grep -qiE 'denied|unauthorized|forbidden' <<<"$out" && [[ "$registry" == oci://ghcr.io/* ]] \
        && ghcr_package_missing "$registry" "$name"; then
        :
    else
        echo "::error::cannot check chart version $version: $out"
        exit 1
    fi
    echo "Chart version $version is new."
    ;;
push)
    registry="${3:?oci registry}"
    package="$(source_of "${2:?package directory}")"
    out="$(helm push "$package" "$registry" 2>&1)"
    echo "$out"
    digest="$(awk '/^Digest:/{print $2}' <<<"$out")"
    [[ "$digest" =~ ^sha256:[0-9a-f]{64}$ ]] || { echo "::error::no chart digest in the push output"; exit 1; }
    echo "digest=$digest" >>"${GITHUB_OUTPUT:-/dev/null}"
    ;;
*)
    echo "unknown mode: $mode" >&2
    exit 2
    ;;
esac
