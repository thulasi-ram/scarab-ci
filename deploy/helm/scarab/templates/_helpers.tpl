{{/* Expand the name of the chart. */}}
{{- define "scarab.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/* Fully qualified app name. */}}
{{- define "scarab.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "scarab.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "scarab.labels" -}}
helm.sh/chart: {{ include "scarab.chart" . }}
{{ include "scarab.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{- define "scarab.selectorLabels" -}}
app.kubernetes.io/name: {{ include "scarab.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/* ServiceAccount name. */}}
{{- define "scarab.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "scarab.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{/* Namespace the executor launches step Pods into (defaults to release ns). */}}
{{- define "scarab.execNamespace" -}}
{{- default .Release.Namespace .Values.scarab.namespace -}}
{{- end -}}

{{/* Directory the file-mounted GitHub App PEM is projected into (enh 245a99c). */}}
{{- define "scarab.githubAppPemDir" -}}
/etc/scarab/forge
{{- end -}}

{{/* Absolute path of the file-mounted GitHub App PEM — the value of
     SCARAB_GITHUB_APP_PEM_FILE, and the mount the Pod projects it at. One
     definition so the ConfigMap and the Deployment can never disagree. */}}
{{- define "scarab.githubAppPemPath" -}}
{{- printf "%s/%s" (include "scarab.githubAppPemDir" .) .Values.secrets.githubAppPemSecret.key -}}
{{- end -}}

{{/* Directory the declarative `connections:` block is projected into
     (ADR-0060 part D). */}}
{{- define "scarab.connectionsDir" -}}
/etc/scarab/connections
{{- end -}}

{{/* Absolute path of the rendered connections block — the value of
     SCARAB_CONNECTIONS_FILE and the file the ConfigMap is mounted as. One
     definition so the ConfigMap, the env var and the mount cannot disagree. */}}
{{- define "scarab.connectionsPath" -}}
{{- printf "%s/connections.yaml" (include "scarab.connectionsDir" .) -}}
{{- end -}}

{{/* Name of the ConfigMap holding the declarative connections block. Separate
     from the main ConfigMap because that one is consumed with `envFrom`, where a
     key named `connections.yaml` is not a legal env var name. */}}
{{- define "scarab.connectionsConfigMapName" -}}
{{- printf "%s-connections" (include "scarab.fullname" .) -}}
{{- end -}}

{{/* Name of the Secret holding sensitive env (existing or chart-managed). */}}
{{- define "scarab.secretName" -}}
{{- if .Values.secrets.existingSecret -}}
{{- .Values.secrets.existingSecret -}}
{{- else -}}
{{- include "scarab.fullname" . -}}
{{- end -}}
{{- end -}}
