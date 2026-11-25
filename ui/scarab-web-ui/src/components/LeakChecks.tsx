// Organization settings → Security → Leak checks: how many times each person
// may run "Check a leaked value" (Org settings › Secrets) per rolling hour and
// per rolling day. Admins read the two limits; an Owner changes them from a
// dialog, and each change asks for its own passkey confirmation through the
// client's step-up retry. 0 in either limit turns the check off for the org.
import { createResource, createSignal, Show } from "solid-js";
import { getLeakCheckLimits, setLeakCheckLimits } from "../api/client";
import { leakChecksOff, limitsText, validateLimits, type LeakCheckLimits } from "../leak-checks";
import Icon from "./Icon";
import Modal, { ModalMessage } from "./Modal";
import { Field, Input } from "./Field";

export default function LeakChecks(props: { org: string; owner: boolean }) {
  const [limits, { mutate }] = createResource(() => props.org, getLeakCheckLimits);
  const [editing, setEditing] = createSignal(false);
  const [notice, setNotice] = createSignal<string | null>(null);

  return (
    <div class="panel">
      <div class="panel-h">
        <span>Leak checks</span>
        <Show when={props.owner && limits()}>
          <button
            class="btn btn-ghost btn-sm"
            type="button"
            onClick={() => {
              setNotice(null);
              setEditing(true);
            }}
          >
            Edit limits
          </button>
        </Show>
      </div>
      <div class="secrets-body">
        <Show when={limits.error || (limits.state === "ready" && !limits())}>
          <p class="error">Could not load the org's leak-check limits.</p>
        </Show>
        <Show when={notice()}>
          <p class="notice-ok">{notice()}</p>
        </Show>
        <Show when={limits()}>
          {(l) => (
            <div class="sec-block">
              <p class={leakChecksOff(l()) ? "tok-warn" : undefined}>
                <Icon icon="search" size={14} /> <span>{limitsText(l())}</span>
              </p>
              <p class="subtle">
                How many times each person can run Check a leaked value in Org settings › Secrets.
                0 in either limit turns the check off for the org.
                <Show when={!props.owner}> Only an org Owner can change the limits.</Show>
              </p>
            </div>
          )}
        </Show>
      </div>

      <Show when={editing() && limits()}>
        {(l) => (
          <EditLimits
            org={props.org}
            limits={l()}
            onClose={() => setEditing(false)}
            onSaved={(next) => {
              mutate(next);
              setEditing(false);
              setNotice(leakChecksOff(next) ? "Saved. Leak checks are off for the org." : "Saved.");
            }}
          />
        )}
      </Show>
    </div>
  );
}

function EditLimits(props: {
  org: string;
  limits: LeakCheckLimits;
  onClose: () => void;
  onSaved: (l: LeakCheckLimits) => void;
}) {
  const [hour, setHour] = createSignal(String(props.limits.per_hour));
  const [day, setDay] = createSignal(String(props.limits.per_day));
  const [busy, setBusy] = createSignal(false);
  const [refusal, setRefusal] = createSignal<string | null>(null);
  const checked = () => validateLimits(hour(), day(), props.limits);
  const problem = (box: "hour" | "day") => {
    const c = checked();
    return c.ok ? null : (c[box] ?? null);
  };

  const save = async () => {
    const c = checked();
    if (!c.ok) return;
    setBusy(true);
    setRefusal(null);
    try {
      props.onSaved(await setLeakCheckLimits(props.org, c.value));
    } catch (e) {
      // `out_of_bounds`, not an Owner, a cancelled confirmation: the server's message.
      setRefusal(e instanceof Error ? e.message : "Could not save the limits.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      eyebrow={`${props.org} · security`}
      icon="search"
      title="Leak-check limits"
      purpose="How many checks each person can run in this org."
      size="narrow"
      busy={busy()}
      onClose={props.onClose}
      footer={
        <>
          <Show when={refusal()}>
            <ModalMessage>{refusal()}</ModalMessage>
          </Show>
          <button class="btn btn-ghost" type="button" onClick={props.onClose} disabled={busy()}>
            Cancel
          </button>
          <button
            class="btn btn-primary"
            type="button"
            disabled={busy() || !checked().ok}
            onClick={() => void save()}
          >
            {busy() ? "Saving…" : "Save"}
          </button>
        </>
      }
    >
      <Field label="Checks per hour" help={`0 to ${props.limits.max_per_hour}.`} error={problem("hour")} required>
        <Input mono inputmode="numeric" suffix="per person" value={hour()} onValue={setHour} disabled={busy()} />
      </Field>
      <Field label="Checks per day" help={`0 to ${props.limits.max_per_day}.`} error={problem("day")} required>
        <Input mono inputmode="numeric" suffix="per person" value={day()} onValue={setDay} disabled={busy()} />
      </Field>
      <div class="dlg-text">
        <p>0 in either limit turns the check off for the org.</p>
        <p>You will be asked to confirm this change with your passkey. It is recorded in the org's security events with the old and new limits.</p>
      </div>
    </Modal>
  );
}
