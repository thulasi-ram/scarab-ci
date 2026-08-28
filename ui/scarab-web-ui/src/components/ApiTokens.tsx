// Issued API tokens (ADR-0049) — the Settings section for the credential a
// machine can hold. Mint, see what is outstanding, revoke.
//
// The whole section is built around one property of the server: the plaintext
// exists in exactly one response and nowhere else, because Scarab stores only a
// SHA-256 of it. So there is no "show token" affordance anywhere in this file,
// not because it was left out but because nothing could implement it — a token
// that is not copied out of the mint dialog is re-minted, never recovered. The
// dialog says so, and refuses to close by accident while the secret is on screen.
//
// The other thing this surface must not misreport is AUTHORITY. A token's `role`
// is a **ceiling**, not a grant: every request takes the lower of that ceiling
// and whatever the owner holds at that moment, so demoting the owner demotes
// their tokens in the same instant. Every place a role appears here says "up to"
// for that reason — a row reading "admin" flat would be a promise the server
// never made.
import { createResource, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import {
  listApiTokens,
  listProjects,
  mintApiToken,
  revokeApiToken,
  type ApiToken,
  type MintedToken,
} from "../api/client";
import {
  expiresSoon,
  expiryLabel,
  isLive,
  lifetimeError,
  MAX_TOKEN_DAYS,
  ROLE_CHOICES,
  scopeLabel,
  sortTokens,
  tokenState,
  type TokenRole,
} from "../api-tokens";
import { relTime, absTime } from "../fmt";
import Icon from "./Icon";

/** The lifetime a token gets unless someone says otherwise. A quarter: long
 * enough that rotating is not a weekly chore, short enough that an abandoned
 * credential dies on its own. */
const DEFAULT_DAYS = 90;

export default function ApiTokens(props: { org: string }) {
  const [epoch, setEpoch] = createSignal(0);
  const [tokens] = createResource(
    () => ({ org: props.org, epoch: epoch() }),
    (k) => listApiTokens(k.org),
  );
  const [minting, setMinting] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const reload = () => setEpoch((e) => e + 1);

  // `null` from the client is the deployment answering "I have no token store",
  // which is a different sentence from "you have no tokens" and must not be
  // rendered as one.
  const unavailable = () => !tokens.loading && !tokens.error && tokens() === null;
  const rows = () => sortTokens(tokens() ?? [], Date.now());
  const liveCount = () => rows().filter((t) => isLive(t, Date.now())).length;

  const revoke = async (t: ApiToken) => {
    if (
      !window.confirm(
        `Revoke "${t.name}"?\n\nThis is permanent and takes effect on the very next request that presents it. Anything still using this token — a CI job, a script, an agent — starts failing immediately, and the credential cannot be restored, only replaced.`,
      )
    )
      return;
    setError(null);
    try {
      await revokeApiToken(props.org, t.id);
      reload();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not revoke the token.");
    }
  };

  return (
    <div class="panel">
      <div class="panel-h">
        <span>API tokens · {props.org}</span>
        <Show when={liveCount() > 0}>
          <span class="tok-count">
            {liveCount()} active
          </span>
        </Show>
      </div>
      <div class="secrets-body">
        <Show when={tokens.error}>
          <p class="error">Could not load API tokens.</p>
        </Show>
        <Show when={error()}>
          <p class="error">{error()}</p>
        </Show>

        <Show when={!tokens.loading} fallback={<p class="empty">loading…</p>}>
          <Show
            when={!unavailable()}
            fallback={
              <p class="empty">
                This deployment cannot issue API tokens — it is running without the
                database-backed token store. Tokens are stored hashed in Postgres; a
                server with no store has nowhere to keep them.
              </p>
            }
          >
            <Show
              when={rows().length > 0}
              fallback={
                <p class="empty">
                  No tokens issued yet. An API token is what lets something that is not a
                  browser talk to Scarab — the CLI, a CI job on another system, a status
                  poller, an agent driving runs.
                </p>
              }
            >
              <ul class="tok-list">
                <For each={rows()}>{(t) => <TokenRow token={t} onRevoke={() => revoke(t)} />}</For>
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
                once, at mint. A token's role is a ceiling — each request also re-checks
                what its owner holds right now, so revoking a person's access disarms
                their tokens too. Tokens cannot mint tokens.
              </small>
            </p>
          </Show>
        </Show>
      </div>

      <Show when={minting()}>
        <MintTokenDialog
          org={props.org}
          onClose={() => setMinting(false)}
          onMinted={reload}
        />
      </Show>
    </div>
  );
}

/** One issued token: what it is, what it may do, and whether it is still alive. */
function TokenRow(props: { token: ApiToken; onRevoke: () => void }) {
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
            revoke
          </button>
        </Show>
      </div>
      <div class="tok-facts">
        {/* Last use is the fact that makes a token revocable in practice: nobody
            dares kill a credential when they cannot tell whether anything is
            still holding it. Written back at most once a minute server-side, so
            it is deliberately reported as an approximation. */}
        <span title={t().last_used_at ? absTime(t().last_used_at!) : undefined}>
          <Icon icon="clock" size={12} />
          {t().last_used_at ? `last used ${relTime(t().last_used_at!)}` : "never used"}
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
          {t().owner_subject}
          {t().created_by !== t().owner_subject ? ` (issued by ${t().created_by})` : ""} ·{" "}
          {relTime(t().created_at)}
        </span>
      </div>
    </li>
  );
}

