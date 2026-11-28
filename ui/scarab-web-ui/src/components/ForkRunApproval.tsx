// Organization settings → Security → Fork Run approval: whether a Run of
// fork code (a pull request from a fork, or a commit no branch or tag of the
// repository names) waits at an approval gate before any of its steps or
// services start. Members read it; an org admin turns it on or off from here.
// Turning it off asks for a passkey confirmation through the client's step-up
// retry; turning it on does not.
import { createResource, createSignal, Show } from "solid-js";
import { getForkRunApproval, setForkRunApproval } from "../api/client";
import { Checkbox } from "./Field";

export default function ForkRunApproval(props: { org: string }) {
  const [setting, { mutate }] = createResource(() => props.org, getForkRunApproval);
  const [busy, setBusy] = createSignal(false);
  const [notice, setNotice] = createSignal<string | null>(null);
  const [refusal, setRefusal] = createSignal<string | null>(null);

  const change = async (enabled: boolean) => {
    setBusy(true);
    setNotice(null);
    setRefusal(null);
    try {
      const next = await setForkRunApproval(props.org, enabled);
      mutate(next);
      setNotice(
        next.enabled
          ? "Saved. New Runs of fork code wait for approval."
          : "Saved. New Runs of fork code start without approval.",
      );
    } catch (e) {
      // Not an admin, a cancelled confirmation: the server's message.
      setRefusal(e instanceof Error ? e.message : "Could not save the setting.");
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
              <Checkbox
                checked={s().enabled}
                disabled={busy() || !s().can_change}
                onChecked={(on) => void change(on)}
                label="Hold Runs of fork code for approval"
                help={
                  <>
                    A Run of code from a fork, or of a commit no branch or tag of the repository
                    names, waits at an approval gate before any of its steps or services start.
                    Anyone with write on the repository can approve it from the Run page. An API
                    token's owner confirms each approval; an app connected to Scarab cannot
                    approve. An approved Run still gets no Environment secrets, registry
                    credentials or cloud roles. The gate fails the Run after 72 hours. A rerun
                    waits for approval again.
                  </>
                }
              />
              <p class="subtle">
                Runs created before a change keep or lack their gate.
                <Show when={!s().can_change}> Only an org admin, in a browser, can change this.</Show>
                <Show when={s().can_change && s().enabled}>
                  {" "}
                  Turning it off asks you to confirm with your passkey.
                </Show>
              </p>
            </div>
          )}
        </Show>
      </div>
    </div>
  );
}
