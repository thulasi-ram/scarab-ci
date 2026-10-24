// Confirm a StepUpConfirmationRequest (ADR-0073 §6) — the page a
// `confirmation_url` opens.
//
// An API token or a CLI session can never confirm a crown jewel itself; it is
// refused with a one-time link instead, and the human it acts for opens that
// link here, reads exactly what would run (operation, target, the redacted
// changes, who asked, until when), and confirms with a passkey. The server
// then runs the request ONCE, exactly as it was raised, and answers with its
// result — which exists only in that response. A minted API token's plaintext,
// for instance, is shown here once with a copy button and never again.
import { For, Show, createResource, createSignal } from "solid-js";
import { A, useParams } from "@solidjs/router";
import {
  HttpError,
  beginRequestConfirmation,
  confirmStepUpRequest,
  getStepUpRequest,
  type StepUpRequestResult,
} from "../api/client";
import { absTime, relTime } from "../fmt";
import {
  requestConfirmable,
  requestErrorText,
  requestStateText,
  resultSecret,
  resultText,
} from "../security";
import { getAssertion, webauthnAvailable, webauthnErrorText } from "../webauthn";
import Icon from "../components/Icon";

export default function ConfirmRequest() {
  const params = useParams<{ id: string }>();
  const [req, { refetch }] = createResource(() => params.id, getStepUpRequest);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [result, setResult] = createSignal<StepUpRequestResult | null>(null);
  const [copied, setCopied] = createSignal(false);

  const loadError = () => {
    const e = req.error as unknown;
    if (!e) return null;
    return e instanceof HttpError ? requestErrorText(e.status, e.body) : "Could not load the request.";
  };

  const confirm = async () => {
    setBusy(true);
    setError(null);
    try {
      if (!webauthnAvailable()) {
        throw new Error("This browser cannot use passkeys here (passkeys need https, or localhost).");
      }
      const options = await beginRequestConfirmation(params.id);
      const credential = await getAssertion(options.public_key);
      setResult(await confirmStepUpRequest(params.id, options.id, credential));
      await refetch();
    } catch (e) {
      if (e instanceof HttpError) setError(requestErrorText(e.status, e.body));
      else setError(webauthnErrorText(e));
      // A 409/410 means the request moved on under us: show its new state.
      if (e instanceof HttpError && (e.status === 409 || e.status === 410)) await refetch();
    } finally {
      setBusy(false);
    }
  };

  const copy = () => {
    const secret = resultSecret(result()?.result_body);
    if (!secret) return;
    navigator.clipboard?.writeText(secret).then(
      () => setCopied(true),
      () => {},
    );
  };

  return (
    <section class="page">
      <div class="page-head">
        <h1>Confirm a request</h1>
      </div>
      <p class="page-sub">a step-up confirmation request raised on your behalf</p>

      <Show when={loadError()}>
        <div class="panel">
          <div class="panel-h">
            <span>Request</span>
          </div>
          <div class="secrets-body">
            <p class="error">{loadError()}</p>
            <p class="subtle">
              <small>
                Only the person a request is for can confirm it.{" "}
                <A href="/account/security">Your security settings</A>
              </small>
            </p>
          </div>
        </div>
      </Show>

      <Show when={req()}>
        {(r) => (
          <div class="panel">
            <div class="panel-h">
              <span>
                <Icon icon="shield-check" size={13} /> {r().summary.operation}
              </span>
              <span class={`tok-state ${r().state === "failed" || r().state === "expired" ? "tok-revoked" : ""}`}>
                {r().state}
              </span>
            </div>
            <div class="secrets-body">
              <dl class="step-up-summary">
                <dt>operation</dt>
                <dd>
                  {r().summary.operation} <span class="subtle mono">({r().op})</span>
                </dd>
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
                <dt>request</dt>
                <dd class="mono">
                  {r().method} {r().path}
                </dd>
                <dt>requested by</dt>
                <dd>{r().requested_by}</dd>
                <dt>for</dt>
                <dd class="mono">{r().for_human}</dd>
                <dt>raised</dt>
                <dd title={absTime(r().created_at)}>{relTime(r().created_at)}</dd>
                <dt>expires</dt>
                <dd>{absTime(r().expires_at)}</dd>
              </dl>

              <p class="subtle">{requestStateText(r().state)}</p>
              <Show when={r().result_status != null && !result()}>
                <p class="subtle">
                  <small>The operation answered HTTP {r().result_status}.</small>
                </p>
              </Show>

              <Show when={error()}>
                <p class="error">{error()}</p>
              </Show>

              <Show when={requestConfirmable(r(), Date.now()) && !result()}>
                <div class="modal-actions">
                  <button class="btn btn-primary" disabled={busy()} onClick={confirm}>
                    <Icon icon="key-round" size={14} />{" "}
                    {busy() ? "Waiting for passkey…" : "Confirm with passkey"}
                  </button>
                </div>
                <p class="subtle">
                  <small>
                    Confirming runs this exact request once, as it was raised. If anything
                    above is not what you expect, do not confirm — it expires on its own.
                  </small>
                </p>
              </Show>

              <Show when={result()}>
                {(res) => (
                  <div class="sec-result">
                    <p class={res().state === "executed" ? "notice-ok" : "error"}>
                      {res().state === "executed" ? "It ran." : "It ran, and the operation failed."}{" "}
                      <Show when={res().result_status != null}>HTTP {res().result_status}.</Show>
                    </p>
                    <Show when={resultSecret(res().result_body)}>
                      {(secret) => (
                        <>
                          <p class="tok-warn">
                            <Icon icon="alert-triangle" size={14} />
                            <span>
                              Copy this now. It is shown this once — the server never stores the
                              result, and leaving this page loses it.
                            </span>
                          </p>
                          <code class="tok-secret mono">{secret()}</code>
                          <div class="modal-actions">
                            <button class="btn btn-primary" onClick={copy}>
                              {copied() ? "Copied" : "Copy"}
                            </button>
                          </div>
                        </>
                      )}
                    </Show>
                    <Show when={resultText(res().result_body)}>
                      <pre class="sec-result-body mono">{resultText(res().result_body)}</pre>
                    </Show>
                  </div>
                )}
              </Show>
            </div>
          </div>
        )}
      </Show>
    </section>
  );
}