/**
 * Mint a token, then reveal it exactly once.
 *
 * Two states in one modal rather than two dialogs: the second is the *result* of
 * the first, and handing the user a fresh scrim to dismiss is how a secret gets
 * clicked away. While the plaintext is showing, the scrim and Escape stop
 * closing the dialog — the only way out is the button that says the secret is
 * gone.
 */
function MintTokenDialog(props: { org: string; onClose: () => void; onMinted: () => void }) {
  const [name, setName] = createSignal("");
  const [project, setProject] = createSignal("");
  const [role, setRole] = createSignal<TokenRole>("viewer");
  const [days, setDays] = createSignal(DEFAULT_DAYS);
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [minted, setMinted] = createSignal<MintedToken | null>(null);
  const [copied, setCopied] = createSignal(false);

  // Escape dismisses the FORM and nothing else. Once the plaintext is on screen
  // the reflex that closes a modal is exactly the reflex that loses the
  // credential, so the only way out of that state is a button that says so.
  onMount(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !minted()) props.onClose();
    };
    document.addEventListener("keydown", onKey);
    onCleanup(() => document.removeEventListener("keydown", onKey));
  });

  // The projects of THIS org, so the scope picker offers real narrowings and
  // nobody has to type a project name that may not exist. A failure here is not
  // fatal: the org-wide default still mints.
  const [projects] = createResource(async () =>
    (await listProjects().catch(() => []))
      .filter((p) => p.org === props.org)
      .map((p) => p.project)
      .sort(),
  );

  const lifetimeProblem = () => lifetimeError(days());
  const ready = () => name().trim().length > 0 && lifetimeProblem() === null;

  const mint = async () => {
    if (!ready() || saving()) return;
    setSaving(true);
    setError(null);
    try {
      setMinted(
        await mintApiToken(props.org, {
          name: name().trim(),
          role: role(),
          expires_in_days: days(),
          ...(project() ? { project: project() } : {}),
        }),
      );
      // Refresh the list underneath now, so closing the reveal lands on a list
      // that already contains the new row rather than one that grows a beat later.
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
      {/* `modal-tall`: four labelled fields with their notes overflow a short
          viewport, and the default modal clips rather than scrolls — which hid
          the Mint button entirely at 800px tall. The fields scroll; the actions
          stay pinned where they can always be reached. */}
      <div class="modal modal-tall" onClick={(e) => e.stopPropagation()}>
        <div class="panel-h">
          <span>{minted() ? "Your new token" : "New API token"}</span>
        </div>

        <Show
          when={minted()}
          fallback={
            <div class="modal-body token-form">
              <div class="form-r">
                <label>name</label>
                <input
                  class="input"
                  placeholder="release-bot, amy's laptop, demo keepalive"
                  value={name()}
                  onInput={(e) => setName(e.currentTarget.value)}
                  autofocus
                />
                <p class="fieldnote">
                  What makes it revocable later — the only way to tell two tokens apart.
                </p>
              </div>

              <div class="form-r">
                <label>scope</label>
                <select
                  class="input"
                  value={project()}
                  onChange={(e) => setProject(e.currentTarget.value)}
                >
                  <option value="">All of {props.org}</option>
                  <For each={projects() ?? []}>
                    {(p) => (
                      <option value={p}>
                        {props.org}/{p}
                      </option>
                    )}
                  </For>
                </select>
                <p class="fieldnote">
                  A token scoped to one repository cannot see another's runs, let alone act
                  on them. Narrow it unless it genuinely needs the whole org.
                </p>
              </div>

              <div class="form-r">
                <label>role ceiling</label>
                <select
                  class="input"
                  value={role()}
                  onChange={(e) => setRole(e.currentTarget.value as TokenRole)}
                >
                  <For each={ROLE_CHOICES}>
                    {(r) => (
                      <option value={r.role}>
                        up to {r.role} — {r.what}
                      </option>
                    )}
                  </For>
                </select>
                <p class="fieldnote">
                  A ceiling, not a grant: each request also re-checks what you hold at that
                  moment and takes the lower of the two. You cannot mint above your own
                  role in this scope.
                </p>
              </div>

              <div class="form-r">
                <label>expires in</label>
                <div class="tok-life">
                  <input
                    class="input tok-days"
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
                  Required, 1–{MAX_TOKEN_DAYS}. There is no "never" — a credential that
                  carries no verb and never expires is the one shape this system does not
                  mint.
                </p>
              </div>

            </div>
          }
        >
          {(m) => (
            <div class="modal-body">
              <p class="tok-warn">
                <Icon icon="alert-triangle" size={14} />
                <span>
                  Copy this now. Scarab keeps only its SHA-256, so this string cannot be
                  recovered from the database, from a backup, or by any later API call — a
                  lost token is re-minted, never re-read.
                </span>
              </p>
              <code class="tok-secret mono">{m().token}</code>
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
                  <b>{scopeLabel(m().record)}</b>, and expires{" "}
                  {absTime(m().record.expires_at)}.
                </small>
              </p>
            </div>
          )}
        </Show>

        {/* Pinned below the scrolling fields — and gone entirely once the token
            exists, because at that point the only actions are the reveal's own. */}
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
