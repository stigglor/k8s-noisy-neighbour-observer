#!/usr/bin/env bash
# End to end test of the chart in a two node kind cluster, where a tainted worker stands in for
# a dedicated node pool. Needs BTF and cgroup v2 in the docker engine (`make preflight`).
# Usage: kind-local.sh up|test|down, also as `make kind-up`, `make kind-test`, `make kind-down`.
set -euo pipefail
cd "$(dirname "$0")/.."

CLUSTER="nno-local"
NS="nno-system"
RELEASE="nno"
CHART="helm/noisy-neighbour-observer"
REPOSITORY="noisy-neighbour-observer"
TAG="dev"
WORKLOADS_NS="nno-e2e"
# A second release with no RBAC, to prove that the agent stops without a pod list.
NORBAC_NS="nno-norbac"
# The agent waits 60s for the first pod list. The rest covers the image start and log delay.
NORBAC_TIMEOUT=120
# Each poll waits at most this long for the agents to read the maps again.
POLL_TIMEOUT=90
# The victim labels of the spin workload.
SPIN="namespace=\"$WORKLOADS_NS\",workload_kind=\"Deployment\",workload=\"spin\""

require() {
    command -v "$1" >/dev/null 2>&1 || { echo "error: '$1' is required but not installed."; exit 1; }
}

preflight() {
    require docker
    require kubectl
    require kind
    require helm
    docker info >/dev/null 2>&1 || { echo "error: the Docker daemon does not respond."; exit 1; }
}

kctl() { kubectl --context "kind-${CLUSTER}" "$@"; }

install() {
    helm --kube-context "kind-${CLUSTER}" upgrade --install "$@" "$CHART" \
        --set image.repository="$REPOSITORY" \
        --set image.tag="$TAG" \
        --set image.pullPolicy=Never
}

up() {
    preflight

    if kind get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
        echo "==> kind cluster '$CLUSTER' already exists"
    else
        echo "==> create kind cluster '$CLUSTER'"
        kind create cluster --config deploy/local/kind-cluster.yaml
    fi

    local arch
    case "$(docker info --format '{{.Architecture}}')" in
        aarch64 | arm64) arch=arm64 ;;
        x86_64 | amd64) arch=amd64 ;;
        *) echo "error: unsupported docker engine architecture"; exit 1 ;;
    esac
    echo "==> build the image for linux/$arch"
    make docker-build PLATFORM="linux/$arch" IMAGE="$REPOSITORY:$TAG"

    echo "==> load the image into kind"
    kind load docker-image "$REPOSITORY:$TAG" --name "$CLUSTER"

    echo "==> deploy the CPU bound workloads"
    kctl apply -f deploy/local/workloads.yaml

    local existed=false
    if helm --kube-context "kind-${CLUSTER}" status "$RELEASE" --namespace "$NS" >/dev/null 2>&1; then
        existed=true
    fi
    echo "==> install the chart"
    install "$RELEASE" --namespace "$NS" --create-namespace --wait --timeout 3m

    # The image tag is static, so a re-run renders the same pod spec. A restart makes the
    # pods of an earlier install use the image that kind just loaded.
    if [ "$existed" = true ]; then
        echo "==> restart the agents, so they use the loaded image"
        kctl -n "$NS" rollout restart "daemonset/$RELEASE-noisy-neighbour-observer"
        kctl -n "$NS" rollout status "daemonset/$RELEASE-noisy-neighbour-observer" --timeout=3m
    fi
    kctl -n "$WORKLOADS_NS" rollout status deploy/stress deploy/spin --timeout=3m
    echo "==> up. Run 'make kind-test' to test."
}

fail() {
    echo "==> FAIL: $*"
    kctl get pods -A -o wide || true
    kctl -n "$NS" logs "daemonset/$RELEASE-noisy-neighbour-observer" --tail=20 || true
    exit 1
}

# Prints "<node> <pod>" for each agent pod that is not on its way out.
agents() {
    kctl -n "$NS" get pods -l "app.kubernetes.io/instance=$RELEASE" -o go-template \
        --template '{{range .items}}{{if not .metadata.deletionTimestamp}}{{.spec.nodeName}} {{.metadata.name}}{{"\n"}}{{end}}{{end}}' \
        | sort
}

