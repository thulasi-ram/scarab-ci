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
      get_step_logs:
        match: { method: POST, json: { "/method": "tools/call", "/params/name": "get_step_logs" } }
        may: [run.read, step.logs.read]
      trigger_run:
        match: { method: POST, json: { "/method": "tools/call", "/params/name": "trigger_run" } }
        may: [repo.read, repo.dispatch]
        writes: 1
      client_reply:
        match: { method: POST, json: { "/result": exists } }
        may: []
      stream:
        match: { method: GET }
        may: [run.read]
```

- A request that matches no operation is rejected by core and never reaches the Plugin. The
  protocol's own messages are listed with `may: []`, so the manifest names everything the
  Plugin can be asked.
- A body that is a list is checked element by element; one unmatched element rejects the
  request. The request's handle gets the matched operations combined and their writes summed.
  Every element comes from one caller.
- Only a POST operation may write.
- Core knows nothing of MCP. The MCP knowledge is in the manifest, where an admin reviews it.

### 3. The Plugin acts as the caller through a handle, never a credential

With each forwarded request core gives the Plugin a **DelegatedCallerHandle**: an opaque value
with which the Plugin calls core as that caller. Its authority is the intersection of the
caller's token, what the caller consented to, and the `may` of the matched operations.

- It lives while that request's connection is open, never longer than core's longest request.
- Core checks it again every 60 seconds: the token, the owner's role, the kill switch.
- A handle for a long-lived stream (a GET) never writes.
- Its writes are capped at the operations' `writes`.

### 4. PluginApi: the public API on the broker listener

A Plugin calls core only through **PluginApi**: the public API's own routes and handlers,
served on the CredentialBroker's listener. It is the public contract on another listener,
never a private API.

- A call proves the installation (its projected ServiceAccount token) and carries a handle.
- The handle becomes a narrowed caller, as a replayed StepUpConfirmationRequest already does,
  and each handler runs its usual `authorize_scoped`.
- The route's ApiOperation is checked against the handle before any handler runs.
- Plugin egress stays broker-only (ADR-0072 §9 unchanged on egress).

Limits, as Helm values:

| Limit | Default |
|---|---|
| Inbound requests per caller per installation | 60/min, 4 concurrent |
| Inbound requests per installation | 600/min, 16 concurrent |
| Core calls per handle | 50 |
| Delegated calls | take a broker permit and the broker's database pool |

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
host loosens and needs a step-up. Core fetches only over HTTPS, follows no redirect, caps the
size and refuses an address that is not public. The consent page names the host it fetched
from, not a name the client chose.

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
- One org admin approves each new digest with a step-up.

There is no second admin, unlike `trusted_dom`. The operator is the second key, held by another
role, and the handles bound what a `trusted_service` Plugin can do (no approvals, consented
operations only, no step-up). `trusted_dom` code can do anything a viewer's session can,
including changing what a step-up shows.

### 9. Fork code and Fork Run approval

**Fork code** is a commit Scarab treats as an outsider's:

| Ref | Treated as |
|---|---|
| a branch or tag of the repository | the repository's code |
| `refs/pull/<N>/head` | fork code when the forge says PR N comes from another repository; no answer refuses the dispatch |
| a raw commit SHA | fork code, always: a forge serves any fork's commits through the base repository |

A Run of fork code gets the fork pull request limits whatever started it, a dispatch by a
person included. This fixes the dispatch hole for people too, and needs no agent-only rule.

**Fork Run approval** is an org setting: `off`, `agent_started` (default: started through an
`oauth` PrincipalToken) or `all`. When it applies, core adds a **Run-start approval gate**: a
`manual` Gate in front of every root Step, `origin` = the setting, released by any member with
write on the repository in a browser, failing the Run (class `gate`) after 72 hours. Loosening
the setting needs a step-up. It is a setting and not a Policy effect: ADR-0075's rule that no
Policy adds a Gate stands.

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
`reject_gate`) come later, one write each. No approve tool, ever.

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
| 1 | Fork code detection at dispatch | 0.8.x patch |
| 2 | ApiOperations for reads, PrincipalToken, DelegationTrail, the approval rule | 0.9.0 |
| 3 | OAuth authorization server, OAuthClientMetadataHosts, consent, Connected apps | 0.9.0 |
| 4 | Fork Run approval | 0.9.0 |
| 5 | `trusted_service` and the operator allowlist | 0.9.0 |
| 6 | PluginApi with handles, limits and the journal | 0.9.0 |
| 7 | The endpoint extension point, request rules, proxy, first ingress policy, lifecycle | 0.9.0 |
| 8 | `scarab-plugin-mcp` with the tools above, e2e and docs | 0.9.0 |
| later | write tools | 0.9.x |

Steps 1 to 4 are useful without MCP.

## Consequences

- An installation with no public URL cannot be used from claude.ai. The docs say so.
- A single-admin org can run the MCP Plugin: no second approval is needed.
- An org that wants the MCP Plugin needs its operator to add the publisher's repository first.
- A PR whose head ref is in another repository costs one forge call per dispatch.
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
