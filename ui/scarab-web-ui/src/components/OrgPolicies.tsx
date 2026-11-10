// Settings → Policies: every rule the org holds, in one place.
//
// - CEL Policies: each with its checkpoint, what it applies to, its mode, and
//   how many Runs or Steps it denied (or, in audit mode, would have denied)
//   in the last 7 days. New policy opens the editor; Paste YAML reads a
//   `kind: policy` file into it. An audit-mode Policy can be enforced from
//   the list; Edit and History open the editor's tabs.
// - The newest decisions of the log.
// - Typed settings: the rules that have a form next to their feature. Each
//   Environment's rules, the library and plugin policies, and API token scopes,
//   each with a link to where it is edited.
import { For, Show, createResource, createSignal } from "solid-js";
import { A, useNavigate } from "@solidjs/router";
import {
  checkPolicy,
  listOrgEnvironments,
  listOrgPolicies,
  listOrgPolicyDecisions,
  policyDecisionCounts,
  updatePolicy,
  type PolicyCheckResult,
  type PolicyDecisionPage,
  type PolicyRecord,
} from "../api/client";
import { isPermissive, rulesSummary } from "../environment-overlays";
import { environmentHref, nameBeside, shownName } from "../org-environments";
import {
  appliesToEnvironments,
  appliesToProjects,
  decisionVerdict,
  isCelPolicy,
  typedSettingRows,
  type DecisionRecordLike,
  type PolicyBody,
  type StoredPolicyLike,
} from "../policies";
import {
  diagnosticText,
  policyEditHref,
  policyHistoryHref,
  windowCountLabel,
  type WindowCounts,
} from "../policy-editor";
import { orgHref } from "../settings-nav";
import { absTime, relTime } from "../fmt";
import Icon from "./Icon";
import Modal, { ModalMessage } from "./Modal";
import { Field, Textarea } from "./Field";

/** How many of the newest decisions the page lists. */
const RECENT = 10;
/** The window the list's counts cover. */
const WINDOW_DAYS = 7;

// The schema leaves a Policy's body and a decision opaque; these read only
// those fields as the shapes `policies.ts` names.
const asStored = (ps: readonly PolicyRecord[]): StoredPolicyLike[] =>
  ps.map((p) => ({ ...p, policy: p.policy as unknown as PolicyBody }));
const asDecisions = (ds: PolicyDecisionPage["decisions"]): DecisionRecordLike[] =>
  ds.map((d) => ({ ...d, decision: d.decision as unknown as DecisionRecordLike["decision"] }));