# The apiserver pod proxy reaches /metrics with no port-forward, and works on a distroless image.
scrape() { kctl get --raw "/api/v1/namespaces/$NS/pods/$1:9400/proxy/metrics"; }

# The label sets of the stable entities on each node: the e2e workloads, the agent and kube-system.
entities() {
    local node pod
    while read -r node pod; do
        scrape "$pod" \
            | sed -n 's/^noisy_neighbour_runqueue_latency_seconds_count\({[^}]*}\).*/\1/p' \
            | grep -E "namespace=\"($WORKLOADS_NS|$NS|kube-system)\"" \
            | sed "s/^/$node /"
    done < <(agents)
}

# Prints "<namespace>/<workload>" of the named culprit with the most wait of spin in a scrape.
top_culprit() {
    sed -n "s/^noisy_neighbour_runqueue_wait_by_culprit_seconds_total{$SPIN,culprit_namespace=\"\([^\"]*\)\",culprit_workload_kind=\"[^\"]*\",culprit_workload=\"\([^\"]*\)\"} \(.*\)/\3 \1\/\2/p" \
        | grep -v ' _other/_other$' | sort -gr | head -1 | cut -d' ' -f2
}

# Prints the wait of spin with cause $1 in a scrape, in seconds.
cause_wait() {
    sed -n "s/^noisy_neighbour_runqueue_wait_by_cause_seconds_total{$SPIN,cause=\"$1\"} //p"
}

series() {
    local node pod
    while read -r node pod; do
        echo "$node $(scrape "$pod" | sed -n 's/^noisy_neighbour_series //p')"
    done < <(agents)
}

# Runs "$@" until its output equals $1, or fails after POLL_TIMEOUT. A probe can fail while new
# agents start (no series yet, or a pod proxy to a terminating pod), so a failure only retries.
wait_equal() {
    local want="$1" message="$2" got deadline=$((SECONDS + POLL_TIMEOUT))
    shift 2
    while true; do
        got="$("$@" 2>/dev/null)" || true
        [ "$got" = "$want" ] && return 0
        if [ "$SECONDS" -ge "$deadline" ]; then
            diff <(echo "$want") <(echo "$got") || true
            fail "$message"
        fi
        sleep 5
    done
}

