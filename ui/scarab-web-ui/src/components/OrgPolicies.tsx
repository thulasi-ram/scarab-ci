// Settings → Policies: every rule the org holds, in one place, read-only.
//
// - CEL Policies: each with its checkpoint, mode, the projects and
//   environments it applies to, and how often it matched, denied and (in
//   audit mode) would have denied in the newest decisions of the log.
// - The newest decisions themselves.
// - Typed settings: the rules that have a form next to their feature. Each
//   Environment's rules, the library and plugin lists, and API token scopes,
//   each with a link to where it is edited.
//
// Nothing is edited here: CEL Policies are written with the policy API, and
// each typed setting is edited where its feature is.
import { For, Show, createResource } from "solid-js";
import { A } from "@solidjs/router";
import {
  listOrgEnvironments,
  listOrgPolicies,
  listOrgPolicyDecisions,
  type PolicyDecisionPage,
  type PolicyRecord,
} from "../api/client";
import { isPermissive, rulesSummary } from "../environment-overlays";
import { environmentEditHref, nameBeside, shownName } from "../org-environments";
import {
  appliesToEnvironments,
  appliesToProjects,
  countsOf,
  decisionVerdict,
  isCelPolicy,
  policyCounts,
  typedSettingRows,
  type DecisionRecordLike,
  type PolicyBody,
  type StoredPolicyLike,
} from "../policies";
import { orgHref } from "../settings-nav";
import { absTime, relTime } from "../fmt";
import Icon from "./Icon";

/** How many of the newest decisions the page lists. */
const RECENT = 10;

// The schema leaves a Policy's body and a decision opaque; these read only
// those fields as the shapes `policies.ts` names.
const asStored = (ps: readonly PolicyRecord[]): StoredPolicyLike[] =>
  ps.map((p) => ({ ...p, policy: p.policy as unknown as PolicyBody }));
const asDecisions = (ds: PolicyDecisionPage["decisions"]): DecisionRecordLike[] =>
  ds.map((d) => ({ ...d, decision: d.decision as unknown as DecisionRecordLike["decision"] }));

