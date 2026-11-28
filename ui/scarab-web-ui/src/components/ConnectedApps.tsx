// Account settings › Connected apps: the apps that act for you at a plugin
// endpoint (OAuth grants). Disconnecting one revokes its grant and token at
// once; anything the app's plugin was doing for you stops on its next call.
import { For, Show, createResource, createSignal } from "solid-js";
import { disconnectApp, listConnectedApps, type ConnectedApp } from "../api/client";
import { connectedAppState, endpointLabel } from "../connected-apps";
import { absTime, relTime } from "../fmt";
import Pill from "./Pill";

export function ConnectedApps() {
  const [epoch, setEpoch] = createSignal(0);
  const [apps] = createResource(epoch, () => listConnectedApps());
  const [error, setError] = createSignal<string | null>(null);

  const disconnect = async (app: ConnectedApp) => {
    setError(null);
    try {
      await disconnectApp(app.id);
      setEpoch((e) => e + 1);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not disconnect the app.");
    }
  };

  return (
    <div class="panel">
      <div class="panel-h">
        <span>Connected apps</span>
      </div>
      <div class="secrets-body">
        <Show when={error()}>
          <p class="error">{error()}</p>
        </Show>
        <Show when={!apps.loading} fallback={<p class="empty">loading…</p>}>
          <Show
            when={apps() !== null}
            fallback={<p class="empty">This server does not let apps connect to plugin endpoints.</p>}
          >
            <Show
              when={(apps() ?? []).length > 0}
              fallback={
                <p class="empty">
                  No app is connected. An app such as claude.ai connects when you approve it with
                  your passkey from its own settings.
                </p>
              }
            >
              <div class="tok-table-scroll">
                <table class="table tok-table" aria-label="Connected apps">
                  <thead>
                    <tr>
                      <th>App</th>
                      <th>Endpoint</th>
                      <th>Operations</th>
                      <th>Last used</th>
                      <th>Ends</th>
                      <th />
                    </tr>
                  </thead>
                  <tbody>
                    <For each={apps() ?? []}>
                      {(app) => {
                        const state = () => connectedAppState(app, Date.now());
                        return (
                          <tr classList={{ "tok-dead": state() !== "connected" }}>
                            <td>
                              <span class="tok-name">{app.client_host}</span>{" "}
                              <Show when={state() !== "connected"}>
                                <Pill tone="muted" look="dotted">
                                  {state()}
                                </Pill>
                              </Show>
                            </td>
                            <td class="mono">
                              {app.org}: {endpointLabel(app.resource)}
                            </td>
                            <td class="mono">{app.operations.join(", ") || "none"}</td>
                            <td class="nw" title={absTime(app.last_used_at)}>
                              {relTime(app.last_used_at)}
                            </td>
                            <td class="nw">{absTime(app.ends_at)}</td>
                            <td class="nw">
                              <Show when={state() === "connected"}>
                                <button class="btn btn-danger btn-sm" onClick={() => disconnect(app)}>
                                  Disconnect
                                </button>
                              </Show>
                            </td>
                          </tr>
                        );
                      }}
                    </For>
                  </tbody>
                </table>
              </div>
            </Show>
          </Show>
        </Show>
      </div>
    </div>
  );
}
