# scarab Helm chart

Deploys **scarab-server** (one binary, selectable roles) onto Kubernetes,
with the RBAC it needs to launch one Pod per step in its namespace.

## Prerequisites

- Kubernetes 1.25+
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
  --version 0.2.2 --namespace scarab --create-namespace -f values.yaml
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

On EKS you can drop the S3 keys entirely: leave `SCARAB_S3_ACCESS_KEY` /
`SCARAB_S3_SECRET_KEY` unset and Scarab uses AWS's ambient credential chain
(IRSA, Pod Identity, instance role). Annotate both ServiceAccounts with the
role, because the server and the workspace StatefulSet both reach the bucket:
`serviceAccount.annotations` and `workspace.serviceAccount.annotations`, e.g.
`eks.amazonaws.com/role-arn: arn:aws:iam::<acct>:role/scarab`. Step Pods run
under neither ServiceAccount, so they do not inherit the role.

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
`installation_repositories` webhook deliveries (or `deploy/local-helm/reseed.sh`
for a local loop). A fresh database still needs one delivery.

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
  --set-json 'scarab.connections=[{"id":"forgejo-main","kind":"forgejo","base_url":"https://git.example.com","credential":{"env":"FORGEJO_CI_TOKEN"},"repos":["acme/widgets"]}]'
```

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

## Step Pod placement (`scarab.placement`, ADR-0055)

Step Pods are created by the server at run time, so the chart's `tolerations` /
`nodeSelector` (which place the **server**) never reach them. On a cluster whose
every node pool is tainted, step Pods (clone steps included) stay `Pending`
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
```

**`baseline` and `k8s` are Pod fragments, not pod-spec fragments.** They are
RFC-7386 merge-patched onto the whole Pod, so tolerations live under `spec:`.

The block is rendered verbatim into a mounted ConfigMap and
`SCARAB_PLACEMENT_CONFIG_FILE` points at it, so the keys are the **server's**
schema (`baseline`, `default_resources`, `profiles`). The server silently accepts
two mistakes: an unknown top-level key, and a fragment key other than
`metadata`/`spec` (dropped when the merged Pod is decoded). Either one leaves
every step Pod `Pending` with no error. The chart **fails the render** on both
instead. A change rolls the server (it is read once at boot).

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

Operating it: `/readyz` is *warm writable + cold reachable*, deliberately not the
control plane's database check. The warm tier is bounded by **space** and **LRU
eviction is not implemented yet**, so `scarab_workspace_warm_used_bytes` on
`/metrics` is the whole budget. Alert on it against
`workspace.persistence.size`. `scarab_workspace_warm_write_failed_total`
climbing means snapshots are still durable (cold succeeded) but the cache is
not being filled.

**`workspace.persistence.storageClass` backs the warm CAS**, the Depot's
local cache tier in front of the cold object store (ADR-0066: the Depot is a
cache). The warm tier exists so that reads and writes are local syscalls, so
prefer local disk. An NFS-backed class adds a network round trip to every blob,
which is the cost the warm tier is there to remove.

> **Unverified in this chart version:** more than one replica, per-availability-
> zone placement, and reachability from a step Pod. `workspace.replicaCount > 1`
> has not been exercised.

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
one Deployment per installed plugin (e.g. the first-party Argo CD reporter) in
the shared `plugins.namespace`, and plugins talk to Scarab only through the
**CredentialBroker**: a second listener on `plugins.brokerPort`, exposed by
the ClusterIP Service `<fullname>-broker` and never through the ingress.

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
installation showing `probe_failed` and why. Each plugin Pod then re-checks its
own network in an `isolation-self-check` initContainer (the probe image) before
the plugin starts, on whichever node it landed: the broker reachable; the API
server, the internet, `169.254.169.254` and DNS not. Until that passes the
plugin does not run. The probe's control Pod (never a plugin) uses the cluster
DNS, which must be the usual `kube-system` Pods labelled `k8s-app=kube-dns`.

