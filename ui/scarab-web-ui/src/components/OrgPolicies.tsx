// Settings › Policies: the org's CEL policies.
//
// Each row shows a Policy's checkpoint, whether it is in audit mode, what it
// applies to, how many Runs or Steps it denied (or, in audit mode, would have
// denied) in the last 7 days, and its version. An audit-mode Policy can be
// enforced from its row; Manage opens its page, with its editor, its history
// and its decisions. At the right of the header, Paste YAML reads a
// `kind: policy` file into the editor and New policy opens the editor on its
// own page.
//
// Rules with a form of their own are edited next to what they govern, not
// here: library and plugin policies on Libraries and Plugins, an
// Environment's rules on its page, a token's scope when it is minted.
import { For, Show, createResource, createSignal } from "solid-js";
import { A, useNavigate } from "@solidjs/router";
import {
  checkPolicy,
  listOrgPolicies,
  policyDecisionCounts,
  updatePolicy,
  type PolicyCheckResult,
  type PolicyRecord,
} from "../api/client";
import {
  appliesToEnvironments,
  appliesToProjects,
  isCelPolicy,
  type PolicyBody,
  type StoredPolicyLike,
} from "../policies";
import { diagnosticText, policyEditHref, windowCountLabel, type WindowCounts } from "../policy-editor";
import { absTime, relTime } from "../fmt";
import Icon from "./Icon";
import Modal, { ModalMessage } from "./Modal";
import { Field, Textarea } from "./Field";
import Pill from "./Pill";

/** The window the list's counts cover. */
const WINDOW_DAYS = 7;

// The schema leaves a Policy's body opaque; this reads it as the shape
// `policies.ts` names.
const asStored = (ps: readonly PolicyRecord[]): StoredPolicyLike[] =>
  ps.map((p) => ({ ...p, policy: p.policy as unknown as PolicyBody }));

export default function OrgPolicies(props: { org: string }) {
  const [epoch, setEpoch] = createSignal(0);
  const [policies] = createResource(
    () => ({ org: props.org, epoch: epoch() }),
    (k) => listOrgPolicies(k.org),
  );
  const [counts] = createResource(
    () => ({ org: props.org, epoch: epoch() }),
    (k) => policyDecisionCounts(k.org, Date.now() - WINDOW_DAYS * 86_400_000).catch(() => null),
  );
  const [enforcing, setEnforcing] = createSignal<StoredPolicyLike | null>(null);
  const [pasting, setPasting] = createSignal(false);
  const [notice, setNotice] = createSignal<string | null>(null);

  const stored = () => asStored(policies() ?? []);
  const cel = () => stored().filter(isCelPolicy).sort((a, b) => a.name.localeCompare(b.name));
  const countOf = (id: string): WindowCounts | undefined => counts()?.policies.find((c) => c.policy_id === id);
  // `null` is the server answering "no policy store here".
  const available = () => !policies.error && policies.latest !== null && policies.latest !== undefined;

  return (
    <>
      <Show when={notice()}>
        <p class="notice-ok">{notice()}</p>
      </Show>
      <div class="panel">
        <div class="panel-h">
          <span>
            CEL policies · {props.org} <span class="tok-count">{cel().length}</span>
          </span>
          <Show when={available()}>
            <span class="oenv-actions">
              <button class="btn btn-ghost btn-sm" type="button" onClick={() => setPasting(true)}>
                Paste YAML
              </button>
              <A class="btn btn-primary btn-sm" href={policyEditHref(props.org)}>
                <Icon icon="plus" size={14} /> New policy
              </A>
            </span>
          </Show>
        </div>
        <div class="secrets-body">
          <Show when={!policies.error} fallback={<p class="error">Could not load the org's policies.</p>}>
            <Show when={!policies.loading || policies.latest} fallback={<p class="empty">loading…</p>}>
              <Show
                when={policies.latest !== null}
                fallback={<p class="empty">Policies are not configured on this server.</p>}
              >
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
                            <A class="tok-name mono oenv-link" href={policyEditHref(props.org, p.id)}>
                              {p.name}
                            </A>
                            <Pill tone="copper">{p.policy.checkpoint}</Pill>
                            <Show when={p.policy.mode === "audit"}>
                              <Pill tone="copper" title="Its denies are recorded, not enforced">
                                audit
                              </Pill>
                            </Show>
                            <span class="oenv-actions">
                              <Show when={p.policy.mode === "audit"}>
                                <button class="btn btn-ghost btn-sm" type="button" onClick={() => setEnforcing(p)}>
                                  Enforce
                                </button>
                              </Show>
                              <A class="btn btn-ghost btn-sm" href={policyEditHref(props.org, p.id)}>
                                Manage
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
                <p class="subtle">
                  <small>
                    <Icon icon="shield-check" size={12} /> Rules with a form of their own are set
                    next to what they govern: library and plugin policies under Libraries and
                    Plugins, an environment's rules on its page, a token's scope when it is minted.
                  </small>
                </p>
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
