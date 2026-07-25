# scarab Helm chart

Deploys **scarab-server** (one binary, selectable roles) onto Kubernetes,
with the RBAC it needs to launch one Pod per step in its namespace.

## Prerequisites

- Kubernetes 1.25+
- A Postgres database (the durable outbox / coordination bus) — without it the
  server runs API-only.
- An S3-compatible object store for logs/artifacts (AWS S3 or in-cluster MinIO)
  for production; a local dir fallback exists for dev.
- The image published by `.github/workflows/image.yml`
  (`ghcr.io/thulasi-ram/scarab-server`).

## Install

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

## Login (`scarab.oauth`, ADR-0049)

Scarab authenticates operators against a **forge-agnostic OAuth/OIDC provider**:
the endpoints are explicit values, so GitHub, Forgejo and any OIDC issuer are the
same code path. Without one the server has no authenticator and **refuses to
start** (ADR-0048); the only other bootable shape is `scarab.devInsecure`, where
every caller is a synthetic Owner.

The four endpoints/ids are chart values; the **client secret never is** — it comes
by reference from a Secret you manage out-of-band, exactly like the App PEM
below:

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
(bootstrap until scoped RBAC, ADR-0049 C2); everyone else authenticates as
**Viewer**. An empty list means nobody can administer the install — the chart says
so on install.

**All five or none.** The server refuses to boot on a partially configured
provider, so the chart refuses to render one: a missing endpoint, or a login
config with no reachable client secret, **fails `helm upgrade`** with the exact
value to set. If you already use `secrets.existingSecret`, you may supply
`SCARAB_OAUTH_CLIENT_SECRET` as a key of it and leave
`secrets.oauthClientSecret.name` empty.

**`devInsecure` is not a fallback.** Setting `scarab.devInsecure=true` *and*
`scarab.oauth.*` is a hard render error, not a precedence rule — dev-insecure
would silently turn every caller back into an Owner and neuter the login you just
configured. Pick one.

## The GitHub App credential (bootstrap-free)

In App mode (`scarab.githubAppId`) the credential a GitHub connection
authenticates with is the App's private-key PEM. It can be stored in Postgres at
the reserved `_forge` scope via `POST /v1/secrets` (ADR-0046), but that has a
bootstrap ordering problem for a cluster: the server must already be up to
accept the credential it needs, and the credential dies with the database. So the
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

Two other shapes reach the same place — `secrets.githubAppPem` (inline value,
rendered into the chart Secret as `SCARAB_GITHUB_APP_PEM`) and a
`SCARAB_GITHUB_APP_PEM` key in your own `secrets.existingSecret`. Inline wins
over the mounted file. Prefer the mounted file: no key material passes through
Helm values.

**This is the credential only.** Which installations and repos exist is separate
durable state, registered by the App's `installation` /
`installation_repositories` webhook deliveries (or `deploy/local-helm/reseed.sh`
for a local loop) — a fresh database still needs one delivery.

## Declarative connections (`scarab.connections`, ADR-0060 part D)

A forge connection can be **declared in config** instead of created through the
UI. Config-declared connections are provisioned at boot, are **authoritative**,
and are read-only in the UI ("managed by configuration"). This is the only way to
onboard a Forgejo host — and its repos as Projects — without any API call.

```sh
kubectl -n scarab create secret generic scarab-forgejo \
  --from-literal=FORGEJO_CI_TOKEN=<token>

helm upgrade --install scarab deploy/helm/scarab -n scarab \
  --set secrets.existingSecret=scarab-forgejo \
  --set-json 'scarab.connections=[{"id":"forgejo-main","kind":"forgejo","base_url":"https://git.example.com","credential":{"env":"FORGEJO_CI_TOKEN"},"repos":["acme/widgets"]}]'
```

The block is rendered verbatim into a mounted ConfigMap and
`SCARAB_CONNECTIONS_FILE` points at it, so the keys are the **server's** schema
(`base_url`, `secret_ref` — snake_case, not the chart's camelCase). An unknown key
fails the boot loudly rather than being ignored (ADR-0048).

**One owner, never two.** A connection is owned by the config *or* by the
database — declaring an id the database already owns (e.g. one a GitHub
`installation` delivery created) **refuses the boot** and says so, rather than
letting the two drift apart with no authority to break the tie. Removing an entry
*releases* ownership back to the UI; it never deletes the connection, because
Projects — and their Environments, secrets and RBAC — hang off its repo bindings.

**Credentials resolve by one path: env override, then Scarab's secret store.**
`credential: {env: VAR}` / `{file: PATH}` supply the material from the deployment
(missing or empty ⇒ boot failure); `credential: {secret_ref: HANDLE}` resolves the
`_forge`-scoped handle at use-time (unregistered ⇒ reported DEGRADED, since only
the running server can store it). `SCARAB_GITHUB_APP_PEM[_FILE]` above is the same
mechanism, applied kind-wide to GitHub App-mode connections.

## Key values

| Key | Default | Notes |
|-----|---------|-------|
| `image.repository` / `image.tag` | `ghcr.io/thulasi-ram/scarab-server` / appVersion | container image |
| `scarab.role` | `converged` | `api` / `scheduler` / `executor` / `webhook` / `converged` |
| `scarab.executor` | `k8s` | `k8s` (prod) or `local` (dev only) |
| `scarab.namespace` | release ns | namespace step Pods launch into (RBAC granted there) |
| `scarab.s3.*` | — | object store; set `bucket` to enable S3/MinIO |
| `scarab.oauth.*` | — | OAuth/OIDC login: `clientId`, `authorizeUrl`, `tokenUrl`, `userinfoUrl`, `scopes`, `owners` (above) |
| `scarab.devInsecure` | `false` | ⚠ dev/eval only — no auth; mutually exclusive with `scarab.oauth` |
| `secrets.*` / `secrets.existingSecret` | — | sensitive `SCARAB_*` env |
| `secrets.oauthClientSecret.name` / `.key` | — / `oauth-client-secret` | OAuth client secret by reference from your own Secret (above) |
| `secrets.githubAppPemSecret.name` / `.key` | — / `github-app.pem` | mount the App PEM from your own Secret (above) |
| `scarab.connections` | `[]` | declarative, config-owned forge connections (above) |
| `rbac.create` | `true` | Role/RoleBinding for Pod execution |
| `ingress.enabled` | `false` | expose the HTTP API |

See `values.yaml` for the full surface. Every value maps to a `SCARAB_*` env var
read by `crates/scarab-server/src/config.rs`. Webhook verification binds a secret
per forge endpoint: `SCARAB_GITHUB_WEBHOOK_SECRET` for `/webhooks/github`
and `SCARAB_FORGEJO_WEBHOOK_SECRET` for `/webhooks/forgejo`. Both render from
first-class values (`secrets.githubWebhookSecret` / `secrets.forgejoWebhookSecret`),
or supply them as keys of your own `secrets.existingSecret`.

## References

- [ADR-0005 — Tenancy & deployment; Kubernetes as the only backend](../../../docs/adr/0005-tenancy-and-k8s-only.md)
- [ADR-0016 — Code architecture: hexagonal + adapter crates + converged binary](../../../docs/adr/0016-code-architecture.md)
- [ADR-0046 — Forge auth is adapter-internal; GitHub + Forgejo adapters in v1](../../../docs/adr/0046-forge-auth-and-multi-adapter.md)
- [ADR-0048 — Fail-closed startup](../../../docs/adr/0048-fail-closed-startup.md)
- [ADR-0049 — Identity & access: forge-agnostic OAuth/OIDC login](../../../docs/adr/0049-identity-and-access.md)
