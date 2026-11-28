// Organization settings → Security → Fork Run approval: which Runs of fork
// code (a pull request from a fork, or a commit no branch or tag of the
// repository names) wait at an approval gate before any of their steps or
// services start. Members read it; an org admin changes it from here. A
// change that holds fewer Runs asks for a passkey confirmation through the
// client's step-up retry; one that holds more does not.
import { createResource, createSignal, For, Show } from "solid-js";
import {
  getForkRunApproval,
  setForkRunApproval,
  type ForkRunApprovalChoice,
} from "../api/client";

const CHOICES: { id: ForkRunApprovalChoice; name: string; what: string }[] = [
  {
    id: "off",
    name: "Off",
    what: "No Run of fork code waits for approval.",
  },
  {
    id: "agent_started",
    name: "Runs an app started",
    what:
      "A Run of fork code waits when an app connected to Scarab, such as an AI agent, " +
      "started it. No app can connect to this server yet, so today no Run waits. The default.",
  },
  {
    id: "all",
    name: "Every Run of fork code",
    what: "Every Run of fork code waits, whoever or whatever started it.",
  },
];

/** Gates fewer Runs: off < agent_started < all. */
const RANK: Record<ForkRunApprovalChoice, number> = { off: 0, agent_started: 1, all: 2 };

export default function ForkRunApproval(props: { org: string }) {
  const [setting, { mutate }] = createResource(() => props.org, getForkRunApproval);
  const [busy, setBusy] = createSignal(false);
  const [notice, setNotice] = createSignal<string | null>(null);
  const [refusal, setRefusal] = createSignal<string | null>(null);
  // The radio inputs: the browser checks the clicked one before the server
  // answers, so a refused or cancelled change puts the saved value back.
  const inputs: HTMLInputElement[] = [];
  const showSaved = () => {
    const saved = setting()?.setting;
    for (const el of inputs) el.checked = el.value === saved;
  };

  const change = async (next: ForkRunApprovalChoice) => {
    setBusy(true);
    setNotice(null);
    setRefusal(null);
    try {
      mutate(await setForkRunApproval(props.org, next));
      setNotice("Saved. It applies to Runs created from now on.");
    } catch (e) {
      // Not an admin, a cancelled confirmation, another admin's change in
      // between: the server's message, and the saved value checked again.
      setRefusal(e instanceof Error ? e.message : "Could not save the setting.");
      showSaved();
    } finally {
      setBusy(false);
    }
  };

  return (
    <div class="panel">
      <div class="panel-h">
        <span>Fork Run approval</span>
      </div>
      <div class="secrets-body">
        <Show when={setting.error || (setting.state === "ready" && !setting())}>
          <p class="error">Could not load the org's Fork Run approval setting.</p>
        </Show>
        <Show when={notice()}>
          <p class="notice-ok">{notice()}</p>
        </Show>
        <Show when={refusal()}>
          <p class="error">{refusal()}</p>
        </Show>
        <Show when={setting()}>
          {(s) => (
            <div class="sec-block">
              <p class="subtle">
                A Run of code from a fork, or of a commit no branch or tag of the repository names,
                can wait at an approval gate before any of its steps or services start. Anyone with
                write on the repository approves it from the Run page; an API token's owner
                confirms each approval, and an app cannot approve. An approved Run still gets no
                Environment secrets, registry credentials or cloud roles. The gate fails the Run
                after 72 hours, and a rerun waits for approval again.
              </p>
              <div class="sec-pick" role="radiogroup" aria-label="Which Runs wait for approval">
                <For each={CHOICES}>
                  {(c) => (
                    <label classList={{ on: s().setting === c.id }}>
                      <input
                        ref={(el) => inputs.push(el)}
                        type="radio"
                        name="fork-run-approval"
                        value={c.id}
                        checked={s().setting === c.id}
                        disabled={busy() || !s().can_change}
                        onChange={() => void change(c.id)}
                      />
                      <span class="conn-forge">
                        <span class="tok-name">{c.name}</span>
                        <span class="subtle">{c.what}</span>
                      </span>
                    </label>
                  )}
                </For>
              </div>
              <p class="subtle">
                Runs created before a change keep or lack their gate.
                <Show when={!s().can_change}> Only an org admin, in a browser, can change this.</Show>
                <Show when={s().can_change && RANK[s().setting] > 0}>
                  {" "}
                  A change that holds fewer Runs asks you to confirm with your passkey.
                </Show>
              </p>
            </div>
          )}
        </Show>
      </div>
    </div>
  );
}
