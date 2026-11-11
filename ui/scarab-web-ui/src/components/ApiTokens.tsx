// Issued API tokens (git-bug a4cb94f). A token belongs to the person who
// minted it, so there are two sections:
//
// - Account settings › API tokens (`MyApiTokens`): mint (top right), list and
//   revoke your own.
// - Organization settings › API tokens (`OrgApiTokens`): every token issued in
//   the org, whoever holds it, sorted by last use, calls or creation. An admin
//   revokes any of them here; nobody mints here.
//
// Both list one row per token with the same table (`TokenTable`); the org's
// adds the owner. A token has no page of its own: the row holds all there is.
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
  Show,
  Switch,
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
  activeCount,
  expiresSoon,
  expiryLabel,
  lifetimeError,
  MAX_TOKEN_DAYS,
  mintableOrgs,
  ROLE_CHOICES,
  scopeLabel,
  sortTokens,
  TOKEN_SORTS,
  tokenChips,
  tokenState,
  type TokenRole,
} from "../api-tokens";
import { accountHref } from "../settings-nav";
import { relTime, absTime } from "../fmt";
import Chips from "./Chips";
import Icon from "./Icon";
import Modal, { ModalMessage, ModalSection } from "./Modal";
import ConfirmDestructive from "./ConfirmDestructive";
import { Field, Input, Select } from "./Field";
import Pill from "./Pill";

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

/** "3 active", beside a list's heading; nothing when none is. */
function ActiveCount(props: { tokens: readonly ApiToken[] }) {
  const n = () => activeCount(props.tokens, Date.now());
  return (
    <Show when={n() > 0}>
      <span class="tok-count">{n()} active</span>
    </Show>
  );
}

/** `null` is the deployment answering "I have no token store", which is not
 *  the same sentence as "there are no tokens". */
const noStore = (r: { loading: boolean; error: unknown; latest?: ApiToken[] | null }) =>
  !r.loading && !r.error && r.latest === null;

/** Account settings › API tokens: the tokens you hold, in every org. */
export function MyApiTokens() {
  const [epoch, setEpoch] = createSignal(0);
  const [tokens] = createResource(epoch, () => listMyApiTokens());
  const [minting, setMinting] = createSignal(false);
  const [revoking, setRevoking] = createSignal<string | null>(null);
  const reload = () => setEpoch((e) => e + 1);

  const unavailable = () => noStore(tokens);
  const rows = () => sortTokens(tokens() ?? [], Date.now());

  const revokingToken = () => rows().find((t) => t.id === revoking());
  // A refusal shows in the confirm dialog.
  const revoke = async (t: ApiToken) => {
    await revokeMyApiToken(t.id);
    reload();
  };

  return (
    <div class="panel">
      <div class="panel-h">
        <span>
          Your API tokens <ActiveCount tokens={rows()} />
        </span>
        <Show when={!unavailable()}>
          <button class="btn btn-primary btn-sm" onClick={() => setMinting(true)}>
            <Icon icon="plus" size={14} /> New token
          </button>
        </Show>
      </div>
      <div class="secrets-body">
        <Show when={tokens.error}>
          <p class="error">Could not load your API tokens.</p>
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
              <TokenTable label="Your API tokens" tokens={rows()} onRevoke={(t) => setRevoking(t.id)} />
            </Show>
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
      <Show when={revokingToken()}>
        {(t) => (
          <RevokeToken token={t()} onConfirm={() => revoke(t())} onClose={() => setRevoking(null)} />
        )}
      </Show>
    </div>
  );
}

/**
 * One row per token: its name with its state tags, its owner (`showOwner`,
 * the org's list), its scope and ceiling, last use, calls, creation and expiry,
 * and Revoke while it is live. `sort` marks the column the rows are sorted
 * by.
 */
function TokenTable(props: {
  label: string;
  tokens: readonly ApiToken[];
  showOwner?: boolean;
  sort?: TokenSort;
  onRevoke: (t: ApiToken) => void;
}) {
  const sorted = (s: TokenSort) => (props.sort === s ? "descending" : undefined);
  return (
    <div class="tok-table-scroll">
      <table class="table tok-table" aria-label={props.label}>
        <thead>
          <tr>
            <th>Name</th>
            <Show when={props.showOwner}>
              <th>Owner</th>
            </Show>
            <th>Scope</th>
            <th aria-sort={sorted("last_used")}>Last used</th>
            <th aria-sort={sorted("use_count")}>Calls</th>
            <th aria-sort={sorted("created")}>Created</th>
            <th>Expires</th>
            <th />
          </tr>
        </thead>
        <tbody>
          <For each={props.tokens}>
            {(t) => <TokenRow token={t} showOwner={props.showOwner} onRevoke={() => props.onRevoke(t)} />}
          </For>
        </tbody>
      </table>
    </div>
  );
}

