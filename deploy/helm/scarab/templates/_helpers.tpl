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

{{/* Name of the Secret holding sensitive env (existing or chart-managed). */}}
{{- define "scarab.secretName" -}}
{{- if .Values.secrets.existingSecret -}}
{{- .Values.secrets.existingSecret -}}
{{- else -}}
{{- include "scarab.fullname" . -}}
{{- end -}}
{{- end -}}
