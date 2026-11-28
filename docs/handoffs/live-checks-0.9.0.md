# Handoff: live checks for 0.8.4 and 0.9.0

Written 2026-11-28. Both releases are published. Everything below was built,
reviewed and tested in CI, and none of it has been tried against the real
outside system it talks to. Each check says what to do, what counts as a pass,
what to record, and which ticket it closes. Tickets are in git-bug
(`git bug bug show <id>`).

## Where things stand

- `main` is at `v0.9.0` (`62a9ebd6`). `v0.8.4` is the rollback target for 0.9.0.
- The public demo (`ssh demo-scarab`, `https://demo-scarab.ahiravan.dev`) runs
  0.9.0, Helm revision 25, upgraded from 0.7.6 on 2026-11-28. Plugins, the OIDC
  issuer and the OAuth server are off there. After the upgrade the database is
  at migration 114, `/readyz` answers 200, and one Run of this repository
  (checkout, then `cargo check`) went green with its step Pods in
  `scarab-steps`.
- tools-common is on 0.8.3.
- CI's kind e2e suite passed on the 0.9.0 tree. It does not turn on the OAuth
  server, install an endpoint plugin, or configure an OIDC issuer.

What changed, in one line each:

| Release | Contents |
|---|---|
| 0.8.4 | `REGISTRY_AUTH` reads Environment scopes; four fork and ref security fixes; patches that let 0.8.4 read what 0.9.0 writes |
| 0.9.0 | OIDC tokens for step Pods outside an Environment; Fork Run approval; ApiOperations and token operation lists; the plugin endpoint, OAuth server and MCP plugin; raw logs default to the latest attempt |

User docs for the new features: `usage/oidc-tokens`, `usage/aws-roles`,
`usage/fork-run-approval`, `usage/connected-apps`, `usage/mcp-plugin` under
`ui/scarab-docs-ui/src/content/docs/`. Upgrade and rollback notes:
`deploy/helm/scarab/README.md`, the "Upgrading from …" sections.

## Before upgrading tools-common

Read "Upgrading from 0.8.3" and "Upgrading from 0.8.4" in the chart README
first. The points that apply to a deployment with an OIDC issuer and plugins:

1. **Rollback target.** A 0.8.3 server cannot load a Run or a plugin manifest
   that 0.9.0 wrote; 0.8.4 can. Rolling back from 0.9.0 means: stop the server,
   run `crates/scarab-db-postgres/rollback/0114_revert.sql`, then `0113`,
   `0112`, `0111`, and start 0.8.4. A plain `helm rollback` leaves a server
   that refuses to start on the migrated database. Deploying 0.8.4 before 0.9.0
   is optional; what matters is that the rollback goes to 0.8.4 and not 0.8.3.
2. **OIDC tokens for Environment Runs change.** `kid` is now the key's RFC 7638
   thumbprint (was `scarab-key-1`), `event` is the real trigger kind (was always
   `deploy`), an empty `sha` is left out, and a token lives for the step's
   timeout up to one hour (was 15 minutes). The `sub` of an Environment Run
   does not change. Check every AWS trust policy condition against this before
   the upgrade; a verifier that caches the key set refetches by `kid`.
3. **Gate approval by API token** now waits for the token owner's passkey
   confirmation. Find any script that approves gates with a token.
4. **Plugin Deployments** were renamed in 0.8.3 (already live on tools-common).
   Nothing new in 0.9.0 unless a plugin declares an endpoint.
5. An Environment named `none` gets no OIDC token after the upgrade, and the
   name can no longer be created.

Record: the Helm revision before and after, and the server log lines for
migrations 0111 to 0114.

## Check 1: AWS trusts a step by its Scarab OIDC token (ticket 20a9cec)

Goal: a step with no Environment assumes an AWS role through
`AssumeRoleWithWebIdentity`, replacing a shared IRSA role.

Follow `usage/aws-roles`. The decisions it encodes:

- The issuer is published as static files (the discovery document and the key
  set on S3 or CloudFront). There is no Ingress to the server.
- The audience is one server-wide setting, `SCARAB_OIDC_AUDIENCE`; set it to
  `sts.amazonaws.com`.
- Core has no AWS syntax. The step sets two ordinary `env:` values:
  `AWS_ROLE_ARN` and `AWS_WEB_IDENTITY_TOKEN_FILE=/scarab/oidc/token`.

Steps:

1. Set the issuer URL to the public URL of the bucket or distribution. Fetch
   `/.well-known/openid-configuration` and `/.well-known/jwks.json` from the
   server and publish both there. Confirm `issuer` and `jwks_uri` in the
   published document name the public URL.
2. Create the IAM OIDC provider for that URL with audience `sts.amazonaws.com`.
3. Create one role whose trust policy matches `sub` exactly, for example
   `scarab:org/<owner>/repo/<repo>/ref/refs/heads/main`.
4. Run a pipeline on `main` with a step that calls `aws sts get-caller-identity`.

Pass:

- The step prints the role's ARN.
- The same pipeline on another branch is refused by AWS.
- A fork pull request's step has no token file at all.
- A step in an Environment-bound Run still gets `…/env/<name>/ref/…`, and
  `…/env/none/…` before its approval gate.
- A step that first calls AWS after 20 minutes still succeeds (the token now
  outlives 15 minutes).

Record: the decoded claims of one token (never the token), the trust policy,
and the CloudTrail `AssumeRoleWithWebIdentity` event. Then remove the
ServiceAccount annotation of the IRSA role it replaces, last.

