{{/* Expand the name of the chart. */}}
{{- define "scarab.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
  Which Scarab release owns the plugin namespace: `<release namespace>.<release
  name>`, a label value. The runtime refuses a plugin namespace stamped for
  another release, so two releases never sweep each other's plugins.
*/}}
{{- define "scarab.pluginOwner" -}}
{{- $owner := .Values.plugins.owner | default (printf "%s.%s" .Release.Namespace .Release.Name) -}}
{{- if gt (len $owner) 63 -}}
{{- fail (printf "the plugin owner %q is longer than 63 characters, the most a label value holds; truncating could give two releases the same owner, so set plugins.owner to a shorter name unique to this release" $owner) -}}
{{- end -}}
{{- $owner -}}
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

{{/* Does the workspace service (ADR-0061) actually render?

     `workspace.enabled` is not enough on its own: the service REFUSES to boot
     without SCARAB_WORKSPACE_TOKEN_SECRET (a workspace service with no token
     secret would serve every step's inputs to any caller that reaches the port),
     so rendering a StatefulSet that can only CrashLoopBackOff would be a worse
     default than rendering nothing. An install that has not set a secret gets no
     service; the NOTES say so.

     `existingSecret` counts, because in that mode the key is supplied
     out-of-band and the chart cannot see it. */}}
{{- define "scarab.workspaceEnabled" -}}
{{- if and .Values.workspace.enabled (or .Values.secrets.workspaceTokenSecret .Values.secrets.existingSecret) -}}
true
{{- end -}}
{{- end -}}

{{/* Selector labels for the workspace StatefulSet (ADR-0061).

     `app.kubernetes.io/name` is `<name>-workspace`, NOT `<name>` plus a
     component label, and that is load-bearing. The main Service selects on
     `scarab.selectorLabels` = {name, instance}; a workspace Pod carrying those
     two labels plus a third would still MATCH it, so control-plane traffic to
     svc/<fullname> would round-robin into a data-plane Pod that serves no
     control-plane route — intermittent 404s on half the API, which is exactly
     the "reports success but structurally cannot work" class. Kubernetes label
     selectors are subset matches; the only way out is a distinct `name`.

     Same reason the ServiceMonitor keeps working: it selects the main Service's
     labels, so it does not accidentally scrape both workloads through one
     endpoint set. (Scraping the workspace service is a separate ServiceMonitor;
     not shipped, because serviceMonitor.enabled is off by default.) */}}
{{- define "scarab.workspaceSelectorLabels" -}}
app.kubernetes.io/name: {{ printf "%s-workspace" (include "scarab.name" .) }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "scarab.workspaceLabels" -}}
helm.sh/chart: {{ include "scarab.chart" . }}
{{ include "scarab.workspaceSelectorLabels" . }}
app.kubernetes.io/component: workspace
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{/* ServiceAccount name for the workspace StatefulSet (ADR-0061). Separate from
     the server's, and deliberately UNBOUND — see serviceaccount-workspace.yaml. */}}
{{- define "scarab.workspaceServiceAccountName" -}}
{{- printf "%s-workspace" (include "scarab.fullname" .) -}}
{{- end -}}

{{/* In-cluster base URL of the workspace service — the value of
     SCARAB_WORKSPACE_URL. One definition so the ConfigMap and the Service name
     cannot disagree. Overridable via scarab.workspaceUrl for a split install. */}}
{{- define "scarab.workspaceUrl" -}}
{{- if .Values.scarab.workspaceUrl -}}
{{- .Values.scarab.workspaceUrl -}}
{{- else -}}
{{- printf "http://%s-workspace.%s.svc" (include "scarab.fullname" .) .Release.Namespace -}}
{{- end -}}
{{- end -}}

{{/* One Depot replica's URL with `{ordinal}` for its StatefulSet ordinal —
     the value of SCARAB_WORKSPACE_HOME_URL_TEMPLATE (git-bug 12e2f6b). Each
     ordinal has its own Service (service-workspace-replicas.yaml), named like
     its Pod, so the default here and those Services cannot disagree. */}}
{{- define "scarab.workspaceHomeUrlTemplate" -}}
{{- if .Values.workspace.homeUrlTemplate -}}
{{- .Values.workspace.homeUrlTemplate -}}
{{- else -}}
{{- printf "http://%s-workspace-{ordinal}.%s.svc" (include "scarab.fullname" .) .Release.Namespace -}}
{{- end -}}
{{- end -}}

