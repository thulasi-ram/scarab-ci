// Account → Security (ADR-0073): your passkeys, adding and removing one, and
// your own security events.
//
// - A FIRST passkey needs a browser login from the last 5 minutes; an older
//   login gets `403 fresh_login_required`, shown here as "Log in again" (a
//   login that lands straight back on enrollment, `?enroll=1`).
// - A LATER passkey, and removing any, is always-ask: the step-up middleware
//   opens the confirm dialog in the middle of the call, so this panel only
//   ever sees the answer.
// - `?enroll=1` (where every "enroll a passkey" link points) starts enrolling
//   on arrival.
import { For, Show, createResource, createSignal, onMount } from "solid-js";
import { useLocation } from "@solidjs/router";
import {
  FreshLoginRequiredError,
  beginPasskeyRegistration,
  finishPasskeyRegistration,
  getMySecurity,
  listMySecurityEvents,
  removePasskey,
  type Passkey,
  type SecurityEvent,
} from "../api/client";
import { absTime, relTime } from "../fmt";
import { passkeyBadges, passkeyLabel, shouldAutoEnroll, wantsEnroll } from "../security";
import { createPasskey, webauthnAvailable, webauthnErrorText } from "../webauthn";
import Icon from "./Icon";
import SecurityEventTable from "./SecurityEventTable";

