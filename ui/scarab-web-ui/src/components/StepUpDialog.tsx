// The one step-up dialog (ADR-0073), mounted once in Layout and opened by the
// API client whenever a request is refused with `403 step_up_required`.
//
// It shows the server's own summary of what is being confirmed (operation,
// target, the changes — secret values already "(hidden)"), and on "Confirm
// with passkey" runs the WebAuthn assertion over the challenge the refusal
// carried, posts it to `/v1/me/step-up`, and closes with `true` so the client
// retries the refused request. The click is not decoration: browsers only
// open the passkey prompt from a user gesture.
//
// Cancel closes with `false`, and the caller gets the original 403 — the page
// that made the call shows its usual error.
//
// A failed confirm splits two ways (`confirmOnce`): a dismissed or timed-out
// passkey prompt never reached the server, so "Try again" over the same
// challenge is real; a refusal FROM the server spent the challenge, so the
// dialog closes with `{ reissue }` and the client re-sends the request for a
// fresh one — the dialog reopens with the server's reason shown.
import { For, Show, createSignal } from "solid-js";
import { A } from "@solidjs/router";
import { confirmStepUp } from "../api/client";
import {
  confirmOnce,
  stepUpPrompt,
  stepUpScope,
  stepUpTitle,
  type CeremonyAnswer,
} from "../step-up";
import { getAssertion, webauthnAvailable, webauthnErrorText } from "../webauthn";
import Icon from "./Icon";

export default function StepUpDialog() {
  return (
    <Show when={stepUpPrompt()} keyed>
      {(p) => <StepUpModal refusal={p.refusal} note={p.note} settle={p.settle} />}
    </Show>
  );
}

function StepUpModal(props: {
  refusal: NonNullable<ReturnType<typeof stepUpPrompt>>["refusal"];
  note?: string;
  settle: (answer: CeremonyAnswer) => void;
}) {
  const r = () => props.refusal;
  const [busy, setBusy] = createSignal(false);
  // A reopened dialog starts with the reason the last confirmation was refused.
  const [error, setError] = createSignal<string | null>(props.note ?? null);
  const canConfirm = () => r().mode !== "enroll" && !!r().challenge;

  const confirm = async () => {
    const challenge = r().challenge;
    if (!challenge) return;
    setBusy(true);
    setError(null);
    const outcome = await confirmOnce(challenge, {
      assert: async (publicKey) => {
        if (!webauthnAvailable()) {
          throw new Error(
            "This browser cannot use passkeys here (passkeys need https, or localhost).",
          );
        }
        return getAssertion(publicKey);
      },
      confirm: confirmStepUp,
      browserError: webauthnErrorText,
    });
    if (outcome.kind === "confirmed") {
      props.settle(true);
    } else if (outcome.kind === "reissue") {
      // The server spent this challenge: ask the client for a fresh one.
      props.settle({ reissue: outcome.error });
    } else {
      // A dismissed prompt leaves the challenge unused: trying again is real.
      setError(outcome.error);
      setBusy(false);
    }
  };

  return (
    <div class="modal-scrim" onClick={() => !busy() && props.settle(false)}>
      <div
        class="modal step-up-modal"
        role="dialog"
        aria-modal="true"
        aria-labelledby="step-up-title"
        onClick={(e) => e.stopPropagation()}
      >
        <div class="panel-h">
          <span id="step-up-title">
            <Icon icon="shield-check" size={13} /> {stepUpTitle(r())}
          </span>
        </div>
        <div class="modal-body">
          <dl class="step-up-summary">
            <dt>operation</dt>
            <dd>{r().summary.operation}</dd>
            <Show when={r().summary.target}>
              <dt>target</dt>
              <dd class="mono">{r().summary.target}</dd>
            </Show>
            <Show when={r().summary.changes.length > 0}>
              <dt>changes</dt>
              <dd>
                <ul class="step-up-changes mono">
                  <For each={r().summary.changes}>{(c) => <li>{c}</li>}</For>
                </ul>
              </dd>
            </Show>
          </dl>
          <p class="subtle modal-note">
            <small>{stepUpScope(r())}</small>
          </p>

          <Show when={r().mode === "enroll"}>
            <p class="tok-warn">
              <Icon icon="alert-triangle" size={14} />
              <span>
                {r().message}
                <Show when={!r().fresh_login_until}>
                  {" "}Your login is older than 5 minutes, so enrolling starts with signing in
                  again.
                </Show>
              </span>
            </p>
          </Show>
          <Show when={r().mode !== "enroll" && !r().challenge}>
            <p class="error">{r().message}</p>
          </Show>
          <Show when={error()}>
            <p class="error">{error()}</p>
          </Show>

          <div class="modal-actions">
            <Show when={canConfirm()}>
              <button class="btn btn-primary" disabled={busy()} onClick={confirm} autofocus>
                <Icon icon="key-round" size={14} />{" "}
                {busy() ? "Waiting for passkey…" : error() ? "Try again" : "Confirm with passkey"}
              </button>
            </Show>
            <Show when={r().mode === "enroll"}>
              <A
                class="btn btn-primary"
                href={r().enroll_url ?? "/account/security?enroll=1"}
                onClick={() => props.settle(false)}
              >
                <Icon icon="key-round" size={14} /> Enroll a passkey
              </A>
            </Show>
            <button class="btn btn-ghost" disabled={busy()} onClick={() => props.settle(false)}>
              Cancel
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
