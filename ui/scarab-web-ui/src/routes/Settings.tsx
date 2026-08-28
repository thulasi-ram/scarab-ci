// Global Settings (ADR-0060 part A) — the org-scoped surface that has no repo
// to hang off. There is exactly one Org (single-tenant *experience*), so there
// is no org switcher and no org navigation; `Org` stays the model's real RBAC /
// secret inheritance root (ADR-0049 / ADR-0037), we just never make the user
// pick one.
//
// Everything here needs `Administer` on the Org. The server decides that
// (`GET /v1/me` → `can_administer` / `admin_orgs`) and enforces it per request;
// this route mirrors the answer so a non-admin gets an honest "not yours" page
// rather than a wall of failed fetches. Layout hides the nav entry too — belt
// and braces, not a security boundary.
//
// Three sections today: **Connections** (which forges Scarab is wired to, and
// what they cover), **API tokens** (ADR-0049 — the credential a machine can
// hold, minted here and nowhere else) and **Org Secrets** (the top of the
// `env → repo → org` inheritance chain, until now settable only by raw HTTP).
// Connections comes first: without one there are no Projects, so nothing else
// here has anything to act on. Tokens sit above secrets because they are the
// only thing on this page that hands out authority rather than data.
import { createResource, Show } from "solid-js";
import { getMe } from "../api/client";
import ScopedSecrets from "../components/ScopedSecrets";
import Connections from "../components/Connections";
import ApiTokens from "../components/ApiTokens";
import Icon from "../components/Icon";

export default function Settings() {
  const [me] = createResource(getMe);
  /** The one implicit Org, once anything is bound to it. */
  const org = () => me()?.admin_orgs[0];

  return (
    <section class="page">
      <div class="page-head">
        <h1>Settings</h1>
      </div>
      <p class="page-sub">organization-wide configuration</p>

      <Show when={me.error}>
        <p class="error">Could not load your identity. Is the server up?</p>
      </Show>

      <Show when={me()} fallback={<Show when={!me.error}><p class="empty">loading…</p></Show>}>
        <Show
          when={me()!.can_administer}
          fallback={
            <div class="panel">
              <div class="panel-h"><span>Settings</span></div>
              <div class="secrets-body">
                <p class="empty">
                  These settings need the <b>Administer</b> capability on the organization.
                  Ask an owner if you need access.
                </p>
              </div>
            </div>
          }
        >
          <Connections />
          <Show
            when={org()}
            fallback={
              <div class="panel">
                <div class="panel-h"><span>API tokens &amp; org secrets</span></div>
                <div class="secrets-body">
                  <p class="empty">
                    No organization yet. An org comes into being with its first connected
                    repository — connect a forge and bind a repo, then org-wide secrets and
                    API tokens live here.
                  </p>
                </div>
              </div>
            }
          >
            {(o) => (
              <>
                {/* Org-scoped: a token narrowed to one Project is still issued
                    against the org that contains it, so this needs the same
                    `admin_orgs[0]` the secrets below do. */}
                <ApiTokens org={o()} />
                <ScopedSecrets
                  scope={{ org: o() }}
                  title={`Org secrets · ${o()}`}
                  emptyLabel="No org-wide secrets yet."
                />
                <p class="subtle">
                  <small>
                    <Icon icon="key-round" size={12} /> Org secrets are the base of the
                    inheritance chain — every repo and environment in <b>{o()}</b> resolves
                    them unless it sets the same key at a narrower scope. Per-repo and
                    per-environment values live on the repository's Secrets tab.
                  </small>
                </p>
              </>
            )}
          </Show>
        </Show>
      </Show>
    </section>
  );
}
