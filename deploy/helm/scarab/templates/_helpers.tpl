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

{{/* Is OAuth login (ADR-0049) configured at all? Emits a non-empty string when
     ANY of the login knobs is set, so a half-configured provider is caught by
     scarab.validateAuth instead of silently rendering nothing. */}}
{{- define "scarab.oauthConfigured" -}}
{{- $o := .Values.scarab.oauth -}}
{{- if or $o.clientId $o.authorizeUrl $o.tokenUrl $o.userinfoUrl .Values.secrets.oauthClientSecret.name -}}
true
{{- end -}}
{{- end -}}

{{/* Authenticator sanity, evaluated at render time so a contradictory or partial
     login config fails `helm upgrade` with an operator-facing message rather
     than producing env the server refuses to boot under (ADR-0048). Included by
     the ConfigMap, which always renders. Emits nothing when the config is sane.

     Deliberately NOT checked here: "no authenticator at all". That case is the
     chart's default, the server owns the refusal (ConfigError::NoAuthenticator),
     and NOTES.txt warns about it — failing the render would break `helm lint`. */}}
{{- define "scarab.validateAuth" -}}
{{- $o := .Values.scarab.oauth -}}
{{- if include "scarab.oauthConfigured" . -}}
{{- $missing := list -}}
{{- if not $o.clientId -}}{{- $missing = append $missing "scarab.oauth.clientId" -}}{{- end -}}
{{- if not $o.authorizeUrl -}}{{- $missing = append $missing "scarab.oauth.authorizeUrl" -}}{{- end -}}
{{- if not $o.tokenUrl -}}{{- $missing = append $missing "scarab.oauth.tokenUrl" -}}{{- end -}}
{{- if not $o.userinfoUrl -}}{{- $missing = append $missing "scarab.oauth.userinfoUrl" -}}{{- end -}}
{{- if $missing -}}
{{- fail (printf "scarab: OAuth login is only partially configured — missing %s. scarab-server takes all five knobs or none and refuses to boot on a partial provider (ADR-0049); set the rest, or clear scarab.oauth entirely." (join ", " $missing)) -}}
{{- end -}}
{{- if not (or .Values.secrets.oauthClientSecret.name .Values.secrets.existingSecret) -}}
{{- fail "scarab: OAuth login is configured but no client secret can reach the Pod. Set secrets.oauthClientSecret.name to a Secret you manage out-of-band (key defaults to oauth-client-secret), or put a SCARAB_OAUTH_CLIENT_SECRET key in secrets.existingSecret. The client secret is never accepted as a plaintext chart value." -}}
{{- end -}}
{{- if .Values.scarab.devInsecure -}}
{{- fail "scarab: scarab.devInsecure=true AND scarab.oauth are both set — that is a contradiction, not a fallback: dev-insecure makes EVERY caller a synthetic Owner and would silently neuter the login you just configured. Pick one: clear scarab.devInsecure for real authn (ADR-0049), or clear scarab.oauth for a dev/eval install." -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/* Name of the Secret holding sensitive env (existing or chart-managed). */}}
{{- define "scarab.secretName" -}}
{{- if .Values.secrets.existingSecret -}}
{{- .Values.secrets.existingSecret -}}
{{- else -}}
{{- include "scarab.fullname" . -}}
{{- end -}}
{{- end -}}