## Key values

| Key | Default | Notes |
|-----|---------|-------|
| `image.repository` / `image.tag` | `ghcr.io/thulasi-ram/scarab-server` / appVersion | container image |
| `scarab.role` | `converged` | `api` / `scheduler` / `executor` / `webhook` / `converged` |
| `scarab.executor` | `k8s` | `k8s` (prod) or `local` (dev only) |
| `scarab.namespace` | release ns | namespace step Pods launch into (RBAC granted there) |
| `scarab.s3.*` | — | object store; set `bucket` to enable S3/MinIO |
| `scarab.oauth.*` | — | OAuth/OIDC login: `clientId`, `authorizeUrl`, `tokenUrl`, `userinfoUrl`, `scopes`, `owners`, `issuer` (above) |
| `scarab.devInsecure` | `false` | ⚠ dev/eval only — no auth; mutually exclusive with `scarab.oauth` |
| `secrets.*` / `secrets.existingSecret` | — | sensitive `SCARAB_*` env |
| `secrets.oauthClientSecret.name` / `.key` | — / `oauth-client-secret` | OAuth client secret by reference from your own Secret (above) |
| `secrets.githubAppPemSecret.name` / `.key` | — / `github-app.pem` | mount the App PEM from your own Secret (above) |
| `scarab.connections` | `[]` | declarative, config-owned forge connections (above) |
| `scarab.placement` | `{}` | step Pod baseline + PlacementProfiles (above); required on an all-tainted cluster |
| `workspace.enabled` | `true` | the ADR-0061 workspace service; renders only when a workspace token secret exists (above) |
| `workspace.dataDir` | `/var/lib/scarab/cas` | where the warm tier's PVC is mounted (`SCARAB_WORKSPACE_DATA_DIR`) |
| `workspace.persistence.size` / `.storageClass` | `20Gi` / cluster default | the warm tier's volume — bounded by SPACE via the LRU sweep (git-bug cba7165); prefer local disk (above) |
| `workspace.warmBudgetBytes` | `""` = 90% of the volume | warm space bound in plain bytes (`SCARAB_WORKSPACE_WARM_BUDGET_BYTES`); the sweep evicts to 80% of it, committed-durable content first |
| `workspace.blobAuthz` | `""` = `log` | blob-read authorization mode (`SCARAB_DEPOT_BLOB_AUTHZ`, ticket 52ef3aa): `off`/`log`/`enforce`; flip to `enforce` after `scarab_depot_blob_authz_would_deny_total` stays zero over a representative window. Caveat: `log` runs the real closure walk, so a roots claim holding an unwalkable root (absent from warm+packs+cold) 500s blob reads that miss the allowlist where `off` would have served them — intended fail-closed-on-availability behavior |
| `workspace.replicaCount` | `1` | one per failure domain; `>1` is **unverified** |
| `secrets.workspaceTokenSecret` | — | HMAC secret for the workspace token; MUST differ from `resultsTokenSecret` (above) |
| `scarab.workspaceUrl` | — | override the in-cluster workspace Service URL (split installs) |
| `rbac.create` | `true` | Role/RoleBinding for Pod execution |
| `plugins.enabled` | `false` | the ADR-0072 plugin runtime + CredentialBroker (above) |
| `plugins.namespace` / `.createNamespace` | `scarab-plugins` / `true` | the shared plugin namespace; must enforce Pod Security `restricted`. A namespace of its own: the render fails on the release namespace, `scarab.namespace`, `default` or `kube-*` |
| `plugins.brokerPort` | `8090` | the broker listener and its ClusterIP Service port |
| `plugins.probeImage` | `""` = `busybox:1.36.1@sha256:73aa…` | the isolation probe's and the per-Pod self-check's image; digest-pin an override |
| `ingress.enabled` | `false` | expose the HTTP API |

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