test_flow() {
    preflight
    local ds="daemonset/$RELEASE-noisy-neighbour-observer"

    echo "==> one ready agent on each node, the tainted worker included"
    kctl -n "$NS" rollout status "$ds" --timeout=3m
    local nodes desired ready
    nodes="$(kctl get nodes --no-headers | wc -l | tr -d ' ')"
    desired="$(kctl -n "$NS" get "$ds" -o jsonpath='{.status.desiredNumberScheduled}')"
    ready="$(kctl -n "$NS" get "$ds" -o jsonpath='{.status.numberReady}')"
    echo "    nodes=$nodes desiredNumberScheduled=$desired numberReady=$ready"
    [ "$desired" = "$nodes" ] || fail "desiredNumberScheduled $desired, want $nodes"
    [ "$ready" = "$nodes" ] || fail "numberReady $ready, want $nodes"

    echo "==> each agent labels the stress workload and the kube-system workloads"
    local node pod metrics deadline
    while read -r node pod; do
        deadline=$((SECONDS + POLL_TIMEOUT))
        until metrics="$(scrape "$pod")" \
            && grep -q "namespace=\"$WORKLOADS_NS\",workload_kind=\"Deployment\",workload=\"stress\"" <<<"$metrics" \
            && grep -q 'namespace="kube-system",workload_kind="DaemonSet",workload="kube-proxy"' <<<"$metrics"; do
            [ "$SECONDS" -lt "$deadline" ] || fail "no stress or kube-proxy series on $node"
            sleep 5
        done
        echo "    $node:"
        grep '^noisy_neighbour_runqueue_latency_seconds_count{' <<<"$metrics" | sed 's/^/      /'
    done < <(agents)

    # spin has a CPU limit, so its throttle wait ends on an idle CPU.
    echo "==> stress is the top culprit of spin on each node, and spin shows idle wait"
    local top other idle
    while read -r node pod; do
        deadline=$((SECONDS + POLL_TIMEOUT))
        until metrics="$(scrape "$pod")" \
            && top="$(top_culprit <<<"$metrics")" \
            && other="$(cause_wait other_workload <<<"$metrics")" \
            && idle="$(cause_wait idle <<<"$metrics")" \
            && [ "$top" = "$WORKLOADS_NS/stress" ] \
            && awk -v o="$other" -v i="$idle" 'BEGIN { exit !(o > 0 && i > 0) }'; do
            [ "$SECONDS" -lt "$deadline" ] \
                || fail "spin on $node: top culprit '$top', other_workload '$other', idle '$idle'"
            sleep 5
        done
        echo "    $node: top culprit $top, other_workload wait ${other}s, idle wait ${idle}s"
        grep -E "^noisy_neighbour_runqueue_wait_by_(cause|culprit)_seconds_total\{$SPIN," <<<"$metrics" | sed 's/^/      /'
    done < <(agents)

    echo "==> the series count stays the same after a rollout of the stress workload"
    local before
    before="$(series)"
    echo "$before" | sed 's/^/    before: /'
    kctl -n "$WORKLOADS_NS" rollout restart deploy/stress
    kctl -n "$WORKLOADS_NS" rollout status deploy/stress --timeout=2m
    wait_equal "$before" "the series count changed after the rollout" series
    series | sed 's/^/    after:  /'

    echo "==> the label sets stay the same after a chart upgrade"
    before="$(entities)"
    install "$RELEASE" --namespace "$NS" --reuse-values --set-string "podAnnotations.e2e=$(date +%s)" >/dev/null
    kctl -n "$NS" rollout status "$ds" --timeout=3m
    wait_equal "$before" "the label sets changed after the upgrade" entities
    echo "    $(wc -l <<<"$before" | tr -d ' ') label sets unchanged"

    echo "==> appArmor.unconfined is accepted on kind"
    install "$RELEASE" --namespace "$NS" --reuse-values --set appArmor.unconfined=true >/dev/null
    kctl -n "$NS" rollout status "$ds" --timeout=3m
    local profile
    profile="$(kctl -n "$NS" get "$ds" \
        -o jsonpath='{.spec.template.spec.containers[0].securityContext.appArmorProfile.type}')"
    [ "$profile" = "Unconfined" ] || fail "appArmorProfile is '$profile', want Unconfined"
    echo "    appArmorProfile=$profile, all agents ready"

    # Last, because its pods add an entity to the other agents.
    echo "==> without RBAC the agent stops after the first pod list times out"
    kctl create namespace "$NORBAC_NS" --dry-run=client -o yaml | kctl apply -f -
    install norbac --namespace "$NORBAC_NS" --set rbac.create=false >/dev/null
    local norbac_logs=""
    deadline=$((SECONDS + NORBAC_TIMEOUT))
    until grep -q "no pod list after 60s" <<<"$norbac_logs"; do
        [ "$SECONDS" -lt "$deadline" ] || fail "the agent without RBAC did not stop: $norbac_logs"
        sleep 5
        # The current or the last container, whichever logged the startup check.
        norbac_logs="$(kctl -n "$NORBAC_NS" logs "daemonset/norbac-noisy-neighbour-observer" 2>&1 || true)
$(kctl -n "$NORBAC_NS" logs "daemonset/norbac-noisy-neighbour-observer" --previous 2>&1 || true)"
    done
    grep -m1 "no pod list after 60s" <<<"$norbac_logs" | cut -c1-240 | sed 's/^/    /'
    helm --kube-context "kind-${CLUSTER}" uninstall norbac --namespace "$NORBAC_NS" --wait
    kctl delete namespace "$NORBAC_NS" --wait=false

    echo "==> PASS"
}

down() {
    kind delete cluster --name "$CLUSTER"
}

case "${1:-}" in
    up) up ;;
    test) test_flow ;;
    down) down ;;
    *)
        echo "usage: $0 {up|test|down}"
        exit 1
        ;;
esac
