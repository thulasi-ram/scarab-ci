// Your account (ADR-0069): the sign-in accounts linked to you, and the way to
// link another or unlink one.
//
// Open to everyone signed in — unlike Settings, nothing here is about the org.
// Linking is a full browser round-trip to the provider (the server needs the
// provider to prove you control that account), so the Link button POSTs for
// an authorize URL and then NAVIGATES; the callback lands back here with the
// outcome in the query string, which is read once and shown as a notice.
import { For, Show, createResource, createSignal } from "solid-js";
import { useLocation } from "@solidjs/router";
import {
  beginLinkIdentity,
  getMe,
  listAuthProviders,
  unlinkIdentity,
} from "../api/client";
import Icon from "../components/Icon";
import { canUnlink, linkNotice, linkable, providerName } from "../identities";

export default function Account() {
  const location = useLocation();
  const [me, { refetch }] = createResource(getMe);
  const [providers] = createResource(listAuthProviders);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  const notice = () => linkNotice(location.search, providers() ?? []);
  const identities = () => me()?.identities ?? [];

  const link = async (provider: string) => {
    setBusy(true);
    setError(null);
    try {
      window.location.assign(await beginLinkIdentity(provider));
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not start linking.");
      setBusy(false);
    }
  };

  const unlink = async (provider: string) => {
    const name = providerName(providers() ?? [], provider);
    if (
      !window.confirm(
        `Unlink your ${name} account?\n\nYou will no longer be able to sign in with it, and any roles that were imported from that forge account are removed with it.`,
      )
    )
      return;
    setBusy(true);
    setError(null);
    try {
      await unlinkIdentity(provider);
      await refetch();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not unlink.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <section class="page">
      <div class="page-head">
        <h1>Account</h1>
      </div>
      <p class="page-sub">
        the accounts you sign in with
        <Show when={me()}>
          {/* The id grants and approver lists name — shown verbatim, never in
              the uppercased panel header, because it is matched exactly. */}
          {" "}· you are <code class="mono">{me()!.subject}</code>
        </Show>
      </p>

      <Show when={notice()}>
        {(n) => <p class={n().kind === "error" ? "error" : "notice-ok"}>{n().text}</p>}
      </Show>

      <div class="panel">
        <div class="panel-h">
          <span>Sign-in accounts</span>
        </div>
        <div class="secrets-body">
          <Show when={me.error}>
            <p class="error">Could not load your identity.</p>
          </Show>
          <Show when={error()}>
            <p class="error">{error()}</p>
          </Show>
          <Show
            when={identities().length > 0}
            fallback={
              <Show when={me()}>
                <p class="empty">
                  This server is running without the identity store, so there are no linked
                  accounts to show.
                </p>
              </Show>
            }
          >
            <ul class="tok-list">
              <For each={identities()}>
                {(i) => (
                  <li class="tok">
                    <div class="tok-head">
                      <Icon icon="user" size={15} />
                      <span class="tok-name">{providerName(providers() ?? [], i.provider)}</span>
                      <span class="tok-scope mono">{i.handle}</span>
                      <button
                        class="btn btn-ghost btn-sm tok-revoke"
                        disabled={busy() || !canUnlink(identities())}
                        title={
                          canUnlink(identities())
                            ? undefined
                            : "Your only way to sign in — link another account first"
                        }
                        onClick={() => unlink(i.provider)}
                      >
                        unlink
                      </button>
                    </div>
                  </li>
                )}
              </For>
            </ul>
          </Show>

          <Show when={linkable(providers() ?? [], identities()).length > 0 && identities().length > 0}>
            <div class="secrets-actions">
              <For each={linkable(providers() ?? [], identities())}>
                {(p) => (
                  <button
                    class="btn btn-primary btn-sm"
                    disabled={busy()}
                    onClick={() => link(p.id)}
                  >
                    <Icon icon="plus" size={14} /> Link {p.name}
                  </button>
                )}
              </For>
            </div>
          </Show>
          <p class="subtle">
            <small>
              <Icon icon="shield-check" size={12} /> Accounts are linked only by signing in to
              them from here — never by matching email addresses. Linking an account that
              already signed in on its own folds that history into yours.
            </small>
          </p>
        </div>
      </div>
    </section>
  );
}
