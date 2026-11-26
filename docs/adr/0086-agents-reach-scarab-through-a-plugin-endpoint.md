# 0086. Agents reach Scarab through a Plugin endpoint

- **Status:** Proposed
- **Date:** 2026-11-27
- **Deciders:** thulasi.ram (architect)
- **Amends:** [0072](0072-plugins-a-governed-extension-point.md) (§1 a Plugin may read through
  the caller, §7 trust tiers, §9 the first ingress into the plugins namespace),
  [0076](0076-libraries-and-plugins-through-invoke.md) (§7 "core never calls a plugin"),
  [0073](0073-step-up-confirmation.md) (which PrincipalTokens may approve a Gate),
  [0049](0049-identity-and-access.md) (tokens gain an operation list, an audience and an OAuth kind)
- **Builds on:** [0075](0075-policy-engine.md) (unchanged: no Policy adds a Gate),
  [0078](0078-the-environment-adds-its-approval-gate.md) (a Gate core adds, with its origin),
  [0080](0080-principals-have-an-internal-id.md) (Principal ids)
- **Tickets:** git-bug 9130efd (this design), 5d2aace (loopback-only client registration, deferred)

## Context

People want AI agents to use Scarab: read Runs, Steps, logs and results, and start Runs. The
agent that matters first is claude.ai in a browser. It connects to remote MCP servers over
HTTPS from Anthropic's servers and signs in with OAuth, so a local `scarab mcp` over stdio
does not reach it.

Two earlier designs were reviewed and rejected:

- **The MCP server as a Plugin holding a delegated token.** One Plugin instance serves every
  caller in an org, so a token handed to it is usable for anything that caller can do for its
  whole life. ADR-0072 already rejected vending credentials to Plugins for this reason.
- **A Plugin behind a proxy that does not know which request a call serves.** The Plugin holds
  every connected caller's handle at once and can spend an admin's read handle on a write
  someone else asked for: its authority is the union of all callers'.

Building MCP into core was the cheaper design. It was not chosen because moving code out of
core later is much harder than moving a Plugin into core, and agent protocols change often
(MCP changed its registration and discovery rules between the 2025-06-18 and 2025-11-25
specs). This ADR makes MCP a Plugin and adds what core needs to host one.

Facts found in the code while deciding:

- Every GET route shares one step-up operation, `read` (`crates/scarab-server/src/step_up/ops.rs:697`),
  so nothing could limit a caller to "logs" or "status".
- `authorize_scoped` returns only a `Principal` (`crates/scarab-server/src/lib.rs` ~12966), so a
  write made through a token is recorded as if the owner made it in a browser.
- `gate.approve` is conditional (`ops.rs:1340`): any token that may write approves a plain Gate.
- Dispatch accepts `refs/pull/<N>/head` and raw SHAs (`canonicalize_ref`, `lib.rs:8336`) and
  reads the Pipeline at that commit (`lib.rs:8441`). Fork limits key on
  `Event::PullRequest { fork: true }` (`crates/scarab-forge/src/lib.rs:835`), so a dispatch at a
  fork's commit runs the fork's YAML with no fork limits.
- A Plugin's network reaches only the broker listener, which already serves two public API
  routes for `plugin` Gates (`crates/scarab-server/src/broker.rs:108-119`).

## Decision

### 1. Core calls a Plugin only where the Plugin registered

A **PluginExtensionPoint** is a place core defines where it calls whatever Plugin registered
there. The set is closed and grows only with a Scarab release; `UiExtensionPoint` is one kind.
Core has no code for any particular Plugin: it knows which installation is registered (for the
kill switch, trust tier and audit) and never branches on who it is.

A **PluginExtensionRegistration** is declared in the PluginManifest and approved with it.
Nothing registers at run time. An upgrade that adds one is wider.

No Run, Gate or engine path calls a Plugin. This replaces ADR-0076 §7's "core never calls a
plugin".

The first non-UI extension point is the **endpoint**: core forwards an authenticated outside
request to the Plugin and returns its answer. An endpoint registration claims a path under its
own installation:

```
https://<scarab>/v1/orgs/{org}/plugin-installations/{id}/endpoints/<path>
```

`{id}` is the installation's random id, so the URL survives upgrades and a reinstall gets a new
one. `/endpoints/` keeps Plugin paths clear of core's installation routes.