/** One token. Last use and calls are written back at most once a minute
 *  server-side, so both are approximate. */
function TokenRow(props: { token: ApiToken; showOwner?: boolean; onRevoke: () => void }) {
  const t = () => props.token;
  const now = Date.now();
  const state = () => tokenState(t(), now);
  return (
    <tr classList={{ "tok-dead": state() !== "live" }}>
      <td class="keep">
        <span class="tok-name">{t().name}</span>
        <Chips chips={tokenChips(t(), now)} />
      </td>
      <Show when={props.showOwner}>
        <td>
          <span class="mono">{t().owner.handle}</span>
        </td>
      </Show>
      <td>
        <span class="mono">{scopeLabel(t())}</span>{" "}
        <Pill tone="copper">up to {t().role}</Pill>
      </td>
      <td class="nw" title={t().last_used_at ? absTime(t().last_used_at!) : undefined}>
        {t().last_used_at ? relTime(t().last_used_at!) : "never"}
      </td>
      <td class="nw mono">{t().use_count.toLocaleString("en-US")}</td>
      <td class="nw" title={absTime(t().created_at)}>
        {relTime(t().created_at)}
      </td>
      <td class="nw" title={absTime(t().revoked_at ?? t().expires_at)}>
        <Show
          when={state() === "revoked"}
          fallback={
            <span class={expiresSoon(t(), now) ? "tok-soon" : undefined}>{expiryLabel(t(), now)}</span>
          }
        >
          revoked {relTime(t().revoked_at!)}
        </Show>
      </td>
      <td class="nw">
        <Show when={state() === "live"}>
          <button class="btn btn-danger btn-sm" onClick={props.onRevoke}>
            Revoke
          </button>
        </Show>
      </td>
    </tr>
  );
}

