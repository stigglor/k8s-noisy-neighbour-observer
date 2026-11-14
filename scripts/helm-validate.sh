#!/usr/bin/env bash
# Lint the chart and validate its renders in each scenario, locally and in CI (helm.yml).
# Without kubeconform on PATH, lint and render still run; CI installs a pinned kubeconform.
# Env overrides: CHART (default helm/noisy-neighbour-observer), KUBERNETES_VERSION (default 1.34.0).
set -euo pipefail
cd "$(dirname "$0")/.."

CHART="${CHART:-helm/noisy-neighbour-observer}"
KUBERNETES_VERSION="${KUBERNETES_VERSION:-1.34.0}"
# The schema requires an image. A scenario can override it with a later --set.
REQUIRED=(--set image.repository=ci.example.com/noisy-neighbour-observer)

command -v helm >/dev/null || { echo "helm is required (https://helm.sh/docs/intro/install/)"; exit 1; }

# -ignore-missing-schemas covers the PodMonitor CRD, which is not in the default schema set.
if command -v kubeconform >/dev/null; then
    kc() { kubeconform -strict -summary -ignore-missing-schemas -kubernetes-version "$KUBERNETES_VERSION"; }
else
    echo "WARNING: kubeconform not found, so the manifest schema validation does not run."
    echo "         install it to validate locally: https://github.com/yannh/kubeconform"
    kc() { cat >/dev/null; }
fi

render() { helm template ci "$CHART" --namespace nno "${REQUIRED[@]}" "$@"; }

# A render is captured in a variable before grep, because grep -q closes the pipe early and
# pipefail would then report the SIGPIPE of helm.
expect() {
    local rendered="$1" pattern="$2" message="$3"
    grep -qE -- "$pattern" <<<"$rendered" || { echo "FAIL: $message"; exit 1; }
}
# The render must fail, and its error must name the cause, so an unrelated error cannot pass.
reject() {
    local message="$1" cause="$2" errors
    shift 2
    if errors="$(render "$@" 2>&1 >/dev/null)"; then
        echo "FAIL: chart rendered with $message"
        exit 1
    fi
    grep -qF -- "$cause" <<<"$errors" || { echo "FAIL: $message failed with: $errors"; exit 1; }
}

echo "==> helm lint ($CHART)"
helm lint "$CHART" --strict "${REQUIRED[@]}"

echo "==> scenario: default"
render | kc
default_rendered="$(render)"

echo "==> scenario: the default has no CPU limit"
# Prints the lines indented below each limits: key.
limits="$(awk '/^ *limits:/ { depth = index($0, "l"); next }
    depth && match($0, /[^ ]/) > depth { print; next } { depth = 0 }' <<<"$default_rendered")"
expect "$limits" 'memory:' "no memory limit, so the BPF maps can grow the pod without bound"
if grep -q 'cpu:' <<<"$limits"; then
    echo "FAIL: the default sets a CPU limit, so CFS throttle adds to the wait the agent measures"
    exit 1
fi

echo "==> scenario: the default node affinity excludes nodes the agent cannot run on"
expect "$default_rendered" 'key: kubernetes.io/os' "no kubernetes.io/os expression"
expect "$default_rendered" 'key: eks.amazonaws.com/compute-type' "no fargate expression"
expect "$default_rendered" 'key: type$' "no virtual-kubelet expression"

echo "==> scenario: NNO_NODE_NAME comes from the downward API"
grep -A3 'name: NNO_NODE_NAME' <<<"$default_rendered" | grep -q 'fieldPath: spec.nodeName' || {
    echo "FAIL: NNO_NODE_NAME is not spec.nodeName, so the agent watches the wrong pods"
    exit 1
}

# kubelet probes the pod IP, so a bind to 0.0.0.0 fails the probes on an IPv6 cluster.
echo "==> scenario: the default bind address serves IPv6"
expect "$default_rendered" 'value: "\[::\]:9400"' "NNO_BIND_ADDR is not [::]:9400"

echo "==> scenario: PodMonitor"
render --set podMonitor.enabled=true --set podMonitor.interval=30s | kc
podmonitor_rendered="$(render --set podMonitor.enabled=true --show-only templates/podmonitor.yaml)"
expect "$podmonitor_rendered" 'honorLabels: true' "the PodMonitor does not honor the agent labels"
expect "$podmonitor_rendered" 'targetLabel: instance' "the PodMonitor keeps the pod IP as instance"

