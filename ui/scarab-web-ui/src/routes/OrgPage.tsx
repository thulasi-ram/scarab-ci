// The org page (/orgs/<org>): a few counts for the org, and every repository
// its connections reach, each enabled or not.
//
// Enabling a repository is opt-in for every forge. Installing the GitHub App
// or adding a Forgejo connection makes repositories available; Enable binds
// one, which creates its project, and Disable unbinds it, which removes the
// project with its environments, secrets and access grants. Nothing the forge
// sends enables a repository again, so a disabled one stays disabled.
//
// The list is paged on the server: it asks each connection's forge for its
// repositories on every load and merges them with the enabled ones. Who may
// enable or disable each row comes from the server (`can_change`), the same
// rule the bind and unbind endpoints apply.
import { For, Show, createResource, createSignal } from "solid-js";
import { A, useParams } from "@solidjs/router";
import { bindRepo, listOrgRepos, listRuns, unbindRepo, type OrgRepo } from "../api/client";
import { clampPage, orgRunStats, pageCount, pageSpan } from "../org-page";
import { FIRST_ORG_SECTION, decodeParam, orgHref } from "../settings-nav";
import { me } from "../session";
import ConfirmDestructive from "../components/ConfirmDestructive";
import Doodle from "../components/Doodle";
import Icon from "../components/Icon";

/** Repositories per page. */
const PER_PAGE = 25;
/** How many of the most recent runs the counts are taken from (the server's cap). */
const RUNS_LIMIT = 200;

