{{/*
Expand the name of the chart.
*/}}
{{- define "noisy-neighbour-observer.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name, truncated at the 63 character name limit.
*/}}
{{- define "noisy-neighbour-observer.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{/*
The name of cluster scoped objects. It holds the namespace, so releases with the same name in
two namespaces do not take over each other's RBAC.
*/}}
{{- define "noisy-neighbour-observer.clusterName" -}}
{{- $room := max 1 (sub 62 (len .Release.Namespace)) | int -}}
{{- printf "%s-%s" (include "noisy-neighbour-observer.fullname" . | trunc $room | trimSuffix "-") .Release.Namespace | trunc 63 }}
{{- end }}

{{/*
Create chart name and version as used by the chart label.
*/}}
{{- define "noisy-neighbour-observer.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Common labels.
*/}}
{{- define "noisy-neighbour-observer.labels" -}}
helm.sh/chart: {{ include "noisy-neighbour-observer.chart" . }}
{{ include "noisy-neighbour-observer.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/component: agent
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/*
Selector labels (immutable, so no version).
*/}}
{{- define "noisy-neighbour-observer.selectorLabels" -}}
app.kubernetes.io/name: {{ include "noisy-neighbour-observer.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Name of the service account to use.
*/}}
{{- define "noisy-neighbour-observer.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "noisy-neighbour-observer.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/*
Image reference. The schema requires the repository, since no image is published yet.
*/}}
{{- define "noisy-neighbour-observer.image" -}}
{{- printf "%s:%s" .Values.image.repository (default .Chart.AppVersion .Values.image.tag) -}}
{{- end }}

{{/*
Reject values that the agent refuses at startup, or that the kubelet applies in a way the
user does not expect. The agent checks its own config at startup, so the chart does not copy it.
*/}}
{{- define "noisy-neighbour-observer.validate" -}}
{{- if and .Values.privileged .Values.seLinux.type -}}
{{- fail "privileged overrides seLinuxOptions (bottlerocket#3791), so set privileged or seLinux.type, not both" -}}
{{- end -}}
{{- if (.Values.affinity | default dict).nodeAffinity -}}
{{- fail "affinity.nodeAffinity would replace the default node affinity; use extraNodeAffinityExpressions" -}}
{{- end -}}
{{- if and .Values.rbac.create (not .Values.serviceAccount.create) (not .Values.serviceAccount.name) -}}
{{- fail "rbac.create with serviceAccount.create=false needs serviceAccount.name, or the binding grants pod reads to the default ServiceAccount" -}}
{{- end -}}
{{- range $key, $_ := .Values.podLabels -}}
{{- if has $key (list "app.kubernetes.io/name" "app.kubernetes.io/instance") -}}
{{- fail (printf "podLabels must not set %s, because the DaemonSet selector uses it" $key) -}}
{{- end -}}
{{- end -}}
{{- end }}
