// Connect an app to a plugin endpoint: the page an app such as claude.ai
// opens as its authorization endpoint (`/oauth/authorize`).
//
// The server checks the request first (the app's metadata document, read
// only from a host your org's admins allow; the endpoint; your role) and
// says what to show. The page names the host the app's document came from,
// never a name the app chose, the plugin installation and endpoint, and
// every operation the app would get. Connecting asks for your passkey each
// time; the confirmation covers exactly this app, endpoint and operations.
// Approving a gate is never offered.
import { For, Show, createResource, createSignal } from "solid-js";
import {
  consentToAuthorization,
  describeAuthorizationRequest,
  type OAuthAuthorization,
} from "../api/client";
import { authorizationParams, deniedRedirect } from "../connected-apps";
import Icon from "../components/Icon";
import { Checkbox } from "../components/Field";

export default function OAuthAuthorize() {
  const parsed = authorizationParams(window.location.search);
  const [request] = createResource(
    () => (parsed.ok ? parsed.params : null),
    (params) => describeAuthorizationRequest(params),
  );
  const [chosen, setChosen] = createSignal<string[] | null>(null);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  const selected = (r: OAuthAuthorization) =>
    chosen() ?? r.operations.filter((o) => o.selected).map((o) => o.id);
  const toggle = (r: OAuthAuthorization, id: string, on: boolean) => {
    const cur = selected(r).filter((o) => o !== id);
    setChosen(on ? [...cur, id] : cur);
  };

  const connect = async (r: OAuthAuthorization) => {
    if (!parsed.ok || busy()) return;
    setBusy(true);
    setError(null);
    try {
      window.location.assign(await consentToAuthorization(parsed.params, selected(r)));
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not connect the app.");
      setBusy(false);
    }
  };

  const deny = (r: OAuthAuthorization) => {
    if (!parsed.ok) return;
    window.location.assign(deniedRedirect(r.redirect_uri, parsed.params.state));
  };

  return (
    <section class="page">
      <div class="page-head">
        <h1>Connect an app</h1>
      </div>
      <p class="page-sub">an app asks to act for you at one plugin endpoint</p>

      <Show when={!parsed.ok}>
        <div class="panel">
          <div class="secrets-body">
            <p class="error">{parsed.ok ? "" : parsed.error}</p>
          </div>
        </div>
      </Show>
      <Show when={request.error}>
        <div class="panel">
          <div class="secrets-body">
            <p class="error">
              {request.error instanceof Error ? request.error.message : "The request cannot be checked."}
            </p>
            <p class="subtle">
              <small>Nothing was granted. Go back to the app and try again, or ask an org admin.</small>
            </p>
          </div>
        </div>
      </Show>

      <Show when={request()}>
        {(r) => (
          <div class="panel">
            <div class="panel-h">
              <span>
                <Icon icon="shield-check" size={13} /> {r().client_host}
              </span>
            </div>
            <div class="secrets-body">
              <dl class="step-up-summary">
                <dt>App</dt>
                <dd>
                  <span class="mono">{r().client_host}</span>{" "}
                  <span class="subtle">(where its description was read: {r().client_id})</span>
                </dd>
                <dt>Plugin</dt>
                <dd>
                  <span class="mono">{r().installation_name}</span>{" "}
                  <span class="subtle">
                    ({r().plugin} in {r().org}, endpoint <span class="mono">{r().endpoint_path}</span>)
                  </span>
                </dd>
                <dt>Acts as</dt>
                <dd>you, up to {r().role} in {r().org}, and never above what you hold at each call</dd>
                <dt>May</dt>
                <dd>
                  <Show
                    when={r().operations.length > 0}
                    fallback={<span class="subtle">nothing beyond talking to the plugin</span>}
                  >
                    <div class="tok-ops">
                      <For each={r().operations}>
                        {(o) => (
                          <Checkbox
                            checked={selected(r()).includes(o.id)}
                            onChecked={(on) => toggle(r(), o.id, on)}
                            label={
                              <>
                                <span class="mono">{o.id}</span>: {o.label}
                              </>
                            }
                          />
                        )}
                      </For>
                    </div>
                  </Show>
                </dd>
              </dl>
              <p class="subtle">
                <small>
                  Logs can hold text outsiders wrote. The app can never approve a gate. You can
                  disconnect it under Account settings › Connected apps. Connecting asks for your
                  passkey.
                </small>
              </p>
              <Show when={error()}>
                <p class="error">{error()}</p>
              </Show>
              <div class="secrets-actions">
                <button class="btn btn-ghost" type="button" disabled={busy()} onClick={() => deny(r())}>
                  Deny
                </button>
                <button class="btn btn-primary" type="button" disabled={busy()} onClick={() => connect(r())}>
                  {busy() ? "Connecting…" : "Connect"}
                </button>
              </div>
            </div>
          </div>
        )}
      </Show>
    </section>
  );
}