{{/* Whether the chart names and renders the step namespace
     `<release namespace>-steps`: scarab.namespace is empty, the executor is
     k8s, the chart renders the executor's Role (rbac.create) and may create a
     Namespace (scarab.createNamespace). Otherwise the default is the release
     namespace: with rbac.create false the operator's own RoleBinding is there,
     and with createNamespace false a namespace-only installer keeps working. */}}
{{- define "scarab.createExecNamespace" -}}
{{- if and (not .Values.scarab.namespace) (eq .Values.scarab.executor "k8s") .Values.rbac.create .Values.scarab.createNamespace -}}true{{- end -}}
{{- end -}}

{{/* Namespace the executor launches step Pods into: scarab.namespace, else
     `<release namespace>-steps` when the chart creates it (above), else the
     release namespace. A derived name longer than a namespace may be fails the
     render rather than being cut back to the release namespace. */}}
{{- define "scarab.execNamespace" -}}
{{- if .Values.scarab.namespace -}}
{{- .Values.scarab.namespace -}}
{{- else if include "scarab.createExecNamespace" . -}}
{{- $ns := printf "%s-steps" .Release.Namespace -}}
{{- if gt (len $ns) 63 -}}
{{- fail (printf "scarab: the step namespace `%s` is longer than 63 characters; set scarab.namespace." $ns) -}}
{{- end -}}
{{- $ns -}}
{{- else -}}
{{- .Release.Namespace -}}
{{- end -}}
{{- end -}}

{{/* plugins.namespace must be a namespace of its own (ADR-0072 §9). The
     runtime applies a default-deny NetworkPolicy selecting EVERY Pod there and
     the chart grants Scarab Pod/Deployment create rights in it, so pointing it
     at the release's namespace would cut the server off from Postgres/S3, at
     the step namespace would put PR code beside plugins, and at `default` or a
     `kube-*` namespace would hand Scarab's ServiceAccount create rights where
     it has no business. Refused at render time, before any RBAC is granted. */}}
{{- define "scarab.validatePlugins" -}}
{{- if .Values.plugins.enabled -}}
{{- $ns := .Values.plugins.namespace -}}
{{- $reserved := dict
      .Release.Namespace "the release namespace (scarab-server's own)"
      (include "scarab.execNamespace" .) "scarab.namespace (where step Pods run)"
      "default" "the `default` namespace" -}}
{{- if hasKey $reserved $ns -}}
{{- fail (printf "scarab: plugins.namespace `%s` is %s. Plugins need a namespace of their own: the runtime's default-deny NetworkPolicy selects every Pod in it (ADR-0072 §9)." $ns (get $reserved $ns)) -}}
{{- end -}}
{{- if hasPrefix "kube-" $ns -}}
{{- fail (printf "scarab: plugins.namespace `%s` is a Kubernetes system namespace; plugins need a namespace of their own (ADR-0072 §9)." $ns) -}}
{{- end -}}
{{- end -}}
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

{{/* Directory the operator placement config (ADR-0055) is projected into. */}}
{{- define "scarab.placementDir" -}}
/etc/scarab/placement
{{- end -}}

{{/* Absolute path of the rendered placement config — the value of
     SCARAB_PLACEMENT_CONFIG_FILE and the file the ConfigMap is mounted as. One
     definition so the ConfigMap, the env var and the mount cannot disagree. */}}
{{- define "scarab.placementPath" -}}
{{- printf "%s/placement.yaml" (include "scarab.placementDir" .) -}}
{{- end -}}

{{/* Name of the ConfigMap holding the placement config. Separate from the main
     ConfigMap for the same reason as the connections one: that one is consumed
     with `envFrom`. */}}
{{- define "scarab.placementConfigMapName" -}}
{{- printf "%s-placement" (include "scarab.fullname" .) -}}
{{- end -}}

{{/* Is OAuth login (ADR-0049) configured at all? Emits a non-empty string when
     ANY of the login knobs is set, so a half-configured provider is caught by
     scarab.validateAuth instead of silently rendering nothing. */}}
{{/* The env var a named provider's client secret is injected as (ADR-0069). */}}
{{- define "scarab.providerSecretEnv" -}}
{{- printf "SCARAB_OAUTH_SECRET_%s" (. | upper | replace "-" "_") -}}
{{- end -}}