/** The revoke confirm: what stops working, and the token's name typed exactly. */
function RevokeToken(props: { token: ApiToken; onConfirm: () => Promise<void>; onClose: () => void }) {
  return (
    <ConfirmDestructive
      title={`Revoke ${props.token.name}?`}
      purpose="The next request that presents it is refused."
      name={props.token.name}
      confirmLabel="Revoke token"
      onConfirm={props.onConfirm}
      onClose={props.onClose}
    >
      <p>
        Held by <span class="mono">{props.token.owner.handle}</span>. Anything still using it (a
        CI job, a script, an agent) starts failing, and the token cannot be restored, only
        replaced.
      </p>
    </ConfirmDestructive>
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
  const reload = () => setEpoch((e) => e + 1);

  const unavailable = () => noStore(tokens);
  // The server's order, as asked for: a stale row keeps its place, so the list
  // does not move under the cursor between refetches.
  const rows = () => tokens() ?? [];
  const revokingToken = () => rows().find((t) => t.id === revoking());

  // A refusal shows in the confirm dialog.
  const revoke = async (t: ApiToken) => {
    await revokeApiToken(props.org, t.id);
    reload();
  };

  return (
    <div class="panel">
      <div class="panel-h">
        <span>
          API tokens · {props.org} <ActiveCount tokens={rows()} />
        </span>
      </div>
      <div class="secrets-body">
        <p class="subtle">
          Every token issued in {props.org}, whoever holds it. Tokens are personal: each
          person mints their own under{" "}
          <A href={accountHref("tokens")}>Account settings › API tokens</A>.
        </p>
        <Show when={tokens.error}>
          <p class="error">Could not load API tokens.</p>
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
              <TokenTable
                label={`API tokens issued in ${props.org}`}
                tokens={rows()}
                showOwner
                sort={sort()}
                onRevoke={(t) => setRevoking(t.id)}
              />
            </Show>
          </Show>
          <Show when={revokingToken()}>
            {(t) => (
              <RevokeToken token={t()} onConfirm={() => revoke(t())} onClose={() => setRevoking(null)} />
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

  // Escape and the scrim dismiss the form and nothing else: once the
  // plaintext is on screen, the reflex that closes a modal is the reflex that
  // loses it, so the dialog counts as busy until "I've copied it".

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
    <Modal
      title={minted() ? "Your new token" : "New API token"}
      purpose={
        minted()
          ? "Shown once. Copy it before you close this dialog."
          : "A token lets something that is not a browser talk to Scarab as you."
      }
      busy={saving() || !!minted()}
      onClose={props.onClose}
      footer={
        <Show
          when={minted()}
          fallback={
            <>
              <Show when={error()}>
                <ModalMessage>{error()}</ModalMessage>
              </Show>
              <button class="btn btn-ghost" type="button" onClick={props.onClose} disabled={saving()}>
                Cancel
              </button>
              <button class="btn btn-primary" type="button" disabled={!ready() || saving()} onClick={mint}>
                <Icon icon="plus" size={14} /> {saving() ? "Minting…" : "Mint token"}
              </button>
            </>
          }
        >
          <button class="btn btn-ghost" type="button" onClick={props.onClose}>
            {copied() ? "Done" : "I've copied it"}
          </button>
          <button class="btn btn-primary" type="button" onClick={copy}>
            {copied() ? "Copied" : "Copy token"}
          </button>
        </Show>
      }
    >
      <Show
        when={minted()}
        fallback={
          <>
            <ModalSection n={1} title="Token">
              <Field label="Name" required help="What makes it revocable later: the only way to tell two tokens apart.">
                <Input
                  autofocus
                  placeholder="release-bot, laptop CLI, demo keepalive"
                  value={name()}
                  onValue={setName}
                />
              </Field>
            </ModalSection>

            <ModalSection n={2} title="Scope">
              <Switch
                fallback={
                  <Field label="Organization" required>
                    <Input mono placeholder="acme" value={org()} onValue={setOrg} />
                  </Field>
                }
              >
                <Match when={scopes()?.orgs?.length === 0}>
                  <p class="f-help" role="status">
                    You hold no role in any organization, so there is nowhere to mint a token
                    yet. An org admin can grant you one.
                  </p>
                </Match>
                <Match when={(scopes()?.orgs?.length ?? 0) > 0}>
                  <Field label="Organization" required>
                    <Select
                      mono
                      value={org()}
                      onValue={(v) => {
                        setOrg(v);
                        setProject("");
                      }}
                    >
                      <For each={scopes()?.orgs ?? []}>{(o) => <option value={o}>{o}</option>}</For>
                    </Select>
                  </Field>
                </Match>
              </Switch>
              <Field
                label="Repository"
                help="A token scoped to one repository cannot see another's runs. Narrow it unless it needs the whole org."
              >
                <Select mono value={project()} onValue={setProject}>
                  <option value="">All of {org() || "the org"}</option>
                  <For each={orgProjects()}>
                    {(p) => (
                      <option value={p}>
                        {org()}/{p}
                      </option>
                    )}
                  </For>
                </Select>
              </Field>
            </ModalSection>

            <ModalSection n={3} title="Limits">
              <Field
                label="Role ceiling"
                help="A ceiling, not a grant: each request also checks what you hold at that moment and takes the lower of the two. You cannot mint above your own role in this scope."
              >
                <Select value={role()} onValue={(v) => setRole(v as TokenRole)}>
                  <For each={ROLE_CHOICES}>
                    {(r) => (
                      <option value={r.role}>
                        up to {r.role}: {r.what}
                      </option>
                    )}
                  </For>
                </Select>
              </Field>
              <Field
                label="Expires in"
                required
                help={`Required, 1–${MAX_TOKEN_DAYS}. There is no "never": Scarab does not mint a credential that carries a role and never expires.`}
                error={lifetimeProblem()}
              >
                <div class="tok-life">
                  <Input
                    mono
                    class="tok-days"
                    inputmode="numeric"
                    suffix="days"
                    value={String(days())}
                    onValue={(v) => setDays(Number(v))}
                  />
                  <For each={[30, 90, 365]}>
                    {(d) => (
                      <button
                        class="btn btn-ghost btn-sm"
                        type="button"
                        classList={{ "tok-preset-on": days() === d }}
                        onClick={() => setDays(d)}
                      >
                        {d}d
                      </button>
                    )}
                  </For>
                </div>
              </Field>
            </ModalSection>
          </>
        }
      >
        {(m) => (
          <>
            <p class="tok-warn">
              <Icon icon="alert-triangle" size={14} />
              <span>
                Copy this now. Scarab keeps only its SHA-256, so this string cannot be
                recovered from the database, from a backup, or by any later API call. A lost
                token is re-minted, never re-read.
              </span>
            </p>
            <code class="tok-secret mono">{m().token}</code>
            <p class="dlg-text">
              Send it as <code class="mono">Authorization: Bearer …</code>, or give it to the CLI as{" "}
              <code class="mono">SCARAB_TOKEN</code>. It acts as <b>{m().record.owner.handle}</b>, up
              to <b>{m().record.role}</b> on <b>{scopeLabel(m().record)}</b>, and expires{" "}
              {absTime(m().record.expires_at)}.
            </p>
          </>
        )}
      </Show>
    </Modal>
  );
}