export default function OrgPolicies(props: { org: string }) {
  const [epoch, setEpoch] = createSignal(0);
  const [policies] = createResource(
    () => ({ org: props.org, epoch: epoch() }),
    (k) => listOrgPolicies(k.org),
  );
  const [decisions] = createResource(() => props.org, (org) => listOrgPolicyDecisions(org, RECENT));
  const [counts] = createResource(
    () => ({ org: props.org, epoch: epoch() }),
    (k) => policyDecisionCounts(k.org, Date.now() - WINDOW_DAYS * 86_400_000).catch(() => null),
  );
  const [environments] = createResource(() => props.org, (org) => listOrgEnvironments(org).catch(() => null));
  const [enforcing, setEnforcing] = createSignal<StoredPolicyLike | null>(null);
  const [pasting, setPasting] = createSignal(false);
  const [notice, setNotice] = createSignal<string | null>(null);

  const stored = () => asStored(policies() ?? []);
  const cel = () => stored().filter(isCelPolicy).sort((a, b) => a.name.localeCompare(b.name));
  // An errored resource throws when read, so the error is checked first.
  const log = () => (decisions.error ? [] : asDecisions(decisions()?.decisions ?? []));
  const countOf = (id: string): WindowCounts | undefined => counts()?.policies.find((c) => c.policy_id === id);
  const settings = () => typedSettingRows(props.org, stored());

  return (
    <>
      <Show when={notice()}>
        <p class="notice-ok">{notice()}</p>
      </Show>
      <div class="panel">
        <div class="panel-h">
          <span>CEL policies</span>
          <Show when={cel().length > 0}>
            <span class="tok-count">{cel().length}</span>
          </Show>
        </div>
        <div class="secrets-body">
          <Show when={!policies.error} fallback={<p class="error">Could not load the org's policies.</p>}>
            <Show when={!policies.loading || policies.latest} fallback={<p class="empty">loading…</p>}>
              <Show
                when={policies.latest !== null}
                fallback={<p class="empty">Policies are not configured on this server.</p>}
              >
                <div class="pol-list-actions">
                  <A class="btn btn-primary btn-sm" href={policyEditHref(props.org)}>
                    <Icon icon="plus" size={14} /> New policy
                  </A>
                  <button class="btn btn-ghost btn-sm" type="button" onClick={() => setPasting(true)}>
                    Paste YAML
                  </button>
                </div>
                <Show
                  when={cel().length > 0}
                  fallback={
                    <p class="empty">
                      No CEL policies in {props.org}. A policy is a rule over what a Run, a Step launch
                      or a gate approval carries, such as "no deploys from a fork".
                    </p>
                  }
                >
                  <ul class="tok-list" aria-label="CEL policies">
                    <For each={cel()}>
                      {(p) => (
                        <li class="tok">
                          <div class="tok-head">
                            <A class="tok-name mono" href={policyEditHref(props.org, p.id)}>
                              {p.name}
                            </A>
                            <span class="tok-role mono">{p.policy.checkpoint}</span>
                            <Show when={p.policy.mode === "audit"}>
                              <span class="tok-role" title="Its denies are recorded, not enforced">
                                audit
                              </span>
                            </Show>
                            <span class="oenv-actions">
                              <Show when={p.policy.mode === "audit"}>
                                <button class="btn btn-ghost btn-sm" type="button" onClick={() => setEnforcing(p)}>
                                  Enforce
                                </button>
                              </Show>
                              <A class="btn btn-ghost btn-sm" href={policyEditHref(props.org, p.id)}>
                                Edit
                              </A>
                              <A class="btn btn-ghost btn-sm" href={policyHistoryHref(props.org, p.id)}>
                                History
                              </A>
                            </span>
                          </div>
                          <dl class="lib-facts pol-facts">
                            <dt>Applies to projects</dt>
                            <dd class="m">{appliesToProjects(p.policy)}</dd>
                            <dt>Environments</dt>
                            <dd class="m">{appliesToEnvironments(p.policy)}</dd>
                            <dt>Decisions</dt>
                            <dd>
                              <Show when={counts()} fallback="no counts">
                                {windowCountLabel(countOf(p.id), p.policy.mode, WINDOW_DAYS)}
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

      <Show when={enforcing()}>
        {(p) => (
          <EnforceDialog
            org={props.org}
            policy={p()}
            counts={countOf(p().id)}
            onClose={() => setEnforcing(null)}
            onDone={() => {
              // Read before the dialog closes: the accessor is gone after.
              const name = p().name;
              setEnforcing(null);
              setNotice(`${name} is enforced.`);
              setEpoch((n) => n + 1);
            }}
          />
        )}
      </Show>
      <Show when={pasting()}>
        <PasteDialog org={props.org} onClose={() => setPasting(false)} />
      </Show>

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
                      <A class="btn btn-ghost btn-sm oenv-actions" href={environmentHref(props.org, env.name)}>
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

          <h3 class="pol-h">Library and plugin policies</h3>
          <Show
            when={settings().length > 0}
            fallback={<p class="empty">No library or plugin policies: every installed library and plugin may be used.</p>}
          >
            <ul class="tok-list" aria-label="Library and plugin policies">
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

/** Enforce an audit-mode Policy. Moving to enforce tightens, so it needs no
 *  passkey. */
function EnforceDialog(props: {
  org: string;
  policy: StoredPolicyLike;
  counts: WindowCounts | undefined;
  onClose: () => void;
  onDone: () => void;
}) {
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const go = async () => {
    setBusy(true);
    setError(null);
    try {
      await updatePolicy(
        { org: props.org },
        props.policy.id,
        { ...props.policy.policy, mode: "enforce" },
        props.policy.version,
        "enforce",
      );
      props.onDone();
    } catch (e) {
      setError(e instanceof Error ? e.message : "The server refused it.");
      setBusy(false);
    }
  };
  return (
    <Modal
      size="narrow"
      eyebrow={`${props.org} · policies`}
      title={`Enforce ${props.policy.name}?`}
      purpose="From now on, what it matches and a rule holds for is refused."
      busy={busy()}
      onClose={props.onClose}
      footer={
        <>
          <Show when={error()}>
            <ModalMessage>{error()}</ModalMessage>
          </Show>
          <button class="btn btn-ghost" type="button" onClick={props.onClose} disabled={busy()}>
            Cancel
          </button>
          <button class="btn btn-primary" type="button" onClick={go} disabled={busy()}>
            {busy() ? "Enforcing…" : "Enforce"}
          </button>
        </>
      }
    >
      <p>
        In the last {WINDOW_DAYS} days it would have denied {props.counts?.would_deny ?? 0} time
        {(props.counts?.would_deny ?? 0) === 1 ? "" : "s"}. Moving it back to audit later asks for a
        passkey.
      </p>
    </Modal>
  );
}

/** Paste a `kind: policy` YAML file: the server parses and checks it, and a
 *  Policy opens in the editor, unsaved. */
function PasteDialog(props: { org: string; onClose: () => void }) {
  const nav = useNavigate();
  const [yaml, setYaml] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [result, setResult] = createSignal<PolicyCheckResult | null>(null);
  const go = async () => {
    setBusy(true);
    setError(null);
    try {
      const r = await checkPolicy(props.org, { yaml: yaml() });
      if (r.policy) {
        nav(policyEditHref(props.org), { state: { draft: r.policy } });
        return;
      }
      setResult(r);
    } catch (e) {
      setError(e instanceof Error ? e.message : "The server refused it.");
    } finally {
      setBusy(false);
    }
  };
  return (
    <Modal
      eyebrow={`${props.org} · policies`}
      title="Paste YAML"
      purpose="A kind: policy file opens in the editor. Nothing is saved until you save it there."
      busy={busy()}
      onClose={props.onClose}
      footer={
        <>
          <Show when={error()}>
            <ModalMessage>{error()}</ModalMessage>
          </Show>
          <Show when={result() && !result()!.policy}>
            <ModalMessage>{result()!.diagnostics.map(diagnosticText).join(" · ")}</ModalMessage>
          </Show>
          <button class="btn btn-ghost" type="button" onClick={props.onClose} disabled={busy()}>
            Cancel
          </button>
          <button class="btn btn-primary" type="button" onClick={go} disabled={busy() || yaml().trim() === ""}>
            {busy() ? "Reading…" : "Open in editor"}
          </button>
        </>
      }
    >
      <Field label="Policy YAML">
        <Textarea
          mono
          rows={12}
          autofocus
          value={yaml()}
          onValue={setYaml}
          placeholder={"kind: policy\nname: no-fork-deploys\ncheckpoint: run.create\nrules:\n  - when: trigger.fork && pipeline.deploy\n    effect: deny\n    message: A fork may not deploy."}
        />
      </Field>
    </Modal>
  );
}
