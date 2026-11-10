// Your sign-in accounts: the accounts linked to you, and the way
// to link another or unlink one. The Profile section of Account settings.
//
// Open to everyone signed in. Linking is a full browser round-trip to the
// provider (the server needs the provider to prove you control that account),
// so the Link button POSTs for an authorize URL and then NAVIGATES; the
// callback lands back on this section with the outcome in the query string,
// which is read once and shown as a notice.
import { For, Show, createResource, createSignal } from "solid-js";
import { useLocation } from "@solidjs/router";
import {
  beginLinkIdentity,
  confirmIdentityMerge,
  getIdentityMerge,
  getMe,
  listAuthProviders,
  unlinkIdentity,
} from "../api/client";
import Icon from "./Icon";
import ConfirmDestructive from "./ConfirmDestructive";
import Modal from "./Modal";
import {
  canUnlink,
  confirmMerge as runMergeConfirm,
  linkNotice,
  linkable,
  pendingMergeId,
  providerName,
  type Notice,
} from "../identities";
import { getAssertion, webauthnErrorText } from "../webauthn";

export default function LinkedAccounts() {
  const location = useLocation();
  const [me, { refetch }] = createResource(getMe);
  const [providers] = createResource(listAuthProviders);
  const [busy, setBusy] = createSignal(false);
  const [choosing, setChoosing] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  // A confirmed merge replaces the callback's notice.
  const [merged, setMerged] = createSignal<Notice | null>(null);
  const notice = () => merged() ?? linkNotice(location.search, providers() ?? []);
  const identities = () => me()?.identities ?? [];

  // The link landed on an account that holds passkeys: one of ITS passkeys
  // must confirm the merge, or nothing changes.
  const mergeId = () => (merged() ? null : pendingMergeId(location.search));
  const [merge, { refetch: reloadMerge }] = createResource(mergeId, getIdentityMerge);
  const [mergeError, setMergeError] = createSignal<string | null>(null);
  const confirmMerge = async () => {
    const m = merge();
    if (!m) return;
    setBusy(true);
    setMergeError(null);
    const outcome = await runMergeConfirm(m.id, m.public_key, {
      assert: getAssertion,
      confirm: confirmIdentityMerge,
      reload: reloadMerge,
      webauthnError: webauthnErrorText,
    });
    if ("notice" in outcome) {
      setMerged(outcome.notice);
      await refetch();
    } else {
      setMergeError(outcome.error);
    }
    setBusy(false);
  };

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

  // The account the confirm dialog is about to unlink. A refusal shows in
  // the dialog.
  const [unlinking, setUnlinking] = createSignal<{ provider: string; handle: string } | null>(null);
  const unlink = async (provider: string) => {
    setBusy(true);
    setError(null);
    try {
      await unlinkIdentity(provider);
      await refetch();
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <Show when={me()}>
        {/* The id grants and approver lists name — shown verbatim in Mono,
            not in the panel header, because it is matched exactly. */}
        <p class="subtle">
          You are <code class="mono">{me()!.handle}</code>.
        </p>
      </Show>

      <Show when={notice()}>
        {(n) => <p class={n().kind === "error" ? "error" : "notice-ok"}>{n().text}</p>}
      </Show>

      <Show when={mergeId()}>
        <div class="panel">
          <div class="panel-h">
            <span>Merge an account into yours</span>
          </div>
          <div class="secrets-body">
            <Show when={merge.error}>
              <p class="error">{(merge.error as Error).message}</p>
            </Show>
            <Show when={merge()}>
              {(m) => (
                <>
                  <p>
                    The {providerName(providers() ?? [], m().provider)} account{" "}
                    <code class="mono">{m().handle}</code> is the only way to sign in as{" "}
                    <code class="mono">{m().from}</code>, which has {m().passkeys}{" "}
                    {m().passkeys === 1 ? "passkey" : "passkeys"}. To merge it into your account,
                    confirm with a passkey of <code class="mono">{m().from}</code>. Signing in to
                    the account is not enough on its own.
                  </p>
                  <p class="subtle">
                    Its history, roles and tokens become yours. Its passkeys are deleted; add one of
                    yours under Security if you want to use that device here.
                  </p>
                  <Show when={mergeError()}>
                    <p class="error">{mergeError()}</p>
                  </Show>
                  <div class="secrets-actions">
                    <button class="btn btn-primary btn-sm" disabled={busy()} onClick={confirmMerge}>
                      <Icon icon="shield-check" size={14} /> Confirm with a passkey of {m().from}
                    </button>
                  </div>
                </>
              )}
            </Show>
          </div>
        </div>
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
                        onClick={() => setUnlinking({ provider: i.provider, handle: i.handle })}
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
              {/* One button whatever the operator named its providers: with
                  one left to link it goes straight there, with several it
                  asks which. */}
              <button
                class="btn btn-primary btn-sm"
                disabled={busy()}
                onClick={() => {
                  const left = linkable(providers() ?? [], identities());
                  if (left.length === 1) void link(left[0]!.id);
                  else setChoosing(true);
                }}
              >
                <Icon icon="plus" size={14} /> Link a forge account
              </button>
            </div>
          </Show>
          <Show when={choosing()}>
            <Modal
              title="Link a forge account"
              purpose="Pick where the account lives. You will sign in there and come back here."
              icon="plus"
              busy={busy()}
              onClose={() => setChoosing(false)}
              footer={
                <button class="btn btn-ghost" type="button" onClick={() => setChoosing(false)}>
                  Cancel
                </button>
              }
            >
              <div class="link-choices">
                <For each={linkable(providers() ?? [], identities())}>
                  {(p) => (
                    <button class="btn btn-ghost" type="button" disabled={busy()} onClick={() => link(p.id)}>
                      {p.name}
                    </button>
                  )}
                </For>
              </div>
            </Modal>
          </Show>
          <Show when={unlinking()}>
            {(u) => (
              <ConfirmDestructive
                title={`Unlink your ${providerName(providers() ?? [], u().provider)} account?`}
                purpose="You will no longer be able to sign in with it."
                name={u().handle}
                confirmLabel="Unlink account"
                onConfirm={() => unlink(u().provider)}
                onClose={() => setUnlinking(null)}
              >
                <p>Any roles that were imported from that forge account are removed with it.</p>
              </ConfirmDestructive>
            )}
          </Show>
          <p class="subtle">
            <small>
              <Icon icon="shield-check" size={12} /> Accounts are linked only by signing in to
              them from here — never by matching email addresses. Linking an account that
              already signed in on its own folds that history into yours; if it has passkeys,
              one of them must confirm.
            </small>
          </p>
        </div>
      </div>
    </>
  );
}