echo "==> scenario: every toggle"
render \
    --set seLinux.type=spc_t \
    --set appArmor.unconfined=true \
    --set 'extraCapabilities={SYS_ADMIN}' \
    --set prometheusAnnotations=true \
    --set podAnnotations.example=value \
    --set rbac.create=false \
    --set serviceAccount.create=false \
    --set priorityClassQuota.create=false \
    --set 'extraEnv[0].name=EXAMPLE' --set-string 'extraEnv[0].value=1' | kc
render --set privileged=true | kc

echo "==> scenario: extra node affinity expressions join the default term"
extra_rendered="$(render \
    --set 'extraNodeAffinityExpressions[0].key=pool' \
    --set 'extraNodeAffinityExpressions[0].operator=NotIn' \
    --set 'extraNodeAffinityExpressions[0].values={windows}')"
kc <<<"$extra_rendered"
expect "$extra_rendered" 'key: pool' "the extra expression is missing"
expect "$extra_rendered" 'key: kubernetes.io/os' "the extra expression replaced the defaults"
if [ "$(grep -c -- '- matchExpressions:' <<<"$extra_rendered")" != 1 ]; then
    echo "FAIL: the extra expression is in a second term, which ORs with the defaults"
    exit 1
fi

echo "==> scenario: container granularity and custom bounds"
render \
    --set config.granularity=container \
    --set 'config.histogramBounds={500ns,2us,1ms}' \
    --set config.seriesBudget=22 | kc

echo "==> scenario: kube-system needs no quota"
if helm template ci "$CHART" --namespace kube-system "${REQUIRED[@]}" | grep -q 'kind: ResourceQuota'; then
    echo "FAIL: the chart renders a priority class quota in kube-system"
    exit 1
fi

echo "==> scenario: large numbers from a values file render as integers"
values="$(mktemp)"
trap 'rm -f "$values"' EXIT
printf 'config:\n  maxCgroups: 1048576\n  seriesBudget: 2000000\n  maxPairs: 3000000\n' >"$values"
numbers_rendered="$(render -f "$values")"
expect "$numbers_rendered" 'value: "1048576"' "maxCgroups from a values file is not an integer"
expect "$numbers_rendered" 'value: "2000000"' "seriesBudget from a values file is not an integer"
expect "$numbers_rendered" 'value: "3000000"' "maxPairs from a values file is not an integer"

echo "==> scenario: cluster scoped names hold the namespace"
expect "$(render --show-only templates/rbac.yaml)" 'name: ci-noisy-neighbour-observer-nno$' \
    "the ClusterRole name does not hold the namespace"

echo "==> scenario: long names keep the namespace in cluster scoped names"
long=(--set fullnameOverride=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa --show-only templates/rbac.yaml)
prod="$(helm template ci "$CHART" --namespace observability-prod "${REQUIRED[@]}" "${long[@]}")"
staging="$(helm template ci "$CHART" --namespace observability-staging "${REQUIRED[@]}" "${long[@]}")"
expect "$prod" 'name: a+-observability-prod$' "a long name cut the namespace from the ClusterRole name"
expect "$staging" 'name: a+-observability-staging$' "a long name cut the namespace from the ClusterRole name"

echo "==> scenario: a user label replaces a chart label"
merged="$(render --set 'podLabels.app\.kubernetes\.io/component=custom' --show-only templates/daemonset.yaml)"
if [ "$(grep -c 'app.kubernetes.io/component:' <<<"$merged")" != 2 ]; then
    echo "FAIL: app.kubernetes.io/component renders twice in the pod template"
    exit 1
fi
expect "$merged" 'app.kubernetes.io/component: "custom"' "the user label did not replace the chart label"

echo "==> scenario: invalid values are rejected"
reject "privileged and seLinux.type" "bottlerocket#3791" --set privileged=true --set seLinux.type=spc_t
reject "no image.repository" "/image/repository" --set image.repository=
reject "an unknown granularity" "/config/granularity" --set config.granularity=node
reject "a zero read interval" "/config/readInterval" --set config.readInterval=0s
reject "a misspelled value key" "additional properties 'enable'" --set podMonitor.enable=true
reject "rbac on the default ServiceAccount" "serviceAccount.name" --set serviceAccount.create=false
reject "podLabels over the selector" "selector" --set 'podLabels.app\.kubernetes\.io/name=other'
reject "affinity.nodeAffinity" "extraNodeAffinityExpressions" --set 'affinity.nodeAffinity.requiredDuringSchedulingIgnoredDuringExecution.nodeSelectorTerms[0].matchExpressions[0].key=a'

echo "✓ helm chart validated"
