// Organization settings → Security: the Org's step-up settings, then its
// security event log.
//
// - Whether step-up is enforced, and which admins hold no passkey: every
//   crown jewel is refused for them until they enroll one.
// - An Owner's Reset passkeys, top right, for a member who lost theirs: a
//   dialog to pick the member, then the typed confirm. It is always-ask, so
//   the step-up middleware has the Owner confirm it with their own passkey.
//   An admin without a passkey in the list opens the same dialog with that
//   admin picked.
// - The event log (`EventLog.tsx`) with its filters and paging; its
//   "Refusals, last 24h" preset shows who was asked to confirm, for what.
import { For, Show, createResource, createSignal } from "solid-js";
import { A } from "@solidjs/router";
import { getOrgSecuritySummary, listBindings, resetMemberPasskeys } from "../api/client";
import { canResetPasskeys, postureText, resetCandidates, resetResultText } from "../security";
import { accountHref } from "../settings-nav";
import Icon from "./Icon";
import Modal from "./Modal";
import ConfirmDestructive from "./ConfirmDestructive";
import EventLog from "./EventLog";

export default function OrgSecurity(props: { org: string }) {
  const [summary, { refetch: refetchSummary }] = createResource(() => props.org, getOrgSecuritySummary);
  const [bindings] = createResource(() => props.org, listBindings);

  const [notice, setNotice] = createSignal<string | null>(null);
  // The reset dialog: closed (null), picking a member, or confirming one.
  const [step, setStep] = createSignal<"pick" | "confirm" | null>(null);
  const [member, setMember] = createSignal("");
  const open = (subject = "") => {
    setNotice(null);
    setMember(subject);
    setStep("pick");
  };
  const owner = () => canResetPasskeys(summary());
  const candidates = () => resetCandidates(bindings() ?? [], summary()?.admins_without_passkey ?? []);

  // A refusal (a cancelled step-up included) shows in the confirm dialog.
  const reset = async () => {
    const r = await resetMemberPasskeys(props.org, member());
    setStep(null);
    setNotice(resetResultText(r));
    await refetchSummary();
  };

  return (
    <>
      <div class="panel">
        <div class="panel-h">
          <span>
            Security · step-up{" "}
            <Show when={summary()?.pending_requests}>
              <span class="tok-count" title="confirmation requests waiting for a human">
                {summary()!.pending_requests} pending
              </span>
            </Show>
          </span>
          <Show when={owner()}>
            <button class="btn btn-danger btn-sm" onClick={() => open()}>
              <Icon icon="key-round" size={14} /> Reset passkeys
            </button>
          </Show>
        </div>
        <div class="secrets-body">
          <Show when={summary.error || (summary.state === "ready" && !summary())}>
            <p class="error">Could not load the Org's security summary.</p>
          </Show>
          <Show when={notice()}>
            <p class="notice-ok">{notice()}</p>
          </Show>
          <Show when={summary()}>
            {(s) => (
              <>
                <p class={s().enforced && s().admins_without_passkey.length ? "tok-warn" : "subtle"}>
                  <Icon icon="shield-check" size={14} /> <span>{postureText(s())}</span>
                </p>

                <Show when={s().admins_without_passkey.length > 0}>
                  <div class="sec-block">
                    <h3 class="sec-sub">Admins without a passkey</h3>
                    <ul class="sec-chips" aria-label="Admins without a passkey">
                      <For each={s().admins_without_passkey}>
                        {(a) => (
                          <li class="mono">
                            <Show when={owner()} fallback={a}>
                              <button
                                class="sec-chip-btn mono"
                                title={`Reset the passkeys ${a} holds`}
                                onClick={() => open(a)}
                              >
                                {a}
                              </button>
                            </Show>
                          </li>
                        )}
                      </For>
                    </ul>
                    <p class="subtle">
                      <small>
                        Every crown-jewel change is refused for them until they{" "}
                        <A href={`${accountHref("security")}?enroll=1`}>enroll a passkey</A> under
                        Account settings › Security.
                      </small>
                    </p>
                  </div>
                </Show>
              </>
            )}
          </Show>
        </div>
      </div>

      <Show when={step() === "pick"}>
        <PickMember
          org={props.org}
          members={candidates()}
          loading={bindings.loading}
          picked={member()}
          onPick={setMember}
          onNext={() => setStep("confirm")}
          onClose={() => setStep(null)}
        />
      </Show>
      <Show when={step() === "confirm" && member()}>
        {(subject) => (
          <ConfirmDestructive
            eyebrow={`${props.org} · security`}
            title={`Reset every passkey ${subject()} holds?`}
            purpose="They can no longer confirm anything until they enroll a new passkey from a fresh login."
            name={subject()}
            confirmLabel="Reset passkeys"
            onConfirm={reset}
            // Cancel goes back to the member list; after a reset the dialog is closed.
            onClose={() => setStep((s) => (s === "confirm" ? "pick" : s))}
          >
            <p>Any open step-up window of theirs closes now.</p>
            <p>You will be asked to confirm this with your own passkey.</p>
          </ConfirmDestructive>
        )}
      </Show>

      <EventLog org={props.org} />
    </>
  );
}

/** The reset dialog's first step: which member, with the admins who hold no
 *  passkey marked. */
function PickMember(props: {
  org: string;
  members: { subject: string; name: string; noPasskey: boolean }[];
  loading: boolean;
  picked: string;
  onPick: (subject: string) => void;
  onNext: () => void;
  onClose: () => void;
}) {
  return (
    <Modal
      eyebrow={`${props.org} · security`}
      title="Reset a member's passkeys"
      purpose="For a member who lost their only passkey."
      onClose={props.onClose}
      footer={
        <>
          <button class="btn btn-ghost" type="button" onClick={props.onClose}>
            Cancel
          </button>
          <button class="btn btn-danger" type="button" disabled={!props.picked} onClick={props.onNext}>
            Continue
          </button>
        </>
      }
    >
      <Show
        when={props.members.length > 0}
        fallback={<p class="empty">{props.loading ? "loading…" : "No members to show."}</p>}
      >
        <div class="sec-pick" role="radiogroup" aria-label="Member">
          <For each={props.members}>
            {(m) => (
              <label classList={{ on: props.picked === m.subject }}>
                <input
                  type="radio"
                  name="reset-member"
                  value={m.subject}
                  checked={props.picked === m.subject}
                  onChange={() => props.onPick(m.subject)}
                />
                <span class="tok-name">{m.name}</span>
                <span class="tok-scope mono">{m.subject}</span>
                <Show when={m.noPasskey}>
                  <span class="tok-state tok-expired">no passkey</span>
                </Show>
              </label>
            )}
          </For>
        </div>
      </Show>
      <p class="dlg-text">
        A passkey is not Org-scoped, so you can reset only members of Orgs you own.
      </p>
    </Modal>
  );
}