export default function OrgPolicies(props: { org: string }) {
  const [policies] = createResource(() => props.org, listOrgPolicies);
  const [decisions] = createResource(() => props.org, (org) => listOrgPolicyDecisions(org));
  const [environments] = createResource(() => props.org, (org) => listOrgEnvironments(org).catch(() => null));

  const stored = () => asStored(policies() ?? []);
  const cel = () => stored().filter(isCelPolicy).sort((a, b) => a.name.localeCompare(b.name));
  // An errored resource throws when read, so the error is checked first.
  const log = () => (decisions.error ? [] : asDecisions(decisions()?.decisions ?? []));
  const counts = () => policyCounts(log());
  const settings = () => typedSettingRows(props.org, stored());

  return (
    <>
      <div class="panel">
        <div class="panel-h">
          <span>CEL policies</span>
          <Show when={cel().length > 0}>
            <span class="tok-count">{cel().length}</span>
          </Show>
        </div>
        <div class="secrets-body">
          <Show when={!policies.error} fallback={<p class="error">Could not load the org's policies.</p>}>
            <Show when={!policies.loading || policies()} fallback={<p class="empty">loading…</p>}>
              <Show
                when={policies() !== null}
                fallback={<p class="empty">Policies are not configured on this server.</p>}
              >
                <Show
                  when={cel().length > 0}
                  fallback={
                    <p class="empty">
                      No CEL policies in {props.org}. Add one with the policy API:{" "}
                      <span class="mono">POST /v1/orgs/{props.org}/policies</span>.
                    </p>
                  }
                >
                  <p class="subtle">
                    <small>
                      Counts are over the newest {log().length} decision{log().length === 1 ? "" : "s"} in
                      the log.
                    </small>
                  </p>
                  <ul class="tok-list" aria-label="CEL policies">
                    <For each={cel()}>
                      {(p) => (
                        <li class="tok">
                          <div class="tok-head">
                            <span class="tok-name mono">{p.name}</span>
                            <span class="tok-role mono">{p.policy.checkpoint}</span>
                            <Show when={p.policy.mode === "audit"}>
                              <span class="tok-role" title="Its denies are recorded, not enforced">
                                audit
                              </span>
                            </Show>
                          </div>
                          <dl class="lib-facts pol-facts">
                            <dt>Applies to projects</dt>
                            <dd class="m">{appliesToProjects(p.policy)}</dd>
                            <dt>Environments</dt>
                            <dd class="m">{appliesToEnvironments(p.policy)}</dd>
                            <dt>Decisions</dt>
                            <dd>
                              {countsOf(counts(), p.id).matched} matched · {countsOf(counts(), p.id).denied} denied
                              <Show when={p.policy.mode === "audit"}>
                                {" "}· {countsOf(counts(), p.id).wouldDeny} would have denied
                              </Show>
                            </dd>
                            <dt>Version</dt>
                            <dd>
                              {p.version} ·{" "}
                              <span title={absTime(p.updated_at)}>changed {relTime(p.updated_at)}</span>
                            </dd>
                          </dl>
                        </li>
                      )}
                    </For>
                  </ul>
                </Show>
              </Show>
            </Show>
          </Show>
        </div>
      </div>

      <div class="panel">
        <div class="panel-h">
          <span>Recent decisions</span>
        </div>
        <div class="secrets-body">
          <Show
            when={log().length > 0}
            fallback={
              decisions.error ? (
                <p class="error">Could not load the decision log.</p>
              ) : (
                <p class="empty">{decisions.loading ? "loading…" : "No decisions recorded yet."}</p>
              )
            }
          >
            <ul class="tok-list" aria-label="Recent decisions">
              <For each={log().slice(0, RECENT)}>
                {(d) => (
                  <li class="tok">
                    <div class="tok-head">
                      <span class={`tok-role ${decisionVerdict(d) === "denied" ? "pol-denied" : ""}`}>
                        {decisionVerdict(d)}
                      </span>
                      <span class="mono">{d.decision.checkpoint}</span>
                      <Show when={d.project}>
                        <span class="mono">{d.project}</span>
                      </Show>
                      <Show when={d.project && d.run_id}>
                        <A href={`/${encodeURIComponent(props.org)}/${encodeURIComponent(d.project!)}/runs/${d.run_id}`}>
                          run
                        </A>
                      </Show>
                      <span class="subtle" title={absTime(d.at)}>
                        {relTime(d.at)}
                      </span>
                    </div>
                    <Show when={decisionVerdict(d) === "denied" && d.decision.outcome?.messages?.length}>
                      <div class="tok-facts">{d.decision.outcome!.messages!.join(" · ")}</div>
                    </Show>
                  </li>
                )}
              </For>
            </ul>
          </Show>
        </div>
      </div>

      <div class="panel">
        <div class="panel-h">
          <span>Typed settings</span>
        </div>
        <div class="secrets-body">
          <p class="subtle">
            <small>Rules with a form next to their feature. Read-only here: each links to where it is edited.</small>
          </p>

          <h3 class="pol-h">Environment rules</h3>
          <Show
            when={(environments() ?? []).length > 0}
            fallback={<p class="empty">{environments.loading ? "loading…" : "No environments."}</p>}
          >
            <ul class="tok-list" aria-label="Environment rules">
              <For each={environments()}>
                {(env) => (
                  <li class="tok">
                    <div class="tok-head">
                      <span class="tok-name">{shownName(env)}</span>
                      <Show when={nameBeside(env)}>{(n) => <span class="mono subtle">{n()}</span>}</Show>
                      <A class="btn btn-ghost btn-sm oenv-actions" href={environmentEditHref(props.org, env.name)}>
                        Edit
                      </A>
                    </div>
                    <div class="tok-facts mono">
                      <span>
                        {isPermissive(env.protection) ? "no baseline rule" : rulesSummary(env.protection)}
                      </span>
                      <Show when={env.protection.approvers.length > 0}>
                        <span>
                          {(env.protection.approval_gate?.missing_gate_mode ?? "add") === "refuse"
                            ? "refuses a run with no approval gate"
                            : "adds an approval gate when missing"}
                        </span>
                      </Show>
                    </div>
                  </li>
                )}
              </For>
            </ul>
          </Show>

          <h3 class="pol-h">Library and plugin lists</h3>
          <Show
            when={settings().length > 0}
            fallback={<p class="empty">No library or plugin lists: every installed library and plugin may be used.</p>}
          >
            <ul class="tok-list" aria-label="Library and plugin lists">
              <For each={settings()}>
                {(row) => (
                  <li class="tok">
                    <div class="tok-head">
                      <span class="tok-name mono">{row.name}</span>
                      <span class="tok-role">{row.label}</span>
                      <A class="btn btn-ghost btn-sm oenv-actions" href={row.href}>
                        {row.sectionLabel}
                      </A>
                    </div>
                    <dl class="lib-facts pol-facts">
                      <dt>Applies to projects</dt>
                      <dd class="m">{row.appliesTo}</dd>
                      <dt>List</dt>
                      <dd class="m">{row.values.join(", ") || "empty"}</dd>
                    </dl>
                  </li>
                )}
              </For>
            </ul>
          </Show>

          <h3 class="pol-h">API token scopes</h3>
          <p>
            Each token's scope, role ceiling and lifetime are chosen when it is minted, which needs
            a passkey confirmation. No org rule limits them yet: anyone with a role may mint a token
            up to that role, for up to 365 days. <A href={orgHref(props.org, "tokens")}>API tokens</A>
          </p>
        </div>
      </div>
    </>
  );
}