export default function SecuritySettings() {
  const location = useLocation();
  const [sec, { refetch }] = createResource(getMySecurity);
  const [label, setLabel] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [notice, setNotice] = createSignal<string | null>(null);
  const [freshLogin, setFreshLogin] = createSignal<{ message: string; url: string } | null>(null);

  // Own events, paged by `next_before`.
  const [events, setEvents] = createSignal<SecurityEvent[]>([]);
  const [nextBefore, setNextBefore] = createSignal<number | null>(null);
  const [eventsError, setEventsError] = createSignal(false);
  const loadEvents = async (before?: number) => {
    try {
      const page = await listMySecurityEvents(before);
      setEvents((prev) => (before === undefined ? page.events : [...prev, ...page.events]));
      setNextBefore(page.next_before ?? null);
      setEventsError(false);
    } catch {
      setEventsError(true);
    }
  };

  const enforced = () => sec()?.step_up === "enforce";

  const enroll = async (auto = false) => {
    setBusy(true);
    setError(null);
    setNotice(null);
    setFreshLogin(null);
    try {
      if (!webauthnAvailable()) {
        throw new Error("This browser cannot use passkeys here (passkeys need https, or localhost).");
      }
      const options = await beginPasskeyRegistration(passkeyLabel(label()));
      const credential = await createPasskey(options.public_key);
      const added = await finishPasskeyRegistration(options.id, credential);
      setNotice(`Passkey "${added.label}" added.`);
      setLabel("");
      await refetch();
      await loadEvents();
    } catch (e) {
      if (e instanceof FreshLoginRequiredError) {
        setFreshLogin({ message: e.message, url: e.loginUrl });
      } else if (auto && (e as { name?: string } | null)?.name === "NotAllowedError") {
        // The arrival-time start had no click behind it; some browsers refuse
        // that. Not an error the person made — just ask for the click.
        setNotice("Press “Add passkey” to enroll.");
      } else {
        setError(webauthnErrorText(e));
      }
    } finally {
      setBusy(false);
    }
  };

  const remove = async (p: Passkey) => {
    if (
      !window.confirm(
        `Remove the passkey "${p.label}"?\n\nIt can no longer confirm anything. If it is your last one, every crown-jewel change is refused until you enroll another.`,
      )
    )
      return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await removePasskey(p.id);
      setNotice(`Passkey "${p.label}" removed.`);
      await refetch();
      await loadEvents();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not remove the passkey.");
    } finally {
      setBusy(false);
    }
  };

  onMount(() => {
    void loadEvents();
    // `?enroll=1`: start as soon as we know step-up is on. A browser that
    // demands a click for the passkey prompt refuses this one quietly, and
    // the Add button is right there.
    if (wantsEnroll(location.search)) {
      void (async () => {
        const s = await getMySecurity().catch(() => null);
        if (shouldAutoEnroll(location.search, s)) await enroll(true);
      })();
    }
  });

  return (
    <div class="panel" id="security">
      <div class="panel-h">
        <span>Security · passkeys</span>
        <Show when={sec()}>
          <span class="tok-count">{sec()!.passkeys.length}</span>
        </Show>
      </div>
      <div class="secrets-body">
        <Show when={sec.error}>
          <p class="error">Could not load your security settings.</p>
        </Show>
        <Show when={notice()}>
          <p class="notice-ok">{notice()}</p>
        </Show>
        <Show when={error()}>
          <p class="error">{error()}</p>
        </Show>
        <Show when={freshLogin()}>
          {(f) => (
            <div class="tok-warn">
              <Icon icon="alert-triangle" size={14} />
              <span>
                {f().message}{" "}
                {/* A server route (the OAuth start), so a plain anchor. */}
                <a class="btn btn-primary btn-sm" href={f().url}>
                  Log in again
                </a>
              </span>
            </div>
          )}
        </Show>

        <Show when={sec()}>
          {(s) => (
            <>
              <Show
                when={enforced()}
                fallback={
                  <p class="empty">
                    Step-up is off on this server, so no change needs a passkey and none can be
                    enrolled here.
                  </p>
                }
              >
                <Show when={s().window_expires_at}>
                  {(w) => (
                    <p class="subtle">
                      <small>
                        <Icon icon="clock" size={12} /> Step-up window open in this session until{" "}
                        {absTime(w())}.
                      </small>
                    </p>
                  )}
                </Show>
                <Show
                  when={s().passkeys.length > 0}
                  fallback={
                    <p class="empty">
                      No passkey yet. Every crown-jewel change (secrets, roles, tokens,
                      connections…) is refused until you enroll one.
                    </p>
                  }
                >
                  <ul class="tok-list">
                    <For each={s().passkeys}>
                      {(p) => (
                        <li class="tok" classList={{ "tok-dead": p.clone_suspected }}>
                          <div class="tok-head">
                            <Icon icon="key-round" size={15} />
                            <span class="tok-name">{p.label}</span>
                            <For each={passkeyBadges(p)}>
                              {(b) => (
                                <span
                                  class="tok-state"
                                  classList={{ "tok-revoked": b.tone === "danger" }}
                                >
                                  {b.text}
                                </span>
                              )}
                            </For>
                            <button
                              class="btn btn-danger btn-sm tok-revoke"
                              disabled={busy()}
                              onClick={() => remove(p)}
                            >
                              remove
                            </button>
                          </div>
                          <div class="tok-facts">
                            <span title={absTime(p.created_at)}>added {relTime(p.created_at)}</span>
                            <span>
                              {p.last_used_at != null ? (
                                <span title={absTime(p.last_used_at)}>
                                  last used {relTime(p.last_used_at)}
                                </span>
                              ) : (
                                "never used"
                              )}
                            </span>
                          </div>
                        </li>
                      )}
                    </For>
                  </ul>
                </Show>

                <div class="secrets-actions sec-add">
                  <input
                    class="input"
                    placeholder="Name it (MacBook Touch ID, YubiKey…)"
                    maxLength={64}
                    value={label()}
                    onInput={(e) => setLabel(e.currentTarget.value)}
                  />
                  <button class="btn btn-primary btn-sm" disabled={busy()} onClick={() => enroll()}>
                    <Icon icon="plus" size={14} /> {busy() ? "Working…" : "Add passkey"}
                  </button>
                </div>
                <p class="subtle">
                  <small>
                    <Icon icon="shield-check" size={12} /> A first passkey needs a login from the
                    last 5 minutes; adding another, or removing one, asks you to confirm with a
                    passkey you already hold.
                  </small>
                </p>
              </Show>
            </>
          )}
        </Show>

        <h3 class="sec-sub">Your security events</h3>
        <Show when={eventsError()}>
          <p class="error">Could not load your security events.</p>
        </Show>
        <SecurityEventTable events={events()} hideWho empty="Nothing recorded for you yet." />
        <Show when={nextBefore() !== null}>
          <div class="secrets-actions">
            <button class="btn btn-ghost btn-sm" onClick={() => loadEvents(nextBefore()!)}>
              Older events
            </button>
          </div>
        </Show>
      </div>
    </div>
  );
}
