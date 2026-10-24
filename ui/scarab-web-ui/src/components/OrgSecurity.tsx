// Settings → Security (ADR-0073): the Org's step-up posture and its
// OrgSecurityEventLog.
//
// - The posture: whether step-up is enforced, and which admins hold no
//   passkey — every crown jewel is refused for them until they enroll one.
// - Recent refusals (the last 24 hours): who was asked to confirm, for what.
// - The event log, filterable by kind and paged by `next_before`.
// - An Owner's "Reset passkeys" for a member who lost theirs — always-ask, so
//   the step-up middleware has the Owner confirm it with their own passkey.
import { For, Show, createResource, createSignal } from "solid-js";
import {
  getOrgSecuritySummary,
  listBindings,
  listOrgSecurityEvents,
  resetMemberPasskeys,
  type SecurityEvent,
} from "../api/client";
import {
  EVENT_KINDS,
  canResetPasskeys,
  eventKindLabel,
  postureText,
  resetCandidates,
} from "../security";
import Icon from "./Icon";
import SecurityEventTable from "./SecurityEventTable";

export default function OrgSecurity(props: { org: string }) {
  const [summary, { refetch: refetchSummary }] = createResource(() => props.org, getOrgSecuritySummary);
  const [bindings] = createResource(() => props.org, listBindings);

  const [kind, setKind] = createSignal("");
  const [events, setEvents] = createSignal<SecurityEvent[]>([]);
  const [nextBefore, setNextBefore] = createSignal<number | null>(null);
  const [eventsError, setEventsError] = createSignal<string | null>(null);
  const [loading, setLoading] = createSignal(false);

  const loadEvents = async (before?: number) => {
    setLoading(true);
    try {
      const page = await listOrgSecurityEvents(props.org, { kind: kind(), before });
      setEvents((prev) => (before === undefined ? page.events : [...prev, ...page.events]));
      setNextBefore(page.next_before ?? null);
      setEventsError(null);
    } catch (e) {
      setEventsError(e instanceof Error ? e.message : "Could not load the event log.");
    } finally {
      setLoading(false);
    }
  };
  void loadEvents();

  const [resetMsg, setResetMsg] = createSignal<{ ok: boolean; text: string } | null>(null);
  const [resetting, setResetting] = createSignal<string | null>(null);
  const reset = async (subject: string) => {
    if (
      !window.confirm(
        `Reset every passkey ${subject} holds?\n\nThey can no longer confirm anything until they enroll a new passkey from a fresh login, and any open step-up window of theirs closes now. You will be asked to confirm this with your own passkey.`,
      )
    )
      return;
    setResetting(subject);
    setResetMsg(null);
    try {
      const r = await resetMemberPasskeys(props.org, subject);
      setResetMsg({
        ok: true,
        text: `Reset ${r.subject}: ${r.removed} passkey${r.removed === 1 ? "" : "s"} removed, ${r.windows_revoked} confirmation${r.windows_revoked === 1 ? "" : "s"} revoked.`,
      });
      await refetchSummary();
      await loadEvents();
    } catch (e) {
      setResetMsg({ ok: false, text: e instanceof Error ? e.message : "Could not reset." });
    } finally {
      setResetting(null);
    }
  };

  return (
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

              <h3 class="sec-sub">Recent refusals · last 24h</h3>
              <SecurityEventTable
                events={s().recent_refusals}
                empty="Nobody was refused for a missing step-up in the last 24 hours."
              />

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
                              onClick={() => reset(m.subject)}
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

        <h3 class="sec-sub">Event log</h3>
        <div class="secrets-actions sec-filter">
          <select
            class="input"
            value={kind()}
            onChange={(e) => {
              setKind(e.currentTarget.value);
              void loadEvents();
            }}
          >
            <option value="">All events</option>
            <For each={EVENT_KINDS}>
              {(k) => <option value={k}>{eventKindLabel(k)}</option>}
            </For>
          </select>
        </div>
        <Show when={eventsError()}>
          <p class="error">{eventsError()}</p>
        </Show>
        <SecurityEventTable
          events={events()}
          empty={kind() ? "No events of this kind." : "No security events yet."}
        />
        <Show when={nextBefore() !== null}>
          <div class="secrets-actions">
            <button
              class="btn btn-ghost btn-sm"
              disabled={loading()}
              onClick={() => loadEvents(nextBefore()!)}
            >
              Older events
            </button>
          </div>
        </Show>
      </div>
    </div>
  );
}