export default function OrgPage() {
  const params = useParams<{ org: string }>();
  const org = () => decodeParam(params.org);

  const [page, setPage] = createSignal(1);
  const [query, setQuery] = createSignal("");
  const [epoch, setEpoch] = createSignal(0);
  const [repos] = createResource(
    () => ({ org: org(), page: page(), q: query(), epoch: epoch() }),
    (k) => listOrgRepos(k.org, { page: k.page, perPage: PER_PAGE, q: k.q }),
  );
  const [runs] = createResource(() => listRuns(RUNS_LIMIT));
  const stats = () => orgRunStats(runs() ?? [], org(), Date.now(), RUNS_LIMIT);
  const reload = () => setEpoch((e) => e + 1);

  const total = () => repos()?.total ?? 0;
  const pages = () => pageCount(total(), PER_PAGE);
  const span = () => pageSpan(page(), PER_PAGE, total());
  const go = (p: number) => setPage(clampPage(p, total(), PER_PAGE));
  const search = (q: string) => {
    setQuery(q);
    setPage(1);
  };
  const administers = () =>
    repos()?.can_administer === true || (me()?.admin_orgs ?? []).includes(org());

  const [busy, setBusy] = createSignal<string | null>(null);
  const [note, setNote] = createSignal<string | null>(null);
  const [error, setError] = createSignal<string | null>(null);
  const [disabling, setDisabling] = createSignal<OrgRepo | null>(null);

  const enable = async (r: OrgRepo) => {
    setBusy(`${r.owner}/${r.name}`);
    setNote(null);
    setError(null);
    try {
      const res = await bindRepo(r.connection_id, { owner: r.owner, name: r.name });
      setNote(
        res.webhook_error
          ? `Enabled ${res.org}/${res.project}, but its webhook could not be registered: ${res.webhook_error}. Pushes will not start runs until it is (Connections, Webhook).`
          : `Enabled ${res.org}/${res.project}.`,
      );
      reload();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not enable the repository.");
    } finally {
      setBusy(null);
    }
  };

  const disable = async (r: OrgRepo) => {
    await unbindRepo(r.connection_id, r.owner, r.name);
    setNote(`Disabled ${r.owner}/${r.name}.`);
    reload();
  };

  return (
    <section class="page">
      <Doodle icon="boxes" size={230} rotate={10} opacity={0.16} top="52px" right="48px" />

      <div class="page-head">
        <h1>{org()}</h1>
      </div>
      <div class="page-toolbar">
        <Show when={administers()}>
          <A href={orgHref(org(), FIRST_ORG_SECTION)} class="btn btn-ghost">
            <Icon icon="settings" size={14} /> Organization settings
          </A>
        </Show>
      </div>

      {/* ── A few counts ─────────────────────────────────────────────── */}
      <div class="dash-label">overview</div>
      <div class="org-stats">
        <div class="org-stat">
          <span class="org-stat-n mono">
            <Show when={repos()} fallback="–">{(p) => p().enabled_total}</Show>
          </span>
          <span class="org-stat-label">enabled repositories</span>
        </div>
        <div class="org-stat">
          <span class="org-stat-n mono">
            <Show when={runs()} fallback="–">
              {stats().partial ? "≥" : ""}
              {stats().lastDay}
            </Show>
          </span>
          <span class="org-stat-label">runs, last 24 hours</span>
          <Show when={runs()}>
            <span class="org-stat-sub mono">
              {stats().partial ? "≥" : ""}
              {stats().lastWeek} in 7 days
            </span>
          </Show>
        </div>
        <div class="org-stat" classList={{ "org-stat-bad": stats().failedLastDay > 0 }}>
          <span class="org-stat-n mono">
            <Show when={runs()} fallback="–">{stats().failedLastDay}</Show>
          </span>
          <span class="org-stat-label">failed, last 24 hours</span>
        </div>
        <A href="/" class="org-stat org-stat-link" classList={{ "org-stat-warn": stats().waiting > 0 }}>
          <span class="org-stat-n mono">
            <Show when={runs()} fallback="–">{stats().waiting}</Show>
          </span>
          <span class="org-stat-label">waiting on a gate</span>
        </A>
      </div>
      <Show when={runs.error}>
        <p class="error">Could not load runs.</p>
      </Show>

      {/* ── Repositories ─────────────────────────────────────────────── */}
      <div class="dash-label repos-label">
        repositories <span class="count">· {total()}</span>
        <div class="repo-search">
          <Icon icon="search" size={13} />
          <input
            type="text"
            placeholder="Filter by name…"
            aria-label="Filter repositories by name"
            value={query()}
            onInput={(e) => search(e.currentTarget.value)}
          />
        </div>
      </div>
      <p class="subtle org-repos-help">
        <small>
          Enabling a repository creates its project, so pushes to it start runs. Disabling removes
          the project with its environments, secrets and access grants; the forge does not add it
          back.
        </small>
      </p>

      <Show when={repos.error}>
        <p class="error">
          {repos.error instanceof Error ? repos.error.message : "Could not load repositories."}
        </p>
      </Show>
      <Show when={note()}>
        <p class="subtle"><small>{note()}</small></p>
      </Show>
      <Show when={error()}>
        <p class="error">{error()}</p>
      </Show>
      <For each={repos()?.unlisted ?? []}>
        {(u) => (
          <p class="subtle">
            <small>
              <Icon icon="alert-triangle" size={12} /> Connection{" "}
              <code class="mono">{u.connection_id}</code> could not list its repositories:{" "}
              {u.reason}
            </small>
          </p>
        )}
      </For>

      <Show when={repos()} fallback={<Show when={!repos.error}><p class="empty">loading…</p></Show>}>
        {(p) => (
          <Show
            when={p().repos.length > 0}
            fallback={
              <p class="empty">
                <Show
                  when={query()}
                  fallback={
                    <>
                      No repositories yet. Connect a forge to <span class="mono">{org()}</span>{" "}
                      in{" "}
                      <A href={orgHref(org(), "connections")}>Connections</A>: for GitHub, install
                      the Scarab App on the account.
                    </>
                  }
                >
                  No repositories match.
                </Show>
              </p>
            }
          >
            <div class="repo-list">
              <For each={p().repos}>
                {(r) => (
                  <div class="repo-row org-repo-row">
                    <Icon icon="git-branch" size={14} class="rr-ico" />
                    <Show
                      when={r.enabled && r.project}
                      fallback={<span class="repo-name org-repo-off">{r.name}</span>}
                    >
                      {(project) => (
                        <A href={`/${org()}/${project()}`} class="repo-name org-repo-link">
                          {r.name}
                        </A>
                      )}
                    </Show>
                    <span
                      class="org-repo-state"
                      classList={{ "org-repo-on": r.enabled }}
                    >
                      {r.enabled ? "Enabled" : "Not enabled"}
                    </span>
                    <span class="rr-coord mono" title={`through connection ${r.connection_id}`}>
                      {r.kind}
                    </span>
                    <a
                      class="rr-coord mono org-repo-forge"
                      href={r.repo_url}
                      target="_blank"
                      rel="noreferrer"
                    >
                      {r.owner}/{r.name}
                    </a>
                    <span class="topbar-spacer" />
                    <Show when={r.can_change}>
                      <Show
                        when={r.enabled}
                        fallback={
                          <button
                            class="btn btn-primary btn-sm"
                            disabled={busy() != null}
                            onClick={() => void enable(r)}
                          >
                            {busy() === `${r.owner}/${r.name}` ? "Enabling…" : "Enable"}
                          </button>
                        }
                      >
                        <button
                          class="btn btn-danger btn-sm"
                          disabled={busy() != null}
                          onClick={() => setDisabling(r)}
                        >
                          Disable
                        </button>
                      </Show>
                    </Show>
                  </div>
                )}
              </For>
            </div>
            <div class="org-pager">
              <span class="subtle mono">
                {span().from}–{span().to} of {total()}
              </span>
              <span class="topbar-spacer" />
              <button
                class="btn btn-ghost btn-sm"
                disabled={page() <= 1 || repos.loading}
                onClick={() => go(page() - 1)}
              >
                Previous
              </button>
              <span class="subtle mono">
                page {page()} of {pages()}
              </span>
              <button
                class="btn btn-ghost btn-sm"
                disabled={page() >= pages() || repos.loading}
                onClick={() => go(page() + 1)}
              >
                Next
              </button>
            </div>
          </Show>
        )}
      </Show>

      <Show when={disabling()}>
        {(r) => (
          <ConfirmDestructive
            eyebrow={`${org()} · ${r().connection_id}`}
            title={`Disable ${r().owner}/${r().name}?`}
            purpose="Its project is removed, with its environments, secrets and access grants."
            name={`${r().owner}/${r().project ?? r().name}`}
            confirmLabel="Disable repository"
            onConfirm={() => disable(r())}
            onClose={() => setDisabling(null)}
          >
            <p>
              Pushes to it stop starting runs. The repository on the forge is untouched, and no
              installation event or re-sync enables it again. Enabling it later creates a new
              project without the old one's settings.
            </p>
          </ConfirmDestructive>
        )}
      </Show>
    </section>
  );
}
