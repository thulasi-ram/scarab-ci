// Secrets settings page (ADR-0014). Pick a scope (org / repo / environment),
// list the secret NAMES defined there, add a new secret (name + value), and
// delete one. Values are write-only — the API never returns them, so this page
// never displays a value.
import { createResource, createSignal, For, Show } from "solid-js";
import { listSecrets, putSecret, deleteSecret, type SecretScope } from "../api/client";

export default function Secrets(props: { onBack: () => void }) {
  const [org, setOrg] = createSignal("acme");
  const [repo, setRepo] = createSignal("");
  const [environment, setEnvironment] = createSignal("");
  const [name, setName] = createSignal("");
  const [value, setValue] = createSignal("");
  const [err, setErr] = createSignal<string | null>(null);

  const scope = (): SecretScope => ({
    org: org(),
    repo: repo().trim() || undefined,
    environment: environment().trim() || undefined,
  });
  const scopeLabel = () =>
    [org(), repo().trim(), environment().trim()].filter(Boolean).join(" / ");

  const [names, { refetch }] = createResource(scope, listSecrets);

  async function add(e: Event) {
    e.preventDefault();
    setErr(null);
    try {
      await putSecret({ ...scope(), name: name().trim(), value: value() });
      setName("");
      setValue("");
      await refetch();
    } catch (x) {
      setErr(String(x));
    }
  }

  async function remove(secretName: string) {
    setErr(null);
    try {
      await deleteSecret(scope(), secretName);
      await refetch();
    } catch (x) {
      setErr(String(x));
    }
  }

  return (
    <section>
      <button onClick={props.onBack}>← runs</button>
      <h1>Secrets</h1>

      <fieldset>
        <legend>Scope</legend>
        <label>org <input value={org()} onInput={(e) => setOrg(e.currentTarget.value)} /></label>{" "}
        <label>repo <input value={repo()} onInput={(e) => setRepo(e.currentTarget.value)} placeholder="(org scope)" /></label>{" "}
        <label>environment <input value={environment()} onInput={(e) => setEnvironment(e.currentTarget.value)} placeholder="(needs repo)" /></label>
      </fieldset>

      <h2>Defined at <code>{scopeLabel()}</code></h2>
      <Show when={!names.loading} fallback={<p>loading…</p>}>
        <Show when={(names()?.length ?? 0) > 0} fallback={<p>No secrets at this scope.</p>}>
          <ul>
            <For each={names()}>
              {(n) => (
                <li>
                  <code>{n}</code>{" "}
                  <button onClick={() => remove(n)}>delete</button>
                </li>
              )}
            </For>
          </ul>
        </Show>
      </Show>

      <h2>Add a secret</h2>
      <form onSubmit={add}>
        <input value={name()} onInput={(e) => setName(e.currentTarget.value)} placeholder="NAME" />{" "}
        <input
          type="password"
          value={value()}
          onInput={(e) => setValue(e.currentTarget.value)}
          placeholder="value (write-only)"
        />{" "}
        <button type="submit" disabled={!name().trim()}>Save</button>
      </form>
      <Show when={err()}>{(m) => <p style={{ color: "crimson" }}>{m()}</p>}</Show>
      <p><small>Values are encrypted at rest and never displayed — you can overwrite but not read a value back.</small></p>
    </section>
  );
}