Trust policy rule to keep to: a `*` is safe only after `/ref/`, and `…/ref/*`
includes same-repository pull request heads. Never wildcard `repo/` or `env/`.

Things that would be a bug: a token for fork code; a branch named `ref/x` or
`env/x` getting a token (it must get none, with a server warning); a key
rotation that breaks role assumption when the next key was published first.

## Check 2: claude.ai connects to the MCP plugin (ticket 9130efd)

Goal: a person connects claude.ai to Scarab and an agent reads a Run and
triggers one, with the agent never holding the person's token.

Needs a deployment with plugins on and CPU to spare; the demo box has neither.
Follow `usage/mcp-plugin` and `usage/connected-apps`.

Steps:

1. Set `plugins.enabled`, add the plugin image's repository to
   `plugins.trustedPublishers`, set `oauthServer.enabled` and
   `secrets.oauthRefreshKey` (32 bytes or more; the server refuses to start
   without it).
2. Install the `scarab-plugin-mcp` image as a plugin. It is admitted only as
   `trusted_service` from a listed publisher.
3. In claude.ai, add a custom connector whose URL is the installation's
   endpoint URL. The first attempt is refused: the server log and the refused
   request show the host of the client id URL claude.ai presents. Allow that
   host for the org (`PUT /v1/orgs/{org}/oauth/client-metadata-hosts/{host}`,
   org admin, passkey). **Record that host**; the docs do not name it because
   Anthropic does not publish it.
4. Connect again, sign in, and confirm consent with a passkey.

Pass:

- Consent shows the client's host, the installation, and exactly the operations
  granted. It appears in the org's security events.
- The agent lists Runs, reads a step's logs and results (try a step inlined
  from a library, such as `image/push`), and triggers a Run.
- The Run's events name the token and the plugin installation that acted.
- Log text returned to the agent arrives escaped inside its untrusted-data
  block.
- Account settings, Connected apps, lists the connection; disconnecting ends
  it within one request.
- Killing the plugin answers 503 and the connection survives; enabling it
  again works without a new consent.

Known gaps to watch for: claude.ai falls back to Dynamic Client Registration
unless the metadata advertises `client_id_metadata_document_supported: true`
and `"none"` in `token_endpoint_auth_methods_supported` (it does; if claude.ai
still tries DCR, record the request). There is no settings screen for the
allowed hosts yet. The plugin has no reject tool although `gate.reject` is
delegable (see ticket ffedf7e). An agent can never approve a gate, mint a
token, or change a setting; if any of those work, stop and report.

## Check 3: Fork Run approval with a real agent

Depends on check 2. The org setting has three values: `off`, `agent_started`
(the default) and `all`.

Pass:

- With the default, a Run the agent triggers on a fork pull request's ref
  waits at a gate named for Fork Run approval, and nothing of the fork runs
  first: no step, no shared service.
- The same ref dispatched by a person in the browser does not wait.
- Releasing the gate does not unlock Environment secrets, the registry
  credential or an OIDC token.
- A Rerun asks again; a Retry does not.
- `all` makes every fork-code Run wait; changing `all` to a looser value asks
  for a passkey.

## Check 4: Forgejo AGit pull requests (ticket 14a694f, open)

Goal: confirm what Forgejo sends for an AGit-flow pull request and that such a
pull request is treated as fork code.

Use the Forgejo in the kind stack (`just stack up`).

1. As a user with read access only, push to `refs/for/main/topic` on a
   connected repository.
2. Capture the webhook body Scarab receives and the answer of
   `GET /api/v1/repos/{owner}/{repo}/pulls/{n}`.

Record: `head.ref`, `head.repo`, any `flow` field, and the head SHA. Add the
captured body as a fixture under the Forgejo adapter's tests.

Pass: the Run is fork code (locked out of secrets, `SCARAB_FORK_PR=true`).
0.8.4 already requires that a same-repository pull request's head branch
exists in the base repository at the head commit, so this should pass
whatever the payload says. If `head.ref` turns out to name a real branch at
the same commit, that is the repository's own code and is correct.

## Check 5: demo deploy preflight (ticket 0221524, open)

The fixed preflight ran once on the Oracle box during the 0.9.0 upgrade, with
freshly pulled images, and passed. The bug was a race when the image is
already cached, so two more runs are owed:

1. Run `bash deploy/demo-oracle/deploy.sh 0.9.0` again on the box (images now
   cached). It must pass the role check for the server and the fetcher, and
   leave no preflight Pod behind.
2. Run it once with a tag that does not exist. It must stop before the
   database dump and name the image pull, not print a kubectl error.

Each run dumps the database to R2 first; that is expected.

## Check 6: dispatch refs on real forges

0.8.4 records a bare ref on manual and API dispatch with the kind that
matched. The lookups were written from the API documentation.

On GitHub and on Forgejo, dispatch with: a branch name; a tag name; a name
that is both (expect 400); `HEAD` (expect the default branch); a short SHA
(expect the full SHA recorded, treated as fork code); `refs/heads/x` where `x`
does not exist (expect 400). Record the ref each Run shows.

## Not checks, but open

- `ffedf7e`: show waiting gates and recently rejected gates in the UI and the
  API; ship the MCP plugin's reject tool with it.
- `6460a7d`: the Stateright model experiment for the gate and rerun machine.
- Rerunning a step that `when:` excluded runs it on an ordinary Run; only
  fork-gated Runs refuse it. Not filed.
- The demo runs with plugins and the OAuth server off; turning on the MCP
  plugin there needs CPU the two-core box does not have.
