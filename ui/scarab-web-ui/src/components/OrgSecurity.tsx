// Organization settings → Security: the Org's step-up settings, then its
// security event log.
//
// - Whether step-up is enforced, and which admins hold no passkey: every
//   crown jewel is refused for them until they enroll one.
// - An Owner's "Reset passkeys" for a member who lost theirs: always-ask, so
//   the step-up middleware has the Owner confirm it with their own passkey.
// - The event log (`EventLog.tsx`) with its filters and paging; its
//   "Refusals, last 24h" preset shows who was asked to confirm, for what.
import { For, Show, createResource, createSignal } from "solid-js";
import { getOrgSecuritySummary, listBindings, resetMemberPasskeys } from "../api/client";
import { canResetPasskeys, postureText, resetCandidates } from "../security";
import Icon from "./Icon";
import ConfirmDestructive from "./ConfirmDestructive";
import EventLog from "./EventLog";

export default function OrgSecurity(props: { org: string }) {
  const [summary, { refetch: refetchSummary }] = createResource(() => props.org, getOrgSecuritySummary);
  const [bindings] = createResource(() => props.org, listBindings);

  const [resetMsg, setResetMsg] = createSignal<{ ok: boolean; text: string } | null>(null);
  const [resetting, setResetting] = createSignal<string | null>(null);
  // The member whose passkeys the confirm dialog is about to reset. A refusal
  // (a cancelled step-up included) shows in the dialog.
  const [confirming, setConfirming] = createSignal<string | null>(null);
  const reset = async (subject: string) => {
    setResetting(subject);
    setResetMsg(null);
    try {
      const r = await resetMemberPasskeys(props.org, subject);
      setResetMsg({
        ok: true,
        text: `Reset ${r.subject}: ${r.removed} passkey${r.removed === 1 ? "" : "s"} removed, ${r.windows_revoked} confirmation${r.windows_revoked === 1 ? "" : "s"} revoked.`,
      });
      await refetchSummary();
    } finally {
      setResetting(null);
    }
  };

  return (
    <>
      <div class="panel">
        <div class="panel-h">
          <span>Security · step-up</span>
          <Show when={summary()?.pending_requests}>
            <span class="tok-count" title="confirmation requests waiting for a human">
              {summary()!.pending_requests} pending
            </span>
          </Show>
        </div>
        <div class="secrets-body">
          <Show when={summary.error || (summary.state === "ready" && !summary())}>
            <p class="error">Could not load the Org's security summary.</p>
          </Show>
          <Show when={summary()}>
            {(s) => (
              <>
                <p class={s().enforced && s().admins_without_passkey.length ? "tok-warn" : "subtle"}>
                  <Icon icon="shield-check" size={14} /> <span>{postureText(s())}</span>
                </p>

                <Show when={s().admins_without_passkey.length > 0}>
                  <h3 class="sec-sub">Admins without a passkey</h3>
                  <ul class="sec-chips">
                    <For each={s().admins_without_passkey}>
                      {(a) => <li class="mono">{a}</li>}
                    </For>
                  </ul>
                </Show>

                <Show when={canResetPasskeys(s())}>
                  <h3 class="sec-sub">Reset a member's passkeys</h3>
                  <Show when={resetMsg()}>
                    {(m) => <p class={m().ok ? "notice-ok" : "error"}>{m().text}</p>}
                  </Show>
                  <Show
                    when={(bindings() ?? []).length > 0}
                    fallback={<p class="empty">No members to show.</p>}
                  >
                    <ul class="tok-list">
                      <For each={resetCandidates(bindings() ?? [], s().admins_without_passkey)}>
                        {(m) => (
                          <li class="tok">
                            <div class="tok-head">
                              <Icon icon="user" size={15} />
                              <span class="tok-name">{m.name}</span>
                              <span class="tok-scope mono">{m.subject}</span>
                              <Show when={m.noPasskey}>
                                <span class="tok-state tok-expired">no passkey</span>
                              </Show>
                              <button
                                class="btn btn-danger btn-sm tok-revoke"
                                disabled={resetting() !== null}
                                onClick={() => setConfirming(m.subject)}
                              >
                                {resetting() === m.subject ? "resetting…" : "reset passkeys"}
                              </button>
                            </div>
                          </li>
                        )}
                      </For>
                    </ul>
                  </Show>
                  <p class="subtle">
                    <small>
                      For a member who lost their only passkey. A passkey is not Org-scoped, so
                      you can reset only members of Orgs you own.
                    </small>
                  </p>
                </Show>
              </>
            )}
          </Show>

          <Show when={confirming()}>
            {(subject) => (
              <ConfirmDestructive
                title={`Reset every passkey ${subject()} holds?`}
                purpose="They can no longer confirm anything until they enroll a new passkey from a fresh login."
                name={subject()}
                confirmLabel="Reset passkeys"
                onConfirm={() => reset(subject())}
                onClose={() => setConfirming(null)}
              >
                <p>Any open step-up window of theirs closes now.</p>
                <p>You will be asked to confirm this with your own passkey.</p>
              </ConfirmDestructive>
            )}
          </Show>
        </div>
      </div>
      <EventLog org={props.org} />
    </>
  );
}