### 2. Every request matches a declared operation, or core rejects it

An endpoint registration lists **PluginEndpointOperations**. Each has a
**PluginEndpointRequestRule** (an HTTP method plus JSON-pointer checks on the body, `equals` a
value or `exists`), the ApiOperations it **may** use, and how many writes it may make:

```yaml
extensions:
  - point: endpoint
    path: mcp
    operations:
      initialize:
        match: { method: POST, json: { "/method": "initialize" } }
        may: []
      initialized:
        match: { method: POST, json: { "/method": "notifications/initialized" } }
        may: []
      tools_list:
        match: { method: POST, json: { "/method": "tools/list" } }
        may: []
      get_step_logs:
        match: { method: POST, json: { "/method": "tools/call", "/params/name": "get_step_logs" } }
        may: [run.read, step.logs.read]
      trigger_run:
        match: { method: POST, json: { "/method": "tools/call", "/params/name": "trigger_run" } }
        may: [repo.read, repo.dispatch]
        writes: 1
```

A protocol that streams from server to client, or asks the client questions, would also list a
`GET` operation and one matching replies (`"/result": exists`). The stateless MCP Plugin of §10
needs neither.

- A request that matches no operation is rejected by core and never reaches the Plugin. The
  protocol's own messages are listed with `may: []`, so the manifest names everything the
  Plugin can be asked.
- An element that matches more than one operation is rejected.
- A body that is a list is checked element by element; one unmatched element rejects the
  request. The request's handle gets the matched operations combined and their writes summed.
  Every element comes from one caller.
- Only a POST operation may write.
- Core knows nothing of MCP. The MCP knowledge is in the manifest, where an admin reviews it.

**What core accepts and forwards.** The endpoint takes only a Bearer `oauth` PrincipalToken
whose audience is that URL; never a session cookie. Core reads the whole body up to a size cap,
requires `Content-Type: application/json` with no `Content-Encoding`, rejects a body with
duplicate keys, and evaluates the rules on its own parse. It forwards **its own re-serialised
JSON**, so the Plugin and core never read different bytes. `equals` compares JSON strings
only. Core strips `Authorization` and `Cookie` before forwarding; the Plugin never sees the
client's token.

### 3. The Plugin acts as the caller through a handle, never a credential

With each forwarded request core gives the Plugin a **DelegatedCallerHandle**: an opaque value
with which the Plugin calls core as that caller. Its authority is the intersection of the
caller's token, what the caller consented to, and the `may` of the matched operations.

- It lives while that request's connection is open, and at most 10 minutes.
- Core checks it again every 60 seconds: the token, the owner's role, the kill switch.
- A handle for a long-lived stream (a GET) never writes.
- Its writes are capped at the operations' `writes`, its core calls at 50.

The Plugin's calls may reach any server replica, so a handle is a **Postgres row**: its hash,
the caller, the operations, writes left, calls left and an expiry. The replica holding the
inbound connection heartbeats the row and deletes it when the connection closes. Each call and
each write decrements its counter in one conditional `UPDATE`, which refuses at zero.

**Known limit.** A write handle is bound to the operation, not to its arguments. A Plugin can
spend Alice's `trigger_run` on a repository or ref Alice did not name, within what Alice may
do. Fork code (§9) and Fork Run approval bound what such a Run can reach.

### 4. PluginApi: the public API on the broker listener

A Plugin calls core only through **PluginApi**: the public API's own routes and handlers,
served on the CredentialBroker's listener. It is the public contract on another listener,
never a private API.

- A call proves the installation (its projected ServiceAccount token) and carries a handle.
- The handle becomes a narrowed caller, as a replayed StepUpConfirmationRequest already does,
  and each handler runs its usual `authorize_scoped`.
- The route's ApiOperation is checked against the handle before any handler runs.
- Plugin egress stays broker-only (ADR-0072 §9 unchanged on egress).

**PluginApi serves an allowlist, not the whole router.** A route is served only if a handle's
`may` can name its ApiOperation. These can never appear in `may`, and an install or upgrade
whose manifest names one is refused:

- crown-jewel, conditional, browser-only and not-by-token operations;
- the debug shell (attach and debug Pod);
- every route that does not authenticate a Principal: `/v1/auth/*`, the OAuth endpoints, an
  external Gate's `/release`, the results POST, webhooks.

