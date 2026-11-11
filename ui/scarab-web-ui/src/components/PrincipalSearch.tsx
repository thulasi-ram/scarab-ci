// Find a user to grant a role to or to make an approver.
//
// Grants and approver entries must name a user who exists — a typed string
// nobody holds would be claimed by whoever produced it first at sign-in — so
// the input is a search over the users Scarab knows, not a free-text
// box. It shows who someone IS (name, linked accounts, whether they have
// signed in yet) and hands back their handle, which the server resolves to
// the Principal a grant stores.
import { For, Show, createSignal, onCleanup } from "solid-js";
import { searchPrincipals, type PrincipalMatch } from "../api/client";
import { Input } from "./Field";
import Pill from "./Pill";

export default function PrincipalSearch(props: {
  org: string;
  /** Called with the picked person's handle. */
  onPick: (handle: string) => void;
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

  const pick = (handle: string) => {
    props.onPick(handle);
    setQ("");
    setHits([]);
  };

  return (
    <div class="ps">
      <Input
        value={q()}
        onValue={onInput}
        placeholder={props.placeholder ?? "find a user — name, login or email"}
        aria-label="Find a user"
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
                <button type="button" class="ps-hit" onClick={() => pick(h.handle)}>
                  <span class="ps-name">{h.display_name || h.handle}</span>
                  <span class="ps-id mono">{h.handle}</span>
                  <For each={h.identities}>
                    {(i) => (
                      <Pill tone="plain" look="dotted">
                        {i.provider}:{i.handle}
                      </Pill>
                    )}
                  </For>
                  <Show when={!h.claimed}>
                    <Pill tone="muted" look="dotted">not signed in yet</Pill>
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
