// Issued API tokens (git-bug a4cb94f). A token belongs to the person who
// minted it, so there are two sections:
//
// - Your account › API tokens (`MyApiTokens`): mint, list and revoke your own.
// - Organization settings › API tokens (`OrgApiTokens`): every token issued in
//   the org, whoever holds it, sorted by last use, calls or creation. An admin
//   revokes any of them here; nobody mints here.
//
// The plaintext exists in one response, the mint 201, because Scarab stores
// only a SHA-256 of it. There is no "show token" anywhere in this file: a token
// that is not copied out of the mint dialog is re-minted, never recovered. The
// dialog says so, and does not close by accident while the secret is on screen.
//
// A token's `role` is a ceiling, not a grant: every request takes the lower of
// that ceiling and whatever the owner holds at that moment, so demoting the
// owner demotes their tokens at once. Every place a role appears here says
// "up to" for that reason.
import {
  createResource,
  createSignal,
  For,
  Match,
  onCleanup,
  onMount,
  Show,
  Switch,
  type JSX,
} from "solid-js";
import { A } from "@solidjs/router";
import {
  getMe,
  listApiTokens,
  listMyApiTokens,
  listProjects,
  mintMyApiToken,
  revokeApiToken,
  revokeMyApiToken,
  type ApiToken,
  type MintedToken,
  type TokenSort,
} from "../api/client";
import {
  expiresSoon,
  expiryLabel,
  isLive,
  lifetimeError,
  MAX_TOKEN_DAYS,
  mintableOrgs,
  ROLE_CHOICES,
  scopeLabel,
  sortTokens,
  TOKEN_SORTS,
  tokenState,
  type TokenRole,
} from "../api-tokens";
import { confirmsName } from "../org-environments";
import { accountHref } from "../settings-nav";
import { relTime, absTime } from "../fmt";
import Icon from "./Icon";
import FormSection from "./FormSection";

/** The lifetime a token gets unless someone says otherwise. A quarter: long
 * enough that rotating is not a weekly chore, short enough that an abandoned
 * credential dies on its own. */
const DEFAULT_DAYS = 90;

/** What the panel says when the server has no token store. */
function NoTokenStore() {
  return (
    <p class="empty">
      This deployment cannot issue API tokens: it runs without the database-backed token
      store. Tokens are stored hashed in Postgres, and a server with no store has nowhere to
      keep them.
    </p>
  );
}