A handle never raises a StepUpConfirmationRequest. What would need one is refused, so injected
text cannot send its owner confirmation links through a Plugin.

Limits, as Helm values:

| Limit | Default |
|---|---|
| Inbound requests per caller per installation | 60/min, 4 concurrent |
| Inbound requests per installation | 600/min, 16 concurrent |
| Core calls per handle | 50 |
| Delegated calls | take a broker permit, and run on a separate bounded database pool, not the main one |

Every delegated call, reads included, is journaled in the broker journal with caller,
operation, target and result. The broker already refuses to forward what it cannot journal.

### 5. One ApiOperation per route, reads included

The single route table gives every route its own **ApiOperation**; the shared `read` splits by
what a read exposes (Run and Step status, logs, workspace files and artifacts, settings). The
same names are used by step-up, a token's operation list, OAuth consent and a
PluginEndpointOperation's `may`. The consent page and install review group them for display
only.

### 6. PrincipalToken: one record for issued and OAuth tokens

`ApiToken` becomes **PrincipalToken**, with three new fields:

- `operations`: the ApiOperations it may use (none means today's behaviour);
- `audience`: the one URL it may be used at (RFC 8707);
- `oauth_client`: the client it was granted to.

Two kinds:

- **`issued`**: today's `scarab_pat_` token.
- **`oauth`**: granted through the consent page. An access secret (`scarab_oat_`, 1 hour) and a
  refresh secret (`scarab_ort_`) that rotates on each use; a reused refresh secret revokes the
  grant. The grant ends 30 days after its last use and at most 90 days after consent. Its
  operations never widen: changing them means consenting again.

The routes (`/v1/orgs/{org}/tokens`), the `scarab_pat_` prefix and the `api_tokens` table keep
their names. The UI lists `issued` tokens as "API tokens" and `oauth` ones as "Connected apps".

**Approval.** A PrincipalToken never approves a `manual` Gate itself. An `oauth` one is refused,
with no StepUpConfirmationRequest raised, so text injected into a log cannot flood its owner
with confirmations. An `issued` one raises a StepUpConfirmationRequest for every Gate, plain
ones included. Either may reject. External (`/release`) and `plugin` Gates are unchanged.

**DelegationTrail.** Every write records, beside its Principal, how it arrived: the
PrincipalToken (id and kind), its OAuth client, and the Plugin installation when it came
through PluginApi. The Run page shows "Rerun by alice via claude.ai, through plugin:mcp".

### 7. Scarab is the OAuth authorization server

Core serves OAuth 2.1 authorization code with PKCE, authorization server metadata, protected
resource metadata for each endpoint (RFC 9728, with the `WWW-Authenticate` challenge core
emits), and tokens bound to the endpoint URL.

Clients identify themselves only by **Client ID Metadata Documents**. An org admin lists the
**OAuthClientMetadataHosts** Scarab may fetch them from (for example `claude.ai`). Adding a
host loosens and needs a step-up, and the setting warns against hosts that serve content their
users upload: every path on an allowed host is trusted. The consent page names the host it
fetched from, not a name the client chose.

The fetch and the flow:

- HTTPS only, no redirect followed, a size cap. Core resolves the host, refuses any address
  that is not public (IPv4 and IPv6), and connects to the address it checked, so DNS cannot
  change between the check and the request.
- The document's `client_id` must equal the URL it was fetched from, and it must not declare a
  shared-secret auth method.
- `redirect_uri` must match one of the document's `redirect_uris` exactly.
- PKCE with `S256` only.
- The consent page is served with `frame-ancestors 'none'`, and its POST carries a CSRF token
  bound to the session.
- The token's role ceiling is the user's role in the org at consent; the owner's live role
  still caps it.
- Refresh rotation is one conditional `UPDATE` on the refresh secret's hash. A retry of the
  same refresh within 30 seconds gets the same successor, so a lost response does not revoke a
  legitimate grant; a later reuse revokes it.

The code assumes claude.ai sends a metadata-document `client_id` to a server that advertises
`client_id_metadata_document_supported` and has no registration endpoint. Step 3 starts by
checking that on dogfood; if it does not hold, this section comes back to the owner before
anything else is built.

There is no Dynamic Client Registration. An open registration endpoint lets anyone register a
client that looks like claude.ai with their own redirect. Local agents that register only
through DCR (opencode) cannot connect yet; git-bug 5d2aace holds a loopback-only design for
when one is needed.

**Consent.** The ceiling is the union of the endpoint operations' `may`. The org comes from the
URL. Defaults:

| Group | Default |
|---|---|
| Read Runs and Steps (status, graph, results) | ticked |
| Read logs | ticked, with a one-line warning: logs hold text outsiders wrote and, until ADR-0082 is built, may hold a secret's tail |
| Read workspace files and artifacts | unticked |
| Start Runs | unticked |
| Rerun, retry, cancel, reject | unticked |
| Approve a Gate | never offered |

### 8. The `trusted_service` tier

A Plugin with an endpoint registration is **`trusted_service`**, a new **PluginTrustTier**
beside `framed` and `trusted_dom`. It sees data for many callers, and core cannot stop it
repeating one caller's data to another: core does not read its responses, and a cache keyed
without the caller does this by accident. The guardrails therefore choose who is trusted:

- The **operator** names trusted publishers by image repository
  (`plugins.trustedPublishers`). The list is empty by default, Scarab's own repository
  included. A PluginPublisher signature replaces the repository once signatures are built.
  An entry is `registry/namespace/name`, compared whole after lowercasing the registry: no
  prefix and no glob, so `ghcr.io/acme/mcp` never matches `ghcr.io/acme-evil/mcp`. The
  installed reference must carry a digest.
- One org admin approves each new digest with a step-up.

There is no second admin, unlike `trusted_dom`. The operator is the second key, held by another
role, and the handles bound what a `trusted_service` Plugin can do (no approvals, consented
operations only, no step-up). `trusted_dom` code can do anything a viewer's session can,
including changing what a step-up shows.

### 9. Fork code and Fork Run approval

**Fork code** is a commit Scarab treats as an outsider's. Core classifies the **resolved
commit**, not the ref string the caller typed, because a forge resolves many spellings (a
short or uppercase SHA, `pull/5/head`, `refs/pull/5/merge`) to a commit:

| Resolved commit | Treated as |
|---|---|
| the forge says a `refs/heads/<name>` or `refs/tags/<name>` of the repository points at it | the repository's code |
| reached through `refs/pull/<N>/head`, and the forge says PR N's head is a branch of this repository | the repository's code |
| anything else: another repository's PR, any `refs/pull/N/merge`, a SHA no branch or tag names, an AGit pull request whose head is not a branch | fork code |

When the forge does not answer, the dispatch fails with the existing forge error and a reason
naming fork detection. It is never trusted by default.

Dispatch stamps `fork_code` on the Run's trigger. Every fork limit reads that fact, along with
the existing `pull_request` `fork` flag: secrets, the OIDC subject, the clone token, build
push, and the Rerun, Take and `step.admit` paths. A Run of fork code gets the fork pull request
limits whatever started it, a dispatch by a person included. This fixes the dispatch hole for
people too, and needs no agent-only rule.

**Fork Run approval** is an org setting: `off`, `agent_started` (default: started through an
`oauth` PrincipalToken) or `all`. When it applies, core adds a **Run-start approval gate**: a
`manual` Gate in front of every root Step, `origin` = the setting. Nothing in the Run starts
before it is released, Shared services included. Any member with write on the repository
releases it, under §6's rule: in a browser, or through an `issued` token's
StepUpConfirmationRequest. It fails the Run (class `gate`) after 72 hours. Loosening the
setting needs a step-up.

A Run-start approval gate is never an Environment approval gate. It does not count as the
upstream `manual` Gate ADR-0078 looks for, so an Environment with approvers still adds its own.

It is a setting and not a Policy effect: ADR-0075's rule that no Policy adds a Gate stands.

### 10. The MCP Plugin

`crates/scarab-plugin-mcp`, following the Argo CD Plugin: no `scarab-*` dependency outside its
tests, `manifest.json` beside it, image `scarab-plugin-mcp` built by `image.yml` at the
workspace version. It uses `rmcp` at a pinned version, in stateless Streamable HTTP (no
`Mcp-Session-Id`), so a restart or upgrade drops no session.

First tools:

| Tool | Notes |
|---|---|
| `list_repos`, `list_runs` | `list_runs` filters by branch, status and PR; 20 by default, 100 at most |
| `get_run` | status, trigger, step graph, attempts, failure reasons |
| `get_step_logs` | the last 32 KiB by default, 256 KiB at most; optional `attempt` and a `pattern` filter within that window |
| `get_step_results`, `list_artifacts` | artifacts by name and size only |
| `describe_pipeline` | a Pipeline's typed inputs |
| `trigger_run` | `repo.dispatch`, one write; fork code as in §9 |

Text outsiders wrote (logs, results, PR titles, commit messages) is returned inside a block
marked as untrusted data. Write tools (`rerun_step`, `retry_step`, `cancel_run`,
`reject_gate`) come later, one write each. There is no approve tool, and `gate.approve` cannot
appear in a manifest's `may` (§4).

### 11. Lifecycle

| Event | Endpoint | Handles | `oauth` grants |
|---|---|---|---|
| kill switch on | 503 | refused within 60 s | kept |
| upgrade to a new digest | answers on the new Pod | die with their connections | kept |
| upgrade that removes an operation | its requests rejected | — | kept |
| uninstall | 404 | refused | revoked, every grant with this audience |
| owner leaves or loses the role | — | refused at the next check | disarmed through the owner's role |

### 12. Build order

| # | Change | Release |
|---|---|---|
| 1 | Fork code detection at dispatch, `fork_code` on the Run | 0.8.x patch |
| 2 | ApiOperations for reads, PrincipalToken, DelegationTrail, the approval rule | 0.9.0 |
| 3 | OAuth authorization server, OAuthClientMetadataHosts, consent, Connected apps (starts with the claude.ai check in §7) | 0.9.0 |
| 4 | Fork Run approval | 0.9.0 |
| 5 | `trusted_service` and the operator allowlist | 0.9.0 |
| 6 | PluginApi with handles, limits and the journal | 0.9.0 |
| 7 | The endpoint extension point, request rules, proxy, first ingress policy, lifecycle | 0.9.0 |
| 8 | `scarab-plugin-mcp` with the tools above, e2e and docs | 0.9.0 |
| later | write tools | 0.9.x |

Steps 1, 2 and 4 are useful without MCP. Step 3 is built against step 7's endpoint: the
audience, the protected resource metadata and the consent ceiling all come from an endpoint
registration, so its tests use a test endpoint until step 7 lands.

## Consequences

- An installation with no public URL cannot be used from claude.ai. The docs say so.
- A single-admin org can run the MCP Plugin: no second approval is needed.
- An org that wants the MCP Plugin needs its operator to add the publisher's repository first.
- Every dispatch asks the forge which branches, tags or pull request name the resolved commit:
  one or two forge calls more than today.
- Tests that approve Gates with a token change: they confirm the request or use a session.
- MCP spec changes land in the Plugin and ship in a Scarab patch release; each org moves to the
  new digest when it chooses. Changes to OAuth client registration still land in core.
- claude.ai itself cannot be tested automatically. The acceptance check is a manual one on
  dogfood (add the connector, consent, read a Run, start one), recorded in the PR.

## Alternatives considered

- **MCP in core (`/v1/mcp`).** Less new code and no union-of-callers risk. Rejected because
  moving it into a Plugin later would be a rewrite, and protocol churn would land in core.
- **A local `scarab mcp` over stdio.** Needs no OAuth, but claude.ai in a browser cannot use it.
- **The Plugin holds a delegated token.** See Context: vended credentials.
- **The Plugin says which operation it is serving.** Core cannot check the claim, so the
  Plugin's authority becomes the union of every connected caller's.
- **Plugins register at run time.** An admin could not see where a Plugin will be called before
  it runs.
- **Coarse OAuth scopes (`runs:read`, `logs:read`) beside the route table.** A second vocabulary
  that a new route can miss.
- **Dynamic Client Registration, open or loopback-only.** Not built now (git-bug 5d2aace).
- **Dual org-admin approval for `trusted_service`.** One admin can already cause the same harm
  by other means; the operator allowlist is the second key.
- **Fork approval as a Policy effect at `run.create`.** Would reverse ADR-0075's rule that no
  Policy adds a Gate. A setting covers the need.
- **Refusing PR refs and SHAs for agent tokens.** Fork code detection protects people and agents
  with one rule and lets agents run CI on a pull request.
