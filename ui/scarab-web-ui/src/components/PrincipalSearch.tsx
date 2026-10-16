// Find a user to grant a role to or to make an approver (ADR-0069 §3).
//
// Grants and approver entries must name a user who exists — a typed string
// nobody holds would be claimed by whoever produced it first at sign-in — so
// the honest input is a search over the users Scarab knows, not a free-text
// box. It shows who someone IS (name, linked accounts, whether they have
// signed in yet) and hands back the id a grant stores.
import { For, Show, createSignal, onCleanup } from "solid-js";
import { searchPrincipals, type PrincipalMatch } from "../api/client";

export default function PrincipalSearch(props: {
  org: string;
  onPick: (id: string) => void;
  placeholder?: string;
}) {
  const [q, setQ] = createSignal("");
  const [hits, setHits] = createSignal<PrincipalMatch[]>([]);
  const [error, setError] = createSignal<string | null>(null);
  let timer: ReturnType<typeof setTimeout> | undefined;
  let seq = 0;
  onCleanup(() => clearTimeout(timer));

  const onInput = (value: string) => {
    setQ(value);
    clearTimeout(timer);
    // Debounced, and only the latest answer lands: a slow early response must
    // not overwrite the list for what was typed after it.
    const mine = ++seq;
    timer = setTimeout(async () => {
      try {
        const found = await searchPrincipals(props.org, value);
        if (mine === seq) {
          setHits(found);
          setError(null);
        }
      } catch (e) {
        if (mine === seq) setError(e instanceof Error ? e.message : "search failed");
      }
    }, 180);
  };

  const pick = (id: string) => {
    props.onPick(id);
    setQ("");
    setHits([]);
  };

  return (
    <div class="ps">
      <input
        class="input"
        placeholder={props.placeholder ?? "find a user — name, login or email"}
        value={q()}
        onInput={(e) => onInput(e.currentTarget.value)}
      />
      <Show when={error()}>
        <p class="error">{error()}</p>
      </Show>
      <Show when={q().trim() && hits().length === 0 && !error()}>
        <p class="fieldnote">
          No match. A user exists once they have signed in, or once their forge account has
          been imported from a repository.
        </p>
      </Show>
      <Show when={hits().length > 0}>
        <ul class="ps-list">
          <For each={hits()}>
            {(h) => (
              <li>
                <button type="button" class="ps-hit" onClick={() => pick(h.id)}>
                  <span class="ps-name">{h.display_name || h.id}</span>
                  <span class="ps-id mono">{h.id}</span>
                  <For each={h.identities}>
                    {(i) => (
                      <span class="ps-acct mono">
                        {i.provider}:{i.handle}
                      </span>
                    )}
                  </For>
                  <Show when={!h.claimed}>
                    <span class="tok-state tok-expired">not signed in yet</span>
                  </Show>
                </button>
              </li>
            )}
          </For>
        </ul>
      </Show>
    </div>
  );
}