{{- define "scarab.oauthConfigured" -}}
{{- $o := .Values.scarab.oauth -}}
{{- if or $o.clientId $o.authorizeUrl $o.tokenUrl $o.userinfoUrl $o.issuer .Values.secrets.oauthClientSecret.name -}}
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
{{- end -}}
{{- range .Values.scarab.oauth.providers -}}
{{- if not (and .id .clientId .authorizeUrl .tokenUrl .userinfoUrl) -}}
{{- fail (printf "scarab: scarab.oauth.providers entry `%s` needs id, clientId, authorizeUrl, tokenUrl and userinfoUrl (ADR-0069)." (default "<no id>" .id)) -}}
{{- end -}}
{{- if not (and .clientSecret .clientSecret.name) -}}
{{- fail (printf "scarab: scarab.oauth.providers entry `%s` needs clientSecret.name — a Secret you manage; the client secret is never a plaintext chart value." .id) -}}
{{- end -}}
{{- end -}}
{{- if or (include "scarab.oauthConfigured" .) .Values.scarab.oauth.providers -}}
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

{{/* Whether the step Pod admission policy (admission-policy.yaml, git-bug
     07f0de3) renders: scarab.admissionPolicy.enabled, the k8s executor, and
     either the cluster serves admissionregistration.k8s.io/v1
     ValidatingAdmissionPolicy (Kubernetes 1.30+) or scarab.admissionPolicy.force
     is set. `helm template` without a cluster sees the API only with
     `--api-versions admissionregistration.k8s.io/v1/ValidatingAdmissionPolicy`
     or `force`. */}}
{{- define "scarab.admissionPolicyRenders" -}}
{{- $a := .Values.scarab.admissionPolicy | default dict -}}
{{- if and $a.enabled (eq .Values.scarab.executor "k8s") (or $a.force (.Capabilities.APIVersions.Has "admissionregistration.k8s.io/v1/ValidatingAdmissionPolicy")) -}}true{{- end -}}
{{- end -}}

{{/* The ServiceAccounts and Secrets the operator placement config
     (scarab.placement: the baseline and each profile's `k8s`) puts on step
     Pods, as JSON {"serviceAccounts": [...], "secrets": [...]}. The admission
     policy allows them on any step Pod: it cannot tell which profile a Pod
     was placed with. `default` is the ServiceAccount of a step Pod that names
     none. */}}
{{- define "scarab.admissionPolicyAllowed" -}}
{{- $sas := list "default" -}}
{{- $secrets := list -}}
{{- $placement := .Values.scarab.placement | default dict -}}
{{- $fragments := list -}}
{{- with $placement.baseline }}{{- $fragments = append $fragments . -}}{{- end -}}
{{- range $placement.profiles }}{{- with .k8s }}{{- $fragments = append $fragments . -}}{{- end -}}{{- end -}}
{{- range $fragment := $fragments -}}
{{- $spec := $fragment.spec | default dict -}}
{{- with $spec.serviceAccountName }}{{- $sas = append $sas . -}}{{- end -}}
{{- with $spec.serviceAccount }}{{- $sas = append $sas . -}}{{- end -}}
{{- range $v := $spec.volumes -}}
{{- if $v.secret }}{{- with $v.secret.secretName }}{{- $secrets = append $secrets . -}}{{- end -}}{{- end -}}
{{- if $v.projected -}}
{{- range $s := $v.projected.sources -}}
{{- if $s.secret }}{{- with $s.secret.name }}{{- $secrets = append $secrets . -}}{{- end -}}{{- end -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- range $c := concat ($spec.containers | default list) ($spec.initContainers | default list) -}}
{{- range $e := $c.env -}}
{{- if and $e.valueFrom $e.valueFrom.secretKeyRef }}{{- with $e.valueFrom.secretKeyRef.name }}{{- $secrets = append $secrets . -}}{{- end -}}{{- end -}}
{{- end -}}
{{- range $f := $c.envFrom -}}
{{- if $f.secretRef }}{{- with $f.secretRef.name }}{{- $secrets = append $secrets . -}}{{- end -}}{{- end -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- dict "serviceAccounts" ($sas | uniq) "secrets" ($secrets | uniq) | toJson -}}
{{- end -}}

{{/* Whether the placement config asks for what the admission policy refuses
     (a hostPath volume, hostNetwork, hostPID, hostIPC): NOTES.txt warns. */}}
{{- define "scarab.placementUsesHost" -}}
{{- $placement := .Values.scarab.placement | default dict -}}
{{- $fragments := list -}}
{{- with $placement.baseline }}{{- $fragments = append $fragments . -}}{{- end -}}
{{- range $placement.profiles }}{{- with .k8s }}{{- $fragments = append $fragments . -}}{{- end -}}{{- end -}}
{{- range $fragment := $fragments -}}
{{- $spec := $fragment.spec | default dict -}}
{{- if or $spec.hostNetwork $spec.hostPID $spec.hostIPC }}true{{- end -}}
{{- range $v := $spec.volumes }}{{- if $v.hostPath }}true{{- end -}}{{- end -}}
{{- end -}}
{{- end -}}
