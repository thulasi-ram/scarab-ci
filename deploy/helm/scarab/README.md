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

## Key values

| Key | Default | Notes |
|-----|---------|-------|
| `image.repository` / `image.tag` | `ghcr.io/thulasi-ram/scarab-server` / appVersion | container image |
| `scarab.role` | `converged` | `api` / `scheduler` / `executor` / `webhook` / `converged` |
| `scarab.executor` | `k8s` | `k8s` (prod) or `local` (dev only) |
| `scarab.namespace` | release ns | namespace step Pods launch into (RBAC granted there) |
| `scarab.s3.*` | — | object store; set `bucket` to enable S3/MinIO |
| `secrets.*` / `secrets.existingSecret` | — | sensitive `SCARAB_*` env |
| `secrets.githubAppPemSecret.name` / `.key` | — / `github-app.pem` | mount the App PEM from your own Secret (above) |
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