/** Your account › API tokens: the tokens you hold, in every org. */
export function MyApiTokens() {
  const [epoch, setEpoch] = createSignal(0);
  const [tokens] = createResource(epoch, () => listMyApiTokens());
  const [minting, setMinting] = createSignal(false);
  const [revoking, setRevoking] = createSignal<string | null>(null);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const reload = () => setEpoch((e) => e + 1);

  // `null` is the deployment answering "I have no token store", which is not
  // the same sentence as "you have no tokens".
  const unavailable = () => !tokens.loading && !tokens.error && tokens() === null;
  const rows = () => sortTokens(tokens() ?? [], Date.now());
  const liveCount = () => rows().filter((t) => isLive(t, Date.now())).length;

  const revoke = async (t: ApiToken) => {
    setBusy(true);
    setError(null);
    try {
      await revokeMyApiToken(t.id);
      setRevoking(null);
      reload();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not revoke the token.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div class="panel">
      <div class="panel-h">
        <span>Your API tokens</span>
        <Show when={liveCount() > 0}>
          <span class="tok-count">{liveCount()} active</span>
        </Show>
      </div>
      <div class="secrets-body">
        <Show when={tokens.error}>
          <p class="error">Could not load your API tokens.</p>
        </Show>
        <Show when={error()}>
          <p class="error">{error()}</p>
        </Show>

        <Show when={!tokens.loading} fallback={<p class="empty">loading…</p>}>
          <Show when={!unavailable()} fallback={<NoTokenStore />}>
            <Show
              when={rows().length > 0}
              fallback={
                <p class="empty">
                  You hold no tokens yet. An API token lets something that is not a browser
                  talk to Scarab as you: the CLI, a CI job on another system, a status poller,
                  an agent driving runs.
                </p>
              }
            >
              <ul class="tok-list">
                <For each={rows()}>
                  {(t) => (
                    <TokenRow token={t} onRevoke={() => setRevoking(t.id)}>
                      <Show when={revoking() === t.id}>
                        <RevokeToken
                          token={t}
                          busy={busy()}
                          onConfirm={() => void revoke(t)}
                          onCancel={() => setRevoking(null)}
                        />
                      </Show>
                    </TokenRow>
                  )}
                </For>
              </ul>
            </Show>

            <div class="secrets-actions">
              <button class="btn btn-primary btn-sm" onClick={() => setMinting(true)}>
                <Icon icon="plus" size={14} /> New token
              </button>
            </div>
            <p class="subtle">
              <small>
                <Icon icon="shield-check" size={12} /> Stored as a SHA-256 digest and shown
                once, at mint. A token acts as you, up to its role ceiling and never above
                what you hold at the time of each request, so losing your access disarms your
                tokens too. Tokens cannot mint tokens. Your org's admins can see and revoke
                every token issued in their org.
              </small>
            </p>
          </Show>
        </Show>
      </div>

      <Show when={minting()}>
        <MintTokenDialog onClose={() => setMinting(false)} onMinted={reload} />
      </Show>
    </div>
  );
}

/** One of your tokens: what it is, what it may do, and whether it is alive. */
function TokenRow(props: { token: ApiToken; onRevoke: () => void; children?: JSX.Element }) {
  const t = () => props.token;
  const now = Date.now();
  const state = () => tokenState(t(), now);

  return (
    <li class="tok" classList={{ "tok-dead": state() !== "live" }}>
      <div class="tok-head">
        <Icon icon="key-round" size={15} />
        <span class="tok-name">{t().name}</span>
        <span class="tok-scope mono">{scopeLabel(t())}</span>
        <span class="tok-role">up to {t().role}</span>
        <Show when={state() === "revoked"}>
          <span class="tok-state tok-revoked">revoked</span>
        </Show>
        <Show when={state() === "expired"}>
          <span class="tok-state tok-expired">expired</span>
        </Show>
        <Show when={state() === "live"}>
          <button class="btn btn-danger btn-sm tok-revoke" onClick={props.onRevoke}>
            Revoke
          </button>
        </Show>
      </div>
      <div class="tok-facts">
        {/* Written back at most once a minute server-side, so both the last use
            and the call count are approximate. */}
        <span title={t().last_used_at ? absTime(t().last_used_at!) : undefined}>
          <Icon icon="clock" size={12} />
          {t().last_used_at ? `last used ${relTime(t().last_used_at!)}` : "never used"}
        </span>
        <span>
          <Icon icon="history" size={12} />
          {t().use_count === 1 ? "1 call" : `${t().use_count.toLocaleString("en-US")} calls`}
        </span>
        <Show
          when={state() === "revoked"}
          fallback={
            <span
              class={expiresSoon(t(), now) ? "tok-soon" : undefined}
              title={absTime(t().expires_at)}
            >
              <Icon icon="timer" size={12} />
              {expiryLabel(t(), now)}
            </span>
          }
        >
          <span title={absTime(t().revoked_at!)}>
            <Icon icon="timer" size={12} />
            revoked {relTime(t().revoked_at!)}
          </span>
        </Show>
        <span title={absTime(t().created_at)}>
          <Icon icon="user" size={12} />
          created {relTime(t().created_at)}
        </span>
      </div>
      {props.children}
    </li>
  );
}

/** The revoke confirm: what stops working, and the token's name typed exactly. */
function RevokeToken(props: {
  token: ApiToken;
  busy: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  const [typed, setTyped] = createSignal("");
  const ok = () => confirmsName(typed(), props.token.name) && !props.busy;
  return (
    <div class="typed-confirm">
      <p>
        Revoke <b>{props.token.name}</b>, held by{" "}
        <span class="mono">{props.token.owner_subject}</span>? The next request that presents
        it is refused. Anything still using it (a CI job, a script, an agent) starts failing,
        and the token cannot be restored, only replaced.
      </p>
      <label>
        <span>
          Type <b class="mono">{props.token.name}</b> to confirm
        </span>
        <input
          class="input mono"
          aria-label={`Type ${props.token.name} to confirm`}
          value={typed()}
          onInput={(e) => setTyped(e.currentTarget.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && ok()) props.onConfirm();
          }}
          autofocus
        />
      </label>
      <div class="modal-actions">
        <button class="btn btn-danger btn-sm" disabled={!ok()} onClick={() => props.onConfirm()}>
          {props.busy ? "Revoking…" : "Revoke token"}
        </button>
        <button class="btn btn-ghost btn-sm" onClick={() => props.onCancel()}>
          Cancel
        </button>
      </div>
    </div>
  );
}

/** Organization settings › API tokens: every token issued in `org`. */
export function OrgApiTokens(props: { org: string }) {
  const [epoch, setEpoch] = createSignal(0);
  const [sort, setSort] = createSignal<TokenSort>("last_used");
  const [tokens] = createResource(
    () => ({ org: props.org, sort: sort(), epoch: epoch() }),
    (k) => listApiTokens(k.org, k.sort),
  );
  const [revoking, setRevoking] = createSignal<string | null>(null);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const reload = () => setEpoch((e) => e + 1);

  const unavailable = () => !tokens.loading && !tokens.error && tokens() === null;
  // The server's order, as asked for: a stale row keeps its place, so the list
  // does not move under the cursor between refetches.
  const rows = () => tokens() ?? [];
  const liveCount = () => rows().filter((t) => isLive(t, Date.now())).length;
  const revokingToken = () => rows().find((t) => t.id === revoking());

  const revoke = async (t: ApiToken) => {
    setBusy(true);
    setError(null);
    try {
      await revokeApiToken(props.org, t.id);
      setRevoking(null);
      reload();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not revoke the token.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div class="panel">
      <div class="panel-h">
        <span>API tokens · {props.org}</span>
        <Show when={liveCount() > 0}>
          <span class="tok-count">{liveCount()} active</span>
        </Show>
      </div>
      <div class="secrets-body">
        <p class="subtle">
          Every token issued in {props.org}, whoever holds it. Tokens are personal: each
          person mints their own under{" "}
          <A href={accountHref("tokens")}>Your account › API tokens</A>.
        </p>
        <Show when={tokens.error}>
          <p class="error">Could not load API tokens.</p>
        </Show>
        <Show when={error()}>
          <p class="error">{error()}</p>
        </Show>

        <Show when={!unavailable()} fallback={<NoTokenStore />}>
          <div class="tok-sort" role="group" aria-label="Sort tokens by">
            <span class="tok-sort-label">Sort by</span>
            <For each={TOKEN_SORTS}>
              {(s) => (
                <button
                  class="btn btn-ghost btn-sm"
                  classList={{ "tok-preset-on": sort() === s.sort }}
                  aria-pressed={sort() === s.sort}
                  onClick={() => setSort(s.sort)}
                >
                  {s.label}
                </button>
              )}
            </For>
          </div>
          <Show when={tokens() !== undefined} fallback={<p class="empty">loading…</p>}>
            <Show
              when={rows().length > 0}
              fallback={<p class="empty">No tokens have been issued in {props.org} yet.</p>}
            >
              <div class="tok-table-scroll">
                <table class="table tok-table">
                  <thead>
                    <tr>
                      <th>Name</th>
                      <th>Owner</th>
                      <th>Scope</th>
                      <th aria-sort={sort() === "last_used" ? "descending" : undefined}>Last used</th>
                      <th aria-sort={sort() === "use_count" ? "descending" : undefined}>Calls</th>
                      <th aria-sort={sort() === "created" ? "descending" : undefined}>Created</th>
                      <th>Expires</th>
                      <th />
                    </tr>
                  </thead>
                  <tbody>
                    <For each={rows()}>
                      {(t) => <OrgTokenRow token={t} onRevoke={() => setRevoking(t.id)} />}
                    </For>
                  </tbody>
                </table>
              </div>
            </Show>
          </Show>
          <Show when={revokingToken()}>
            {(t) => (
              <RevokeToken
                token={t()}
                busy={busy()}
                onConfirm={() => void revoke(t())}
                onCancel={() => setRevoking(null)}
              />
            )}
          </Show>
          <p class="subtle">
            <small>
              <Icon icon="clock" size={12} /> Last use and calls are written back at most once a
              minute per token, so both can lag by a minute. An org policy that decides who may
              issue tokens, with which scopes and lifetimes, is not available yet: today anyone
              with a role in an org or project, viewers included, may issue a token there up to
              that role, for up to {MAX_TOKEN_DAYS} days.
            </small>
          </p>
        </Show>
      </div>
    </div>
  );
}

/** One row of the org's list. */
function OrgTokenRow(props: { token: ApiToken; onRevoke: () => void }) {
  const t = () => props.token;
  const now = Date.now();
  const state = () => tokenState(t(), now);
  return (
    <tr classList={{ "tok-dead": state() !== "live" }}>
      <td>
        <span class="tok-name">{t().name}</span>
      </td>
      <td>
        <span class="mono">{t().owner_subject}</span>
      </td>
      <td>
        <span class="mono">{scopeLabel(t())}</span>{" "}
        <span class="tok-role">up to {t().role}</span>
      </td>
      <td title={t().last_used_at ? absTime(t().last_used_at!) : undefined}>
        {t().last_used_at ? relTime(t().last_used_at!) : "never"}
      </td>
      <td class="mono">{t().use_count.toLocaleString("en-US")}</td>
      <td title={absTime(t().created_at)}>{relTime(t().created_at)}</td>
      <td title={absTime(t().revoked_at ?? t().expires_at)}>
        <Show
          when={state() === "revoked"}
          fallback={
            <span class={expiresSoon(t(), now) ? "tok-soon" : undefined}>{expiryLabel(t(), now)}</span>
          }
        >
          <span class="tok-state tok-revoked">revoked</span>
        </Show>
      </td>
      <td>
        <Show when={state() === "live"}>
          <button class="btn btn-danger btn-sm" onClick={props.onRevoke}>
            Revoke
          </button>
        </Show>
      </td>
    </tr>
  );
}

/**
 * Mint a token for yourself, then reveal it once.
 *
 * Two states in one modal rather than two dialogs: the second is the result of
 * the first, and a fresh scrim to dismiss is how a secret gets clicked away.
 * While the plaintext is showing, the scrim and Escape do not close the
 * dialog; the only way out is the button that says the secret is copied.
 */
function MintTokenDialog(props: { onClose: () => void; onMinted: () => void }) {
  const [name, setName] = createSignal("");
  const [org, setOrg] = createSignal("");
  const [project, setProject] = createSignal("");
  const [role, setRole] = createSignal<TokenRole>("viewer");
  const [days, setDays] = createSignal(DEFAULT_DAYS);
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [minted, setMinted] = createSignal<MintedToken | null>(null);
  const [copied, setCopied] = createSignal(false);

  // Escape dismisses the form and nothing else: once the plaintext is on
  // screen, the reflex that closes a modal is the reflex that loses it.
  onMount(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !minted()) props.onClose();
    };
    document.addEventListener("keydown", onKey);
    onCleanup(() => document.removeEventListener("keydown", onKey));
  });

  // The orgs where you hold a role (the server's list) and the projects you
  // can see, so the scope picker offers only orgs a mint can succeed in. A
  // failure here is not fatal: the org can still be typed.
  const [scopes] = createResource(async () => {
    const [me, projects] = await Promise.all([
      getMe().catch(() => null),
      listProjects().catch(() => []),
    ]);
    // No list (the call failed, or a server from before the field): type it.
    const orgs = me?.token_mint_orgs ? mintableOrgs(me.token_mint_orgs) : null;
    if (org() === "" && orgs && orgs.length > 0) setOrg(orgs[0]!);
    return { orgs, projects };
  });
  const orgProjects = () =>
    (scopes()?.projects ?? [])
      .filter((p) => p.org === org())
      .map((p) => p.project)
      .sort();

  const lifetimeProblem = () => lifetimeError(days());
  const ready = () =>
    name().trim().length > 0 && org().trim().length > 0 && lifetimeProblem() === null;

  const mint = async () => {
    if (!ready() || saving()) return;
    setSaving(true);
    setError(null);
    try {
      setMinted(
        await mintMyApiToken({
          org: org().trim(),
          name: name().trim(),
          role: role(),
          expires_in_days: days(),
          ...(project() ? { project: project() } : {}),
        }),
      );
      // Refresh the list underneath now, so closing the reveal lands on a list
      // that already holds the new row.
      props.onMinted();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not mint the token.");
    } finally {
      setSaving(false);
    }
  };

  const copy = () => {
    const secret = minted()?.token;
    if (!secret) return;
    navigator.clipboard?.writeText(secret).then(
      () => setCopied(true),
      () => {},
    );
  };

  return (
    <div class="modal-scrim" onClick={() => !minted() && props.onClose()}>
      {/* `modal-tall`: the fields scroll on a short viewport and the actions
          stay pinned in `.modal-foot`, where they can always be reached. */}
      <div class="modal modal-tall" onClick={(e) => e.stopPropagation()}>
        <div class="panel-h">
          <span>{minted() ? "Your new token" : "New API token"}</span>
        </div>

        <Show
          when={minted()}
          fallback={
            <div class="modal-body">
              <FormSection n={1} title="Name">
                <input
                  class="input"
                  aria-label="name"
                  placeholder="release-bot, laptop CLI, demo keepalive"
                  value={name()}
                  onInput={(e) => setName(e.currentTarget.value)}
                  autofocus
                />
                <p class="fieldnote">
                  What makes it revocable later: the only way to tell two tokens apart.
                </p>
              </FormSection>

              <FormSection n={2} title="Scope">
                <Switch
                  fallback={
                    <input
                      class="input mono"
                      aria-label="organization"
                      placeholder="acme"
                      value={org()}
                      onInput={(e) => setOrg(e.currentTarget.value)}
                    />
                  }
                >
                  <Match when={scopes()?.orgs?.length === 0}>
                    <p class="fieldnote" role="status">
                      You hold no role in any organization, so there is nowhere to mint a token
                      yet. An org admin can grant you one.
                    </p>
                  </Match>
                  <Match when={(scopes()?.orgs?.length ?? 0) > 0}>
                    <select
                      class="input mono"
                      aria-label="organization"
                      value={org()}
                      onChange={(e) => {
                        setOrg(e.currentTarget.value);
                        setProject("");
                      }}
                    >
                      <For each={scopes()?.orgs ?? []}>{(o) => <option value={o}>{o}</option>}</For>
                    </select>
                  </Match>
                </Switch>
                <select
                  class="input mono"
                  aria-label="scope"
                  value={project()}
                  onChange={(e) => setProject(e.currentTarget.value)}
                >
                  <option value="">All of {org() || "the org"}</option>
                  <For each={orgProjects()}>
                    {(p) => (
                      <option value={p}>
                        {org()}/{p}
                      </option>
                    )}
                  </For>
                </select>
                <p class="fieldnote">
                  A token scoped to one repository cannot see another's runs. Narrow it unless
                  it needs the whole org.
                </p>
              </FormSection>

              <FormSection n={3} title="Role ceiling">
                <select
                  class="input"
                  aria-label="role ceiling"
                  value={role()}
                  onChange={(e) => setRole(e.currentTarget.value as TokenRole)}
                >
                  <For each={ROLE_CHOICES}>
                    {(r) => (
                      <option value={r.role}>
                        up to {r.role}: {r.what}
                      </option>
                    )}
                  </For>
                </select>
                <p class="fieldnote">
                  A ceiling, not a grant: each request also checks what you hold at that
                  moment and takes the lower of the two. You cannot mint above your own role
                  in this scope.
                </p>
              </FormSection>

              <FormSection n={4} title="Expires in">
                <div class="tok-life">
                  <input
                    class="input mono tok-days"
                    aria-label="expires in (days)"
                    type="number"
                    min="1"
                    max={MAX_TOKEN_DAYS}
                    value={days()}
                    onInput={(e) => setDays(Number(e.currentTarget.value))}
                  />
                  <span class="tok-life-unit">days</span>
                  <For each={[30, 90, 365]}>
                    {(d) => (
                      <button
                        class="btn btn-ghost btn-sm"
                        classList={{ "tok-preset-on": days() === d }}
                        onClick={() => setDays(d)}
                      >
                        {d}d
                      </button>
                    )}
                  </For>
                </div>
                <p class="fieldnote">
                  Required, 1–{MAX_TOKEN_DAYS}. There is no "never": Scarab does not mint a
                  credential that carries a role and never expires.
                </p>
              </FormSection>
            </div>
          }
        >
          {(m) => (
            <div class="modal-body">
              <p class="tok-warn">
                <Icon icon="alert-triangle" size={14} />
                <span>
                  Copy this now. Scarab keeps only its SHA-256, so this string cannot be
                  recovered from the database, from a backup, or by any later API call. A lost
                  token is re-minted, never re-read.
                </span>
              </p>
              <code class="tok-secret mono">{m().token}</code>
            </div>
          )}
        </Show>
        <Show when={minted()}>
          {(m) => (
            <div class="modal-foot">
              <div class="modal-actions">
                <button class="btn btn-primary" onClick={copy}>
                  {copied() ? "Copied" : "Copy token"}
                </button>
                <button class="btn btn-ghost" onClick={props.onClose}>
                  {copied() ? "Done" : "I've copied it"}
                </button>
              </div>
              <p class="subtle modal-note">
                <small>
                  Send it as <code class="mono">Authorization: Bearer …</code>, or give it to
                  the CLI as <code class="mono">SCARAB_TOKEN</code>. It acts as{" "}
                  <b>{m().record.owner_subject}</b>, up to <b>{m().record.role}</b> on{" "}
                  <b>{scopeLabel(m().record)}</b>, and expires {absTime(m().record.expires_at)}.
                </small>
              </p>
            </div>
          )}
        </Show>

        {/* Pinned below the scrolling fields; once the token exists the footer
            above carries the reveal's own actions instead. */}
        <Show when={!minted()}>
          <div class="modal-foot">
            <Show when={lifetimeProblem()}>
              <p class="error">{lifetimeProblem()}</p>
            </Show>
            <Show when={error()}>
              <p class="error">{error()}</p>
            </Show>
            <div class="modal-actions">
              <button class="btn btn-primary" disabled={!ready() || saving()} onClick={mint}>
                <Icon icon="plus" size={14} /> {saving() ? "Minting…" : "Mint token"}
              </button>
              <button class="btn btn-ghost" onClick={props.onClose}>
                Cancel
              </button>
            </div>
          </div>
        </Show>
      </div>
    </div>
  );
}
