# scarab Helm chart

Deploys **scarab-server** (one binary, selectable roles) onto Kubernetes,
with the RBAC it needs to launch one Pod per step in the step namespace
(`<release namespace>-steps` by default).

## Prerequisites

- Kubernetes 1.25+; 1.30+ for the step Pod admission policy
- A Postgres database (the durable outbox / coordination bus). Without it the
  server runs API-only.
- An S3-compatible object store for logs/artifacts (AWS S3 or in-cluster MinIO)
  for production; a local dir fallback exists for dev.
- The image published by `.github/workflows/image.yml`
  (`ghcr.io/thulasi-ram/scarab-server`).

## Install

Every `v*` tag publishes this chart to `oci://ghcr.io/thulasi-ram/charts/scarab`
at the same version as the images (`.github/workflows/image.yml`, `chart` job),
so a released chart's default `image.tag` (its `appVersion`) always exists:

```sh
helm upgrade --install scarab oci://ghcr.io/thulasi-ram/charts/scarab \
  --version 0.8.3 --namespace scarab --create-namespace -f values.yaml
```

From a checkout, point at the directory instead (and pin `image.tag`, because the
in-tree `appVersion` may be ahead of the last release):

```sh
helm upgrade --install scarab deploy/helm/scarab \
  --namespace scarab --create-namespace \
  --set image.tag=edge \
  --set secrets.databaseUrl='postgres://scarab:scarab@postgres.scarab.svc:5432/scarab' \
  --set scarab.s3.bucket=scarab-logs \
  --set scarab.s3.endpoint=http://minio.scarab.svc:9000 \
  --set secrets.s3AccessKey=scarab \
  --set secrets.s3SecretKey=scarabsecret
```

For production, keep credentials out of `--set`: create a Secret whose keys are
the `SCARAB_*` env var names and reference it with `secrets.existingSecret`.

```sh
kubectl -n scarab create secret generic scarab-env \
  --from-literal=SCARAB_DATABASE_URL=... \
  --from-literal=SCARAB_S3_ACCESS_KEY=... \
  --from-literal=SCARAB_S3_SECRET_KEY=... \
  --from-literal=SCARAB_MASTER_KEY="$(head -c 32 /dev/urandom | base64)"

helm upgrade --install scarab deploy/helm/scarab -n scarab \
  --set secrets.existingSecret=scarab-env --set scarab.s3.bucket=scarab-logs
```

On EKS you can drop the S3 keys: leave `SCARAB_S3_ACCESS_KEY` /
`SCARAB_S3_SECRET_KEY` unset and Scarab uses AWS's ambient credential chain
(IRSA, Pod Identity, instance role). Annotate both ServiceAccounts with the
role, because the server and the workspace StatefulSet both reach the bucket:
`serviceAccount.annotations` and `workspace.serviceAccount.annotations`, e.g.
`eks.amazonaws.com/role-arn: arn:aws:iam::<acct>:role/scarab`. Step Pods run
under neither ServiceAccount, so they do not inherit the role.

## Master keys and value ledgers

Every replica must use the same master keys. `SCARAB_MASTER_KEYS` is a
comma-separated list of base64 32-byte keys: the first encrypts, the rest only
decrypt (`SCARAB_MASTER_KEY` sets one key). Scarab encrypts stored secrets with
them, and each step's value ledger: the secret values a launch hands the step,
which the server redacts the step's log with. A replica that cannot decrypt a
step's ledger stores none of that step's log and leaves it to a replica that
can.

To rotate a key, add the new key after the active one on every replica and roll
them out, then move it to the front on every replica and roll them out again.
At boot each replica rewraps the stored secrets under the active key. A ledger
generation cannot be rewrapped (its encryption is bound to its step and
generation), so it stays under the key it was sealed with until the ledger is
deleted. Remove the old key only when `scarab_secrets_rows_under_nonactive_key`
reads 0 on every replica. The metric counts the stored secrets and the ledger
generations still under a key that is not the active one, as of the replica's
boot, so restart the replicas to read it again. It reaches 0 at most 24.5 hours
after the key stopped being active, the longest a ledger lives. Removing the key
earlier stops the logs of running steps from being stored, and a re-drive of
such a step fails.

One replica revokes forge tokens and deletes ledgers: the one that holds the
lease `ledger-sweep`, every 30 seconds, in a task of its own beside the
scheduler. It waits at most 20 seconds for one step's revocations; a failed
revocation is tried again after 1 minute, doubling to an hour, and a ledger is
deleted anyway 24.5 hours after its first generation once its step is not
running.

## Login (`scarab.oauth`, ADR-0049)

Scarab authenticates operators against a **forge-agnostic OAuth/OIDC provider**:
the endpoints are explicit values, so GitHub, Forgejo and any OIDC issuer are the
same code path. Without one the server has no authenticator and **refuses to
start** (ADR-0048). The only other way to start it is `scarab.devInsecure`, where
every caller is a synthetic Owner.

The four endpoints and ids are chart values. The **client secret never is**: it
comes by reference from a Secret you manage out-of-band, like the App PEM below:

```sh
kubectl -n scarab create secret generic scarab-oauth \
  --from-literal=oauth-client-secret=<client secret>

helm upgrade --install scarab deploy/helm/scarab -n scarab \
  --set scarab.oauth.clientId=Iv1.0123456789abcdef \
  --set scarab.oauth.authorizeUrl=https://github.com/login/oauth/authorize \
  --set scarab.oauth.tokenUrl=https://github.com/login/oauth/access_token \
  --set scarab.oauth.userinfoUrl=https://api.github.com/user \
  --set scarab.oauth.scopes='read:user' \
  --set 'scarab.oauth.owners={alice,bob}' \
  --set secrets.oauthClientSecret.name=scarab-oauth
```

That renders `SCARAB_OAUTH_CLIENT_ID` / `_AUTHORIZE_URL` / `_TOKEN_URL` /
`_USERINFO_URL` (+ optional `_SCOPES`, `_OWNERS` comma-joined) into the ConfigMap,
and injects `SCARAB_OAUTH_CLIENT_SECRET` into the container with a `secretKeyRef`.
For a Forgejo host the same four point at `/login/oauth/authorize`,
`/login/oauth/access_token` and `/api/v1/user` on your instance.

`scarab.oauth.owners` are the provider subjects granted **Owner** at login
(a bootstrap measure until scoped RBAC, ADR-0049 C2); everyone else authenticates
as **Viewer**. An empty list means nobody can administer the install, and the
chart warns about this on install. An entry matches the subject **or** a verified email claim.

### Plain OAuth2 vs OIDC (`scarab.oauth.issuer`)

Leaving `scarab.oauth.issuer` empty is **plain OAuth2**: the provider returns no
`id_token` and the Principal comes from `userinfoUrl`. This is how GitHub and
Forgejo work, and it is the default.

Setting it turns on **OIDC mode**: the `id_token` is verified against that
issuer's discovery document and JWKS (`iss`, `aud`, `exp`, `nonce`, RS256) and
its claims become authoritative, with userinfo as fallback only when no
`id_token` comes back. Use it for Dex, Keycloak, or Google:

```bash
  --set scarab.oauth.issuer=https://dex.example.com \
  --set 'scarab.oauth.owners={alice@example.com}'
```

With an OIDC issuer, `sub` is typically an opaque per-client id, so prefer the
**verified email** in `owners`. An unverified email never grants Owner. Setting
`issuer` *without* the four endpoints is a render error, same as any other
partial provider.

### Several providers (`scarab.oauth.providers`, ADR-0069)

Each sign-in is pinned to a Scarab Principal by the provider's **immutable** user
id (OIDC `sub`, or GitHub/Forgejo's numeric `id`), never by the login, so a
recycled login cannot inherit someone's roles. The provider above keeps issuing
the same bare ids as before (`alice`), so upgrading changes no existing binding.
Set `scarab.oauth.forgeUrl` to the forge it signs into (`https://api.github.com`
for GitHub, the web host for Forgejo) so a forge-permission import resolves
through it.

Additional providers issue ids in their own namespace (`okta:ada@example.com`,
`forgejo:alice`) and share the one callback URL:

```yaml
scarab:
  oauth:
    owners: [alice, "okta:ada@example.com"]
    providers:
      - id: okta
        name: Okta
        clientId: 0oa1example
        authorizeUrl: https://acme.okta.com/oauth2/v1/authorize
        tokenUrl: https://acme.okta.com/oauth2/v1/token
        userinfoUrl: https://acme.okta.com/oauth2/v1/userinfo
        issuer: https://acme.okta.com
        scopes: openid profile email
        clientSecret: { name: okta-oauth, key: client-secret }
```

With more than one provider an owners email must be qualified as
`<provider>:<email>`. It is honoured only when that provider asserts the address
verified, so the weakest configured IdP cannot create an Owner. A bare email refuses boot.

**A provider needs all five values or none.** The server refuses to boot on a partially configured
provider, so the chart refuses to render one: a missing endpoint, or a login
config with no reachable client secret, **fails `helm upgrade`** with the exact
value to set. If you already use `secrets.existingSecret`, you may supply
`SCARAB_OAUTH_CLIENT_SECRET` as a key of it and leave
`secrets.oauthClientSecret.name` empty.

**`devInsecure` cannot be combined with `scarab.oauth`.** Setting
`scarab.devInsecure=true` *and* `scarab.oauth.*` fails the render; neither
setting overrides the other. Dev-insecure mode would silently make every caller
an Owner again and disable the login you just configured. Pick one.

## The GitHub App credential (bootstrap-free)

In App mode (`scarab.githubAppId`) the credential a GitHub connection
authenticates with is the App's private-key PEM. It can be stored in Postgres at
the reserved `_forge` scope via `POST /v1/secrets` (ADR-0046), but that has a
bootstrap ordering problem for a cluster: the server must already be up to
accept the credential it needs, and the credential is lost with the database. So the
chart can also hand the PEM to the server **at boot**, which survives a database
recreate and needs no API call:

```sh
# GitOps-native: the PEM stays in a Secret you manage out-of-band
# (external-secrets / sealed-secrets / SOPS), mounted as a file.
kubectl -n scarab create secret generic scarab-github-app \
  --from-file=github-app.pem=./my-app.private-key.pem

helm upgrade --install scarab deploy/helm/scarab -n scarab \
  --set scarab.githubAppId=123456 \
  --set secrets.githubAppPemSecret.name=scarab-github-app
```

The chart projects that key read-only at `/etc/scarab/forge/<key>` and sets
`SCARAB_GITHUB_APP_PEM_FILE`; an unreadable path is a boot failure (ADR-0048),
never a silent downgrade. A boot-supplied PEM **overrides** the DB-stored
`_forge` credential for GitHub App-mode connections, so an absent one is expected
and not reported as degraded.

There are two other ways to supply it: `secrets.githubAppPem` (inline value,
rendered into the chart Secret as `SCARAB_GITHUB_APP_PEM`) and a
`SCARAB_GITHUB_APP_PEM` key in your own `secrets.existingSecret`. Inline wins
over the mounted file. Prefer the mounted file: no key material passes through
Helm values.

**This covers the credential only.** Which installations and repos exist is
separate durable state, registered by the App's `installation` /
`installation_repositories` webhook deliveries (or, for the local dogfood stack,
the signed synthetic `installation` event `just stack up dogfood` sends). A fresh database still needs one delivery.

## Declarative connections (`scarab.connections`, ADR-0060 part D)

A forge connection can be **declared in config** instead of created through the
UI. Config-declared connections are provisioned at boot, are **authoritative**,
and are read-only in the UI ("managed by configuration"). This is the only way to
onboard a Forgejo host, and its repos as Projects, without any API call.

```sh
kubectl -n scarab create secret generic scarab-forgejo \
  --from-literal=FORGEJO_CI_TOKEN=<token>

helm upgrade --install scarab deploy/helm/scarab -n scarab \
  --set secrets.existingSecret=scarab-forgejo \
  --set-json 'scarab.connections=[{"id":"forgejo-main","kind":"forgejo","org":"acme","base_url":"https://git.example.com","credential":{"env":"FORGEJO_CI_TOKEN"},"repos":["acme/widgets"]}]'
```

A connection serves one org, named by `org` or taken from the owner of its `repos`
when they all have one. Every repo in `repos` must be owned by that org, or the boot
fails.

The block is rendered verbatim into a mounted ConfigMap and
`SCARAB_CONNECTIONS_FILE` points at it, so the keys are the **server's** schema
(`base_url`, `secret_ref`: snake_case, not the chart's camelCase). An unknown key
fails the boot with an error instead of being ignored (ADR-0048).

**Each connection has exactly one owner.** A connection is owned by the config
*or* by the database. Declaring an id the database already owns (e.g. one a
GitHub `installation` delivery created) **refuses the boot** with an error,
rather than letting the two copies drift apart with nothing to decide between
them. Removing an entry hands ownership back to the UI. It never deletes the
connection, because Projects (and their Environments, secrets and RBAC) depend on
its repo bindings.

**Credentials resolve by one path: env override, then Scarab's secret store.**
`credential: {env: VAR}` / `{file: PATH}` supply the material from the deployment
(missing or empty ⇒ boot failure); `credential: {secret_ref: HANDLE}` resolves the
`_forge`-scoped handle at use-time (unregistered ⇒ reported DEGRADED, since only
the running server can store it). `SCARAB_GITHUB_APP_PEM[_FILE]` above is the same
mechanism, applied kind-wide to GitHub App-mode connections.

## Where step Pods run (`scarab.namespace`)

Step Pods run pipeline code, so by default they run in a namespace of their
own, `<release namespace>-steps`, not beside the server's Secret, its
ServiceAccount and the Data Depot. The chart renders that namespace and a Role
in it for the server's ServiceAccount: step Pods and their logs, exec and
events, the Secrets the server creates for each Pod, and the Services and
NetworkPolicies of shared services. Step Pods reach the Data Depot and the
server's results API through Services by their full DNS names, so the server
needs no Kubernetes API rights in its own namespace for them.

| Values | Step Pods run in |
|--------|------------------|
| `scarab.namespace` empty (default) | `<release namespace>-steps`, which the chart creates |
| `scarab.namespace` empty, `scarab.createNamespace: false` | the release namespace, beside the server |
| `scarab.namespace` empty, `rbac.create: false` | the release namespace, where you grant the server's ServiceAccount its rights |
| `scarab.namespace` empty, `scarab.executor: local` | no step Pods; the chart renders no Namespace |
| `scarab.namespace: <name>` | `<name>`, which you create before the install; the chart renders the Role into it |
| `scarab.namespace: <release namespace>` | the release namespace, beside the server (the behaviour up to 0.4.4) |

Creating a Namespace needs cluster-wide rights. An installer with rights only in
the release namespace sets `scarab.createNamespace: false`, or names a namespace
that already exists in `scarab.namespace`. To keep step Pods apart and create
the namespace yourself, set `scarab.namespace: <release namespace>-steps` and
create it before the install. With `rbac.create: false` the chart renders no
Role, so the default is the release namespace, where your own RoleBinding is;
name another namespace only if you grant the server's ServiceAccount the
executor's rights there (`templates/rbac.yaml` lists them). A release namespace
longer than 57 characters makes `<release namespace>-steps` too long: the
render fails and asks for `scarab.namespace`.

What you set up for step Pods belongs in the step namespace: a LimitRange or
ResourceQuota, `imagePullSecrets` on its `default` ServiceAccount, and every
ServiceAccount a placement profile names (an IRSA role's, for example).

The chart renders no Pod Security label on the namespace: a `rootless-buildkit`
step needs an Unconfined seccomp profile, which the `baseline` level refuses.
`helm uninstall` leaves the namespace and any step Pods in it
(`helm.sh/resource-policy: keep`); a reinstall under the same release name and
namespace adopts it again.

## The step Pod admission policy (`scarab.admissionPolicy`)

Any Pod can mount any Secret and run as any ServiceAccount in its own namespace: the kubelet
resolves those references without asking the Pod's ServiceAccount, and RBAC cannot limit `create
pods` by what the Pod refers to. Scarab's Pod builder keeps step Pods off the server's Secret and
ServiceAccount. The chart also installs a ValidatingAdmissionPolicy, so the cluster refuses such a
Pod if Scarab has a bug.

The policy holds every Pod the server's ServiceAccount creates in the step namespace, in both
namespace modes: `<release namespace>-steps`, and `scarab.namespace` set to the release namespace.
It refuses a Pod that:

- names a Secret in a Secret volume, a projected volume, an env `secretKeyRef` or an `envFrom`
  `secretRef`, other than the Pod's own per-Pod Secrets (`<pod>-v-secrets`, `<pod>-v-token`,
  `<pod>-v-registry`, `<pod>-v-oidc`, `<pod>-workspace`, and the names a server before 0.7
  wrote: `<pod>-secrets`, `<pod>-token`, `<pod>-registry`, `<pod>-oidc`), the Secrets your
  `scarab.placement` baseline and profiles name, and `scarab.admissionPolicy.allowSecrets`;
- runs as a ServiceAccount other than `default`, the ServiceAccounts your placement baseline and
  profiles name, and `scarab.admissionPolicy.allowServiceAccounts`;
- has a `hostPath` volume, `hostNetwork`, `hostPID` or `hostIPC`.

The old names are allowed so that Pods a 0.6 server made still pass during an upgrade. Upgrade the
chart with the server image: under a 0.6 chart's policy, which lists only the old names, every step
Pod a 0.7 server makes with one of the `-v-` Secrets is refused, and its step fails.

The policy cannot tell which profile a Pod was placed with, so a Secret or ServiceAccount any
profile names is allowed on every step Pod. It does not check `privileged` or added capabilities:
Scarab allows those per step by grant, which the policy cannot see. Use Pod Security on the step
namespace for those (see above for why the chart sets no label). It does not check `imagePullSecrets`. Nor does it check CSI
`nodePublishSecretRef`, the Secret references of the older in-tree volume types, or ConfigMap
references: when step Pods share the release namespace, the server's ConfigMaps sit beside them.

The server's own Pods are not held by it when they share the namespace: the ReplicaSet and
StatefulSet controllers create the server's and the Data Depot's Pods, not the server's
ServiceAccount. Plugin Pods and the plugin isolation probe run in `plugins.namespace`, which the
chart keeps apart from the step namespace and labels Pod Security `restricted`.

The two allow lists are for names the chart cannot see, such as the profiles of a placement file
you mount through `extraVolumes` instead of `scarab.placement`.

A step whose Pod the policy refuses fails with the policy's reason, for example `Scarab step Pod
policy: a Secret volume names a Secret that is not the Pod's own`; the Run does not retry the
launch.

The policy and its binding are cluster-scoped: the account that runs `helm install` or `helm
upgrade` needs rights to create `validatingadmissionpolicies` and
`validatingadmissionpolicybindings` (`admissionregistration.k8s.io`). With `scarab.createNamespace:
false`, the setting for an installer with rights in the release namespace only, the chart leaves
the policy out and the install notes say so; set `scarab.admissionPolicy.enabled: true` to install
it anyway. `enabled: auto` (the default) means on unless `scarab.createNamespace` is false.

The policy needs Kubernetes 1.30 or later (`admissionregistration.k8s.io/v1`). On an older cluster
the chart installs no policy and the install notes say so. `helm template` without a cluster
cannot see the API: pass `--api-versions
admissionregistration.k8s.io/v1/ValidatingAdmissionPolicy` or set `scarab.admissionPolicy.force:
true`.

To try it before it refuses anything, run it in Audit mode: the API server creates the Pod and records what it
would have refused in its audit log (when the API server has audit logging on).

```yaml
scarab:
  admissionPolicy:
    validationActions: [Audit]
```

When the audit log shows no `scarab-step-pods.*` refusals for your pipelines, set it back to
`[Deny]`.

With the policy on, the render fails when:

- your placement baseline or a profile sets a `hostPath` volume, `hostNetwork`, `hostPID` or
  `hostIPC`: every step Pod placed with it would be refused. The error names the baseline or the
  profile. Remove the field, or set `scarab.admissionPolicy.enabled: false`.
- step Pods share the release namespace and the server's ServiceAccount is one the policy allows:
  `default` (with `serviceAccount.create: false` and no `serviceAccount.name`), or a name in a
  profile or `allowServiceAccounts`. A step Pod could then run as the server. Give the server its
  own ServiceAccount, run step Pods in another namespace, or turn the policy off.

| Key | Default | Notes |
|-----|---------|-------|
| `scarab.admissionPolicy.enabled` | `auto` | `auto`: on unless `scarab.createNamespace` is false; `true`; `false` |
| `scarab.admissionPolicy.force` | `false` | install it even when Helm cannot see the API |
| `scarab.admissionPolicy.validationActions` | `[Deny]` | `[Audit]` for a trial, `[Warn]` to warn the server only |
| `scarab.admissionPolicy.failurePolicy` | `Fail` | `Fail` refuses a Pod when the policy cannot be evaluated |
| `scarab.admissionPolicy.allowSecrets` | `[]` | Secrets a step Pod may name besides its own and the placement config's |
| `scarab.admissionPolicy.allowServiceAccounts` | `[]` | ServiceAccounts a step Pod may run as besides `default` and the placement config's |

The policy is `templates/admission-policy.yaml`.

## Step Pod placement (`scarab.placement`, ADR-0055)

Step Pods are created by the server at run time, so the chart's `tolerations` /
`nodeSelector` (which place the **server**) never reach them. On a cluster whose
every node pool is tainted, step Pods (checkout Steps included) stay `Pending`
until you give them tolerations here:

```yaml
scarab:
  placement:
    baseline:            # merged onto EVERY step Pod
      spec:
        tolerations:
          - { key: dedicated, operator: Equal, value: ci, effect: NoSchedule }
        nodeSelector:
          pool: ci
    profiles:            # opt-in per step: `placement_profiles: [big]`
      - name: big
        k8s:
          spec:
            nodeSelector: { pool: ci-large }
      - name: ecr-push   # a push role: only for a step that pushes
        registry_auth_only: true
        k8s:
          spec:
            serviceAccountName: ecr-pusher
```

**Mark a profile that grants push access `registry_auth_only: true`.** Pipeline
authors name profiles, so without the marker any step could ask for a
ServiceAccount with a push role. With it, the server applies the profile only to
a step with `registry_auth` that the launch found eligible for a credential (a
step from a library the org trusts with the registry credential, not a fork pull
request);
named on any other step it fails that step's launch, and on a fork's push step it
is left out. For `lib:thulasi-ram::scarab-ci::build`, route it to the push step:
`placement_profiles: { image: [arm64], push: [ecr-push] }`. A marked profile
cannot also be `default: true`; the server refuses that config at boot.

**`baseline` and `k8s` are Pod fragments, not pod-spec fragments.** They are
strategic-merged onto the whole Pod, as Kubernetes does, so tolerations live
under `spec:`. Containers, volumes and env merge by name; a profile's
`tolerations` are added to the baseline's. The pipeline syntax page has the
full rules.

The block is rendered verbatim into a mounted ConfigMap and
`SCARAB_PLACEMENT_CONFIG_FILE` points at it, so the keys are the **server's**
schema (`baseline`, `default_resources`, `profiles`, `author_overlay`). The server silently accepts
two mistakes: an unknown top-level key, and a fragment key other than
`metadata`/`spec` (dropped when the merged Pod is decoded). Either one leaves
every step Pod `Pending` with no error. The chart **fails the render** on both
instead. A change rolls the server (it is read once at boot).

### Keys a step's own overlay may set (`author_overlay`)

Where an Environment permits it, a step can carry its own `k8s_overlay`. Labels,
annotations and tolerations in it are refused unless their key is listed here:

```yaml
scarab:
  placement:
    author_overlay:
      label_keys: [team, cost-center]
      annotation_keys: ["karpenter.sh/do-not-disrupt", "prometheus.io/*"]
      toleration_keys: [nvidia.com/gpu]
```

Each entry is an exact key, `prefix/*` (every key under that prefix) or `*`
(every key without a prefix). Nothing is listed by default. Keys under
`scarab.io/`, `scarab.dev/`, `app.kubernetes.io/` and `node-role.kubernetes.io/`,
the AppArmor and seccomp annotations, and a toleration with an empty or missing
key stay refused even when listed. A misspelled key inside `author_overlay`
stops the server at boot; a malformed entry fails validation. A label a
NetworkPolicy or Service selects on, or an annotation an admission webhook acts
on (an IAM role, a mesh or Vault injector), is safer in a placement profile,
which the operator writes.

## The workspace service (`workspace`, ADR-0061)

A second workload in the same release: a **StatefulSet** running the **same
image** with `SCARAB_ROLE=workspace`, holding a warm content-addressed store of
**Workspace Snapshots** on a PVC, with your object store behind it as the cold
archive. It is part of the standard path, not an optional speed-up, because a
fast path plus a fallback path means two code paths to reason about.

```sh
helm upgrade --install scarab deploy/helm/scarab -n scarab \
  --set secrets.workspaceTokenSecret="$(head -c 32 /dev/urandom | base64)" \
  --set workspace.persistence.size=50Gi
```

Why it is built this way:

- **Same image, one release.** That makes version skew between server and
  service impossible. It is also why this is a *role* on the converged binary
  rather than a fourth published artifact.
- **`SCARAB_ROLE=workspace` is set as an explicit `env` entry**, which wins over
  the ConfigMap arriving via `envFrom`. That one line matters: if the precedence
  is wrong, this StatefulSet silently runs a second converged control plane,
  driver loop included. Confirm it after any edit:
  ```sh
  helm template scarab deploy/helm/scarab -n scarab \
    --set scarab.devInsecure=true --set secrets.databaseUrl=x \
    --set secrets.masterKey=y --set secrets.workspaceTokenSecret=z \
    | yq 'select(.kind=="StatefulSet").spec.template.spec.containers[0].env'
  ```
- **The volume attaches to the SERVICE, never to a step Pod.** Every problem PVCs
  have per step (attach/detach latency at each boundary, stuck volumes when a
  spot node is reclaimed, per-node attachment quotas) comes from binding volumes
  to short-lived pods.
- **No RBAC.** It gets its own ServiceAccount with **no RoleBinding**. This is
  deliberate: a workspace replica creates no Pods, reads
  no Secrets through the API and execs into nothing.
- **`secrets.workspaceTokenSecret` must be different from
  `secrets.resultsTokenSecret`.** The results token carries no verb and never
  expires, so reusing it would turn a results-write credential into a content
  read+write credential, and would let the workspace service forge step results
  for any `{run, step, attempt}`.
- **Nothing renders without that secret.** A workspace service with no token
  secret would serve every step's inputs to anything that can reach the port, so
  the chart renders nothing rather than an open service.

Operating it: `/readyz` is *warm writable + cold reachable*, not the
control plane's database check. The warm tier is bounded by **space**: an LRU
sweep runs every 60 seconds and, once `scarab_workspace_warm_used_bytes` passes
`scarab_workspace_warm_budget_bytes` (`workspace.warmBudgetBytes`, default 90%
of the volume), evicts down to 80% of the budget. It evicts content that is
already durable in the packs first, then everything else (cache-only content, and
content whose drain hasn't committed yet). Both gauges are on
`/metrics`. `scarab_workspace_warm_evicted_bytes_total{class="cache_only"}`
climbing means the volume is too small for the working set and future reads
will miss. The sweep never evicts a file whose mtime is under an hour old (a
write sets it, and a read refreshes it at most once an hour), so a volume that
fills within an hour still runs out of space:
`scarab_workspace_warm_full_total` counts those failed writes. `scarab_workspace_warm_write_failed_total`
climbing means snapshots are still durable (cold succeeded) but the cache is
not being filled.

**`workspace.persistence.storageClass` backs the warm CAS**, the Depot's
local cache tier in front of the cold object store (ADR-0066: the Depot is a
cache). The warm tier exists so that reads and writes are local syscalls, so
prefer local disk. An NFS-backed class adds a network round trip to every blob,
which is the cost the warm tier is there to remove.

**More than one replica.** Drains and input fetches can land on any replica.
Two replicas pass the ADR-0077 rank 1 fault runs (`just test chaos`). A Cache is the
exception: it lives only in one replica's warm tier. Above one replica the
chart renders one Service per replica, `<fullname>-workspace-<ordinal>`, and the
server gives each step Pod the replica chosen from its cache key, its home. The
Pod saves and restores that Cache through the home and sends everything else
through `<fullname>-workspace`. A home that is down, has evicted the Cache, or
is no longer the home after `workspace.replicaCount` changes is a cache miss:
the step rebuilds the directory. Changing the count moves about 1/N of the keys.
Change the count with `workspace.replicaCount` and a release upgrade, not
`kubectl scale`: the server reads the count from `SCARAB_WORKSPACE_REPLICAS`,
which the chart copies from that value, so after a scale it picks homes over
the old count until the release is upgraded.
The StatefulSet's `serviceName` is unchanged, so upgrading needs no migration.

> **Unverified in this chart version:** per-availability-zone placement.

### Upgrading from 0.8.4: Fork Run approval

Migration 0114 adds the table `org_fork_run_approval`: each org's Fork Run
approval setting. It is a new table, so it rewrites no row and locks no other
table; it waits at most 5 seconds for its lock, and on a busy database the
server stops with `canceling statement due to lock timeout` and runs it again
when the Pod restarts. It is numbered 0114, after 0112 and 0113, which two
other changes of this release line add. They can land in any order: the
migrator runs every migration a database has not run, whatever its number,
so a database that ran 0114 first still runs 0112 and 0113 when they arrive,
and none of the three depends on another. Every org starts on the default, `agent_started`: a Run of
fork code that an app started through an `oauth` token waits for approval.
Nothing issues such a token yet, so no Run changes until an admin picks `all`
(Org settings › Security).

A Run of fork code the setting holds gets one `manual` gate,
`fork-run-approval`, in front of every root step. A Rerun in such a Run waits
at the gate again, and asks the other gates between it and the rerun step
again too; a Retry does not.

0.8.4 is the release to roll back to: it reads a Run that has the gate, holds
it and its shared services at the gate, and never counts the gate as an
Environment approval gate. A server before 0.8.4 cannot load such a Run, so
do not roll back past 0.8.4 while one exists. On 0.8.4 the gate is weaker: an
issued API token releases it without a confirmation, and a Rerun of a Run
whose gate was released does not put the gate back (0.8.4 has no re-arm), so
the rerun steps start at once. So before rolling back, set the setting to
`off` and let the Runs that have the gate finish.

To roll back: set Fork Run approval to `off` in every org, approve or reject the
gates still waiting, scale the server to zero, back up the database, run
[`rollback/0114_revert.sql`](../../../crates/scarab-db-postgres/rollback/0114_revert.sql)
(`psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f 0114_revert.sql`; a second run
changes nothing), then the revert scripts of the other migrations above 0111
this database ran, newest first, then `0111_revert.sql` (see "Upgrading from
0.8.4: token kinds and operation lists"), then install the 0.8.4 chart. The
settings are dropped.

### Upgrading from 0.8.4: token kinds and operation lists

Migration 0111 adds four columns to `api_tokens`: `kind` (`NOT NULL DEFAULT
'issued'`), and the nullable `operations`, `audience` and `oauth_client`, and
one nullable column to `runs`, `origin_via`, the token that started a Run. No
row is rewritten, and every existing token reads as an issued token with no
operation list, so it works as before. A token minted with an operation list
is stored under a different hash, so a 0.8.4 replica still running during the
rolling update cannot find it and cannot let it use every operation its role
allows. The `ALTER` waits at most 5 seconds for
its lock on `api_tokens` and on `runs`; on a busy database the server stops with `canceling
statement due to lock timeout` and runs it again when the Pod restarts.

Two behaviour changes reach existing tokens. Every GET now names a read
operation (`run.read`, `step.logs.read`, …) instead of one shared `read`, so
the `op` of security events changes for reads. And an API token's approval of
any `manual` gate, a plain one included, now raises a confirmation request its
owner confirms; a script that approved plain gates with a token must confirm
each approval, or approve from a browser.

To roll back: scale the server to zero, back up the database, run
[`rollback/0111_revert.sql`](../../../crates/scarab-db-postgres/rollback/0111_revert.sql)
(`psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f 0111_revert.sql`; a second run
changes nothing), then install the 0.8.4 chart. A 0.8.4 server would let a
token with an operation list use every operation, so the revert revokes those
tokens before it drops the columns; their owners mint new ones. To go back to
0.8.2, run `0110_revert.sql` next.

### Upgrading from 0.8.3: rollback target for Fork Run approval

No migration. A later release adds Fork Run approval, an org setting that puts
a `manual` gate in front of a Run of fork code. 0.8.4 reads that gate; an
earlier 0.8.x server cannot load a Run that has it. Roll back from that release
to 0.8.4. On 0.8.4 such a Run waits at the gate as at any `manual` gate, and
a shared service waits until a Step that uses it may run. The gate is never an
Environment approval gate.

On 0.8.4 the gate is released like any `manual` gate: by anyone with write on
the repository, issued API tokens included, with no confirmation step. The
release that adds Fork Run approval is stricter: an API token cannot release
the gate itself, and its owner confirms each release.

A later release also adds `extensions` to plugin manifests. 0.8.4 reads and
keeps the field of an installation that release made, so the org's plugin list
still loads after a rollback, but it does not act on it: such a plugin gets no
endpoint and no trust tier on 0.8.4. 0.8.4 refuses to install, or upgrade to, a
manifest that declares `extensions`.

### Upgrading from 0.8.2: gate notes

Migration 0110 adds two nullable columns to `attempts`, `gate_note` and
`gate_note_at`: the note a plugin posts on its gate's open wait, which the Run
page shows. No row is rewritten. The `ALTER` waits at most 5 seconds for its
lock on `attempts`; on a busy database the server stops with `canceling
statement due to lock timeout` and runs it again when the Pod restarts.

To roll back: scale the server to zero, back up the database, run
[`rollback/0110_revert.sql`](../../../crates/scarab-db-postgres/rollback/0110_revert.sql)
(`psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f 0110_revert.sql`; a second run
changes nothing), then install the 0.8.2 chart. The notes are dropped. To go
back to 0.7.5, run `0109_revert.sql` next.

Plugin Deployments are renamed from `plugin-<id>` to
`scarab-plugin-<name>-<id>`. The server deletes each old Deployment and creates
the new one once the old Pod is gone, so a plugin is down for about 15 to 30
seconds after the upgrade. A 0.8.2 server knows only the old names: when you
roll back, delete the new Deployments while the server is scaled to zero
(`kubectl -n scarab-plugins delete deployment -l scarab.io/plugin-installation`),
or 0.8.2 starts a second copy of each plugin beside them.

### Upgrading from 0.7.7: build libraries and IR version 4

No migration. The library changes below take effect when an org admin
upgrades a library to the version this release tags.

- **Order: servers first, then libraries and `join`.** Upgrade every server
  replica before an org admin upgrades `build-multiarch`, and before any file
  calls `join`. A 0.7.7 server cannot parse the new `build-multiarch` (a matrix
  dimension written as text), so a Run that invokes it is refused at creation
  there; and it fails a trigger `when:` that calls `join`, so that delivery
  makes no Run. Redeliver the webhook, or rerun, once every replica is
  upgraded. To roll back to 0.7.7, first put the library back to its earlier
  version.
- **`build`'s push step needs step results.** With `push: true` it refuses
  (exit 2) when the server collects no step results, with
  `scarab: push refused: results are not collected, …; set the server's results
  token secret (SCARAB_RESULTS_TOKEN_SECRET, the chart value
  secrets.resultsTokenSecret)`. Earlier versions pushed without checking the
  layout against the build step's digest. Set `secrets.resultsTokenSecret` (or
  a `SCARAB_RESULTS_TOKEN_SECRET` key in `secrets.existingSecret`) before the
  orgs that push images upgrade the library. `build-multiarch` and
  `image-index` with `layouts` needed results already, and now fail with
  `scarab: index refused: results are not collected, …`, which names the same
  secret.
- **`build-multiarch`'s placement keys change.** Its `amd64` and `arm64`
  steps are now instances of one matrix step, `build`. An invoke whose
  `placement_profiles` map names `amd64:` or `arm64:` fails to compile, and the
  error names the key to write instead, `"build[arch=linux/amd64]"`. Step ids
  change from `app/amd64` to `app/build[arch=linux/amd64]`.
- **IR version 4.** A Pipeline that invokes the new `build-multiarch`, or any
  library step that reads a matrix step's instances, or a template, `when:` or
  `validate:` that calls `join`, compiles to IR version 4. A 0.7.7 server
  refuses to launch a step of such a Run. The chart's default
  `strategy: Recreate` stops every old server first; with a rolling update,
  rerun a step that failed that way once every replica runs the new version.
  To roll back to 0.7.7, let version-4 Runs finish first, or cancel them.

Two step id rules are new for every Pipeline: an id may not start with
`<invoke id>/` of an invoke beside it, and a `[` or `]` in an id must pair.
Neither was used in Scarab's own files.

### Upgrading from 0.7.5: leak-check limits

Migration 0109 adds the table `org_leak_check_limits`, which holds the limits
an org Owner sets on leak checks. It is a new table: it takes no lock on
another, and every org starts on the defaults (20 checks per person an hour,
40 a day).

To roll back: scale the server to zero
(`kubectl -n scarab scale deploy/scarab --replicas=0`), back up the database,
run
[`rollback/0109_revert.sql`](../../../crates/scarab-db-postgres/rollback/0109_revert.sql)
(`psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f 0109_revert.sql`; a second run
changes nothing), then install the 0.7.5 chart. The limits Owners set are
dropped. To go back to 0.7.2, run `0106_revert.sql` next.

### Upgrading from 0.7.2: saved pipeline files

Migrations 0106, 0107 and 0108 add nullable columns to `runs`: the pipeline
file a Run was created from (`pipeline_path`, `pipeline_source`,
`pipeline_source_sha`), the newer Run that replaced it (`superseded_by`) and a
pull request's head branch (`origin_pr_head`). No row is rewritten and none is
backfilled, so Runs from before the upgrade have no saved file, no recorded
replacement and no head branch. Each `ALTER` waits at most 5 seconds for its
lock on `runs`; on a busy database the server stops with `canceling statement
due to lock timeout`, Kubernetes restarts it, and it tries again. While one
waits, reads and writes of `runs` wait too, up to 5 seconds per migration.

To roll back: scale the server to zero
(`kubectl -n scarab scale deploy/scarab --replicas=0`), back up the database,
run
[`rollback/0106_revert.sql`](../../../crates/scarab-db-postgres/rollback/0106_revert.sql)
(`psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f 0106_revert.sql`; a second run
changes nothing), then install the 0.7.2 chart. The saved files, the
replacements and the head branches are dropped. To go back to 0.7.1, run
`0104_revert.sql` next.

### Upgrading from 0.7.1: webhook delivery outcomes

Migrations 0104 and 0105 record what each webhook delivery decided for each
Pipeline file, so a redelivery creates only the Runs it is missing.

- **0104** runs in a transaction with a 5-second lock timeout, as 0101 does. It
  adds the `webhook_delivery_pipelines` table, `runs.replaced_by`, and six
  columns on `webhook_deliveries` (`unfinished`, `config_sha`, `head_scope`,
  `head_sha`, `try_until`, `try_token`). No row is rewritten: `unfinished` has
  a constant default and the other columns are nullable. Each `ALTER` takes an
  ACCESS EXCLUSIVE lock for a moment; `runs` comes first, so while the
  migration waits for it no other table is locked. `runs` is written on every
  scheduler tick, so on a busy database the wait can pass 5 seconds; the
  server then stops with `canceling statement due to lock timeout`,
  Kubernetes restarts it, and it tries again. While it waits, reads and
  writes of `runs` wait too, up to 5 seconds per start attempt.
- **0105** builds `webhook_deliveries_at_idx` with `CREATE INDEX CONCURRENTLY`,
  outside a transaction, so deliveries go on being recorded while it builds. A
  build that fails leaves an invalid index; the server drops it and builds it
  again when it starts.

Deliveries recorded before the upgrade read as finished: a redelivery of one is
answered `duplicate delivery`, as in 0.7.1. During a rolling update, a delivery
a new server answered 502 is answered `duplicate delivery` if its redelivery
reaches a 0.7.1 server; redeliver it again once the rollout ends.

The retention sweeper now deletes deliveries older than
`retention.webhookDeliveryDays` (default 30, at least 7). `webhook_deliveries`
was never pruned before, so the first passes on an old database delete 1,000
deliveries at a time.

To roll back: scale the server to zero
(`kubectl -n scarab scale deploy/scarab --replicas=0`), back up the database,
run
[`rollback/0104_revert.sql`](../../../crates/scarab-db-postgres/rollback/0104_revert.sql)
(`psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f 0104_revert.sql`; it reverts 0105
too, and a second run changes nothing), then install the 0.7.1 chart. 0.7.1
reads every recorded delivery as done, including ones the newer server left
unfinished, and a Run a redelivery replaced becomes an ordinary `Failed` Run.
To go back to 0.6.x, run it before `0099_revert.sql`.

### Upgrading to 0.7.0

Read the Breaking changes of 0.7.0 in `CHANGELOG.md` first. 0.7 changes how
logs are stored and how the secret values they are redacted with are kept.

1. Let running Runs finish
   (`kubectl -n <step namespace> get pods -l scarab.io/run` lists none). A step
   or shared service that runs across the upgrade stores no log after it: its
   log gets the note "the rest of this log was not stored: this step was
   started by an earlier Scarab version, so its secret values are not known".
2. Keep `strategy.type: Recreate`, the default. Once migration 0099 has run,
   the database refuses log chunks from a 0.6 server, and a 0.6 server does not
   start. With `RollingUpdate`, the 0.6 servers still running store no log, and
   a step one of them launches gets the same note and no log.
3. Upgrade the chart with the image. A 0.7 server mounts its per-Pod Secrets
   under the `<pod>-v-…` names, and only the 0.7 admission policy allows them
   ([The step Pod admission policy](#the-step-pod-admission-policy-scarabadmissionpolicy)).

Migration 0099 adds the log stream and value ledger tables; its checks on
`log_chunks` are `NOT VALID`, so it scans no stored chunk. Migration 0100
builds `attempts_started_at_idx` with `CREATE INDEX CONCURRENTLY`, outside a
transaction. A build that fails leaves an invalid index: drop it
(`DROP INDEX CONCURRENTLY attempts_started_at_idx;`) and start the server
again. Forge tokens minted by 0.6 are not revoked after the upgrade (0.7
revokes only tokens in a value ledger); a GitHub App token expires an hour
after it was minted. Never drop the `log_chunks_fenced` check on `log_chunks`:
during a rolling update it stops a 0.6 server that is still running from
storing output it cannot redact.

To roll back: a 0.6 server does not start on a database that ran 0099 (its
migrator finds version 99, which it does not know). Let Runs finish and stop
new ones, scale the server to zero
(`kubectl -n scarab scale deploy/scarab --replicas=0`), back up the database,
run
[`rollback/0099_revert.sql`](../../../crates/scarab-db-postgres/rollback/0099_revert.sql)
once (`psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f 0099_revert.sql`; it reverts
0100 to 0103 too), then install the 0.6.1 chart. The revert deletes the notes of logs
stored by 0.7 and every value ledger; output rows stay, and 0.6 reads them as
before. Upgrading again runs 0099 to 0103 again.

Migration 0101 (the first release after 0.7.0) validates three of 0099's
checks on `log_chunks` when the server starts. It waits at most 5 seconds for
its lock on the table. A manual `VACUUM` or `ANALYZE`, an anti-wraparound
autovacuum or a `CREATE INDEX CONCURRENTLY` on `log_chunks` holds that lock
longer, so the server stops with `canceling statement due to lock timeout`,
Kubernetes restarts it, and it tries again; it starts once the other session
ends. A plain autovacuum gives way after about a second. Right after it,
migration 0102 adds the `values_too_large` reason to `log_chunks_reason`
(dropped and added again `NOT VALID`, so it scans no row). Its lock waits for
every other session on `log_chunks`: an open transaction that read the table
(a `pg_dump`, a long `SELECT`) makes it stop with the lock timeout, and while
it waits, up to 5 seconds, log reads and writes wait behind it. Migration 0103
validates that check again in its own transaction, under the same lock and
5-second wait as 0101, with one more scan of `log_chunks`. To go back to 0.7.0,
scale the server to zero, run
`DELETE FROM _sqlx_migrations WHERE version IN (101, 102, 103);` and install
0.7.0.

### Upgrading from 0.4.4 or earlier: step Pods move

Up to 0.4.4 an empty `scarab.namespace` meant the release namespace. Now, with
the k8s executor, `rbac.create: true` and `scarab.createNamespace: true` (the
defaults), it means `<release namespace>-steps`. If your values leave it empty,
the upgrade:

- creates the Namespace `<release namespace>-steps` and moves the executor's
  Role and RoleBinding there, so the server has no rights left over step Pods
  in the release namespace;
- leaves step Pods that are running in the release namespace where they are.
  The new server looks for a step's Pod only in the new namespace: for a step
  still running it finds none and marks the Attempt lost, and the step's retry
  policy decides whether a new Attempt runs, in the new namespace. It does not
  delete the old Pods.

Let running Runs finish before the upgrade
(`kubectl -n <release namespace> get pods -l scarab.io/run` lists none), or set
`scarab.namespace` to the release namespace to keep step Pods where they are.
Delete leftover Pods once they end
(`kubectl -n <release namespace> delete pods -l scarab.io/run`). Before the
upgrade, recreate in the new namespace what you set up for step Pods
([Where step Pods run](#where-step-pods-run-scarabnamespace)).

Creating a Namespace needs cluster-wide rights. If the account that runs
`helm upgrade` has rights only in the release namespace, the upgrade fails;
set `scarab.createNamespace: false` (step Pods stay in the release namespace),
or name a namespace that already exists in `scarab.namespace`.

Installs that already set `scarab.namespace`, set `rbac.create: false`, or run
the local executor see no change.

### Upgrading to 0.3.1

Read the Breaking changes of 0.3.1 in `CHANGELOG.md` first. Back up the
database, and keep the server Deployment's `strategy.type: Recreate`: database
migration 0093 gives every person an internal id, signs everyone out, and a
0.3.0 server must not run against the migrated database.

0.3.1 reads no release manifest and installs no library
into a new org: an org admin installs the clone library in each new org, and
on a Forgejo or GitHub token connection also trusts its source with forge
credentials. The values `scarab.libsManifest` and `plugins.argocdRepo` are
gone; the chart ignores them if your values still set them. Database
migration 0090 trusts the clone library's source in every org that had it
from 0.3.0's manifest, so those orgs keep cloning. Read the Breaking changes in
`CHANGELOG.md`; the docs site's Libraries and Deploying with Argo CD pages
describe how to install the clone library and a plugin.

### Upgrading to 0.3.0

Read the Breaking changes of 0.3.0 in `CHANGELOG.md` first. Two of them need an
action:

- The server Deployment now defaults to `strategy.type: Recreate`, because a
  0.2.x and a 0.3.0 server must not run at the same time. If you set
  `strategy` to `RollingUpdate`, set it back for this upgrade.
- The `clone:` Step kind is replaced by the first-party library
  `lib:thulasi-ram/clone`. A new org gets it installed when its first
  repository is bound; an org that existed before the upgrade does not. In
  each existing org, an org admin opens **Settings › Libraries › Install
  library**, picks the clone library from the release manifest, reviews it,
  and confirms with a passkey. (From 0.3.1 there is no
  release manifest: give the source
  `github.com/thulasi-ram/scarab-ci//libs/clone.yaml@<commit of a release tag>`
  instead.) Until then a push whose Pipeline invokes it is
  refused with "`lib:thulasi-ram/clone` is not installed in this org", shown as a
  refused row in the repository's Runs list.

### Upgrading from 0.2.1 or earlier

Up to 0.2.1 the StatefulSet's `volumeClaimTemplates` carried the chart and app
version as labels, and Kubernetes never lets an update change that field, so
every upgrade failed with `spec: Forbidden: updates to statefulset spec for
fields other than 'replicas', 'ordinals', 'template', ...`. From 0.2.2 the
template is version-free, but an existing StatefulSet still holds the old
labels. Delete it once, keeping its Pod and volume, then upgrade:

```sh
kubectl -n scarab delete sts --cascade=orphan \
  -l app.kubernetes.io/name=scarab-workspace,app.kubernetes.io/instance=<release>
helm upgrade scarab oci://ghcr.io/thulasi-ram/charts/scarab --version 0.2.2 ...
```

The recreated StatefulSet re-adopts the existing PVC by name (`cas-<sts>-0`);
the old PVC keeps its old labels. Expect
one restart of the workspace Pod. Later upgrades apply in place; the chart's CI
renders two versions and fails if any immutable StatefulSet field differs.

## Plugins (`plugins`, ADR-0072)

Off by default. `plugins.enabled=true` runs the plugin runtime: Scarab manages
one Deployment per installed plugin in the shared `plugins.namespace`, and
plugins talk to Scarab only through the **CredentialBroker**: a second listener
on `plugins.brokerPort`, exposed by the ClusterIP Service `<fullname>-broker`
and never through the ingress. No built plugin capability acts on Scarab yet
(ADR-0074 removed the Argo CD reporter), so you need this only to work on a
plugin. A plugin that needs storage brings its own: a PersistentVolumeClaim you
create in `plugins.namespace`, which the runtime mounts and Scarab never
manages.

It renders the namespace (labelled Pod Security `restricted`; set
`plugins.createNamespace=false` to bring your own, with that label), a Role
there, `get` on that one namespace, and a binding to `system:auth-delegator`
(TokenReview, which is how the broker verifies a plugin's projected token).

**The cluster must enforce NetworkPolicy.** The runtime writes a default-deny
policy plus one egress allowance, to the broker and nothing else. That excludes
DNS: the cluster resolver forwards any name upstream, so a DNS allowance would be a
tunnel out. Plugins are handed the broker by IP instead (the runtime resolves
the broker Service to its ClusterIP every pass). Before any plugin starts, and
every 6 h, the runtime runs an isolation probe in the namespace. On a cluster
whose CNI ignores NetworkPolicy the probe fails and the plugin stays down, its
installation showing `probe_failed` and why. A probe that cannot tell (its Pods
did not run, say) starts nothing and runs again on the next pass; a plugin
already running keeps running until 12 h after the last probe that passed, and
is then held as `probe_failed` too, until a probe passes. Each plugin Pod also
re-checks its own network in an `isolation-self-check` initContainer (the probe
image) before the plugin starts, on whichever node it landed: the broker
reachable; the API
server, the internet, `169.254.169.254` and DNS not. Until that passes the
plugin does not run. The probe's control Pod (never a plugin) uses the cluster
DNS, which must be the usual `kube-system` Pods labelled `k8s-app=kube-dns`.

## Key values

| Key | Default | Notes |
|-----|---------|-------|
| `image.repository` / `image.tag` | `ghcr.io/thulasi-ram/scarab-server` / appVersion | container image |
| `strategy` | `type: Recreate` | server Deployment update strategy: old server Pods stop before new ones start, so two versions never run Steps side by side. Switch to `RollingUpdate` only where a release's notes say mixed versions are safe |
| `scarab.role` | `converged` | `api` / `scheduler` / `executor` / `webhook` / `converged` |
| `scarab.executor` | `k8s` | `k8s` (prod) or `local` (dev only) |
| `scarab.namespace` | `<release ns>-steps` | namespace step Pods launch into (RBAC granted there); see [Where step Pods run](#where-step-pods-run-scarabnamespace) |
| `scarab.createNamespace` | `true` | render `<release ns>-steps` when `scarab.namespace` is empty; `false` runs step Pods in the release namespace |
| `scarab.s3.*` | — | object store; set `bucket` to enable S3/MinIO |
| `scarab.oauth.*` | — | OAuth/OIDC login: `clientId`, `authorizeUrl`, `tokenUrl`, `userinfoUrl`, `scopes`, `owners`, `issuer` (above) |
| `scarab.devInsecure` | `false` | ⚠ dev/eval only — no auth; mutually exclusive with `scarab.oauth` |
| `secrets.*` / `secrets.existingSecret` | — | sensitive `SCARAB_*` env |
| `secrets.oauthClientSecret.name` / `.key` | — / `oauth-client-secret` | OAuth client secret by reference from your own Secret (above) |
| `secrets.githubAppPemSecret.name` / `.key` | — / `github-app.pem` | mount the App PEM from your own Secret (above) |
| `scarab.connections` | `[]` | declarative, config-owned forge connections (above) |
| `scarab.placement` | `{}` | step Pod baseline + PlacementProfiles (above); required on an all-tainted cluster |
| `scarab.placement.author_overlay` | nothing listed | label, annotation and toleration keys a step's own `k8s_overlay` may set (above) |
| `workspace.enabled` | `true` | the ADR-0061 workspace service; renders only when a workspace token secret exists (above) |
| `workspace.dataDir` | `/var/lib/scarab/cas` | where the warm tier's PVC is mounted (`SCARAB_WORKSPACE_DATA_DIR`) |
| `workspace.persistence.size` / `.storageClass` | `20Gi` / cluster default | the warm tier's volume — bounded by SPACE via the LRU sweep (git-bug cba7165); prefer local disk (above) |
| `workspace.warmBudgetBytes` | `""` = 90% of the volume | warm space bound in plain bytes (`SCARAB_WORKSPACE_WARM_BUDGET_BYTES`); the sweep evicts to 80% of it, committed-durable content first |
| `workspace.blobAuthz` | `""` = `log` | blob-read authorization mode (`SCARAB_DEPOT_BLOB_AUTHZ`, ticket 52ef3aa): `off`/`log`/`enforce`; flip to `enforce` after `scarab_depot_blob_authz_would_deny_total` stays zero over a representative window. Caveat: `log` runs the real closure walk, so a roots claim holding an unwalkable root (absent from warm+packs+cold) 500s blob reads that miss the allowlist where `off` would have served them — intended fail-closed-on-availability behavior |
| `workspace.replicaCount` | `1` | one per failure domain; above 1, one Service per replica carries each Cache to its home replica (above) |
| `workspace.homeUrlTemplate` | `""` = `http://<fullname>-workspace-{ordinal}.<ns>.svc` | one replica's URL with `{ordinal}` (`SCARAB_WORKSPACE_HOME_URL_TEMPLATE`); set it only beside `scarab.workspaceUrl` in a split install |
| `secrets.workspaceTokenSecret` | — | HMAC secret for the workspace token; MUST differ from `resultsTokenSecret` (above) |
| `scarab.workspaceUrl` | — | override the in-cluster workspace Service URL (split installs) |
| `rbac.create` | `true` | Role/RoleBinding for Pod execution |
| `plugins.enabled` | `false` | the ADR-0072 plugin runtime + CredentialBroker (above) |
| `plugins.namespace` / `.createNamespace` | `scarab-plugins` / `true` | the shared plugin namespace; must enforce Pod Security `restricted`. A namespace of its own: the render fails on the release namespace, `scarab.namespace`, `default` or `kube-*` |
| `plugins.brokerPort` | `8090` | the broker listener and its ClusterIP Service port |
| `plugins.probeImage` | `""` = `busybox:1.36.1@sha256:73aa…` | the isolation probe's and the per-Pod self-check's image; digest-pin an override |
| `ingress.enabled` | `false` | expose the HTTP API. The web UI, the CLI and the results sidecar name a Step by its key and need nothing from the proxy; an API caller that sends a Step id holding `/` needs a proxy that passes `%2F` on unchanged (with Envoy Gateway, `escapedSlashesAction: KeepUnchanged` in a `ClientTrafficPolicy`) |
| `scarab.resultsApiUrl` | `""` = `http://<fullname>.<ns>.svc` | where step Pods post results; leave it empty, or point it at the in-cluster Service, not at a gateway that redirects or rewrites paths |
| `service.type` / `service.nodePort` | `ClusterIP` / `""` | the API Service; with `NodePort`, `nodePort` fixes the port (empty => assigned) |
| `command` / `args` | `[]` / `[]` | override the server container's entrypoint and arguments; empty => the image's |
| `extraVolumes` / `extraVolumeMounts` | `[]` / `[]` | volumes added to the server Pod and mounted in its container, after the chart's own |
| `workspace.command` / `workspace.args` | `[]` / `[]` | the same override for the workspace container |
| `workspace.extraVolumes` / `workspace.extraVolumeMounts` | `[]` / `[]` | the same for the workspace Pod; Pod template only, never `volumeClaimTemplates` |

See `values.yaml` for every value. Every value maps to a `SCARAB_*` env var
read by `crates/scarab-server/src/config.rs`. Webhook verification binds a secret
per forge endpoint: `SCARAB_GITHUB_WEBHOOK_SECRET` for `/webhooks/github`
and `SCARAB_FORGEJO_WEBHOOK_SECRET` for `/webhooks/forgejo`. Both render from
dedicated values (`secrets.githubWebhookSecret` / `secrets.forgejoWebhookSecret`),
or supply them as keys of your own `secrets.existingSecret`.

## References

- [ADR-0005 — Tenancy & deployment; Kubernetes as the only backend](../../../docs/adr/0005-tenancy-and-k8s-only.md)
- [ADR-0016 — Code architecture: hexagonal + adapter crates + converged binary](../../../docs/adr/0016-code-architecture.md)
- [ADR-0046 — Forge auth is adapter-internal; GitHub + Forgejo adapters in v1](../../../docs/adr/0046-forge-auth-and-multi-adapter.md)
- [ADR-0048 — Fail-closed startup](../../../docs/adr/0048-fail-closed-startup.md)
- [ADR-0049 — Identity & access: forge-agnostic OAuth/OIDC login](../../../docs/adr/0049-identity-and-access.md)
- [ADR-0061 — Workspace data path: workspace service + node driver, lazy materialisation](../../../docs/adr/0061-workspace-data-path.md)
- [ADR-0062 — Workspace Export: laziness without a node driver (the Farm and Export rungs)](../../../docs/adr/0062-workspace-export-lazy-without-node-driver.md)
- [ADR-0072 — Plugins: a governed extension point](../../../docs/adr/0072-plugins-a-governed-extension-point.md)
