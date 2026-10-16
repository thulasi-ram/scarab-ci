// Settings › Members (ADR-0049 roles, ADR-0069 identities): who holds which
// role in the org, granting and revoking natively, and re-syncing a repo's
// collaborators from its forge.
//
// Until now there was no UI for role bindings at all — only raw HTTP. Two
// sources write them and the panel keeps them apart in the operator's head:
// a NATIVE grant/revoke here is authoritative, and a forge re-sync only ever
// seeds and refreshes what it seeded itself. "Not signed in yet" marks a user
// a re-sync created from their forge account: whoever signs in with exactly
// that account gets these roles, and nobody else can.
import { For, Show, createResource, createSignal } from "solid-js";
import {
  grantRole,
  importFromForge,
  listBindings,
  listProjects,
  revokeRole,
} from "../api/client";
import { memberRows } from "../identities";
import Icon from "./Icon";
import PrincipalSearch from "./PrincipalSearch";

const ROLES = ["viewer", "member", "admin", "owner"];

export default function Members(props: { org: string }) {
  const [bindings, { refetch }] = createResource(() => listBindings(props.org));
  const [projects] = createResource(async () =>
    (await listProjects().catch(() => []))
      .filter((p) => p.org === props.org)
      .map((p) => p.project)
      .sort(),
  );
  const [error, setError] = createSignal<string | null>(null);
  const [notice, setNotice] = createSignal<string | null>(null);
  const [subject, setSubject] = createSignal("");
  const [role, setRole] = createSignal("member");
  const [project, setProject] = createSignal("");
  const [syncRepo, setSyncRepo] = createSignal("");
  const [busy, setBusy] = createSignal(false);

  const rows = () => memberRows(bindings() ?? []);

  const act = async (what: () => Promise<string | void>) => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const msg = await what();
      if (msg) setNotice(msg);
      await refetch();
    } catch (e) {
      setError(e instanceof Error ? e.message : "That did not work.");
    } finally {
      setBusy(false);
    }
  };

  const grant = () =>
    act(async () => {
      await grantRole(props.org, {
        subject: subject(),
        role: role(),
        ...(project() ? { project: project() } : {}),
      });
      setSubject("");
    });

  const revoke = (s: string, p: string) => {
    if (
      !window.confirm(
        `Revoke ${s}'s role on ${p || `all of ${props.org}`}?\n\nA native revoke is durable: a later forge re-sync will not give it back.`,
      )
    )
      return;
    void act(() => revokeRole(props.org, s, p || undefined));
  };

  const resync = () =>
    act(async () => {
      const seeded = await importFromForge(props.org, syncRepo());
      return `Re-synced ${props.org}/${syncRepo()} from its forge: ${seeded.length} collaborator${seeded.length === 1 ? "" : "s"} hold roles from it now.`;
    });

  return (
    <div class="panel">
      <div class="panel-h">
        <span>Members · {props.org}</span>
        <Show when={rows().length > 0}>
          <span class="tok-count">{rows().length} people</span>
        </Show>
      </div>
      <div class="secrets-body">
        <Show when={bindings.error}>
          <p class="error">Could not load members.</p>
        </Show>
        <Show when={error()}>
          <p class="error">{error()}</p>
        </Show>
        <Show when={notice()}>
          <p class="notice-ok">{notice()}</p>
        </Show>

        <Show when={!bindings.loading} fallback={<p class="empty">loading…</p>}>
          <Show
            when={bindings() !== null}
            fallback={
              <p class="empty">
                This deployment has no role store — authorization is off (dev), so there is
                nobody to grant anything to.
              </p>
            }
          >
            <Show
              when={rows().length > 0}
              fallback={
                <p class="empty">
                  Nobody holds a role in {props.org} yet. Re-sync a repository from its
                  forge to bring in its collaborators, or grant someone below.
                </p>
              }
            >
              <ul class="tok-list">
                <For each={rows()}>
                  {(r) => (
                    <li class="tok">
                      <div class="tok-head">
                        <Icon icon="user" size={15} />
                        <span class="tok-name">{r.display}</span>
                        <span class="tok-scope mono">{r.subject}</span>
                        <Show when={r.pending}>
                          <span class="tok-state tok-expired" title="Imported from the forge; becomes theirs when they sign in with that forge account">
                            not signed in yet
                          </span>
                        </Show>
                      </div>
                      <div class="tok-facts">
                        <Show when={r.orgRole}>
                          <span>
                            <span class="tok-role">{r.orgRole}</span> all of {props.org}
                            <button class="btn btn-ghost btn-sm" disabled={busy()} onClick={() => revoke(r.subject, "")}>
                              revoke
                            </button>
                          </span>
                        </Show>
                        <For each={r.projects}>
                          {(p) => (
                            <span>
                              <span class="tok-role">{p.role}</span> {p.project}
                              <button class="btn btn-ghost btn-sm" disabled={busy()} onClick={() => revoke(r.subject, p.project)}>
                                revoke
                              </button>
                            </span>
                          )}
                        </For>
                      </div>
                    </li>
                  )}
                </For>
              </ul>
            </Show>

            <div class="form-r">
              <label>grant</label>
              <Show
                when={subject()}
                fallback={<PrincipalSearch org={props.org} onPick={setSubject} />}
              >
                <div class="tok-life">
                  <span class="tok-scope mono">{subject()}</span>
                  <select class="input" value={role()} onChange={(e) => setRole(e.currentTarget.value)}>
                    <For each={ROLES}>{(r) => <option value={r}>{r}</option>}</For>
                  </select>
                  <select class="input" value={project()} onChange={(e) => setProject(e.currentTarget.value)}>
                    <option value="">all of {props.org}</option>
                    <For each={projects() ?? []}>{(p) => <option value={p}>{p}</option>}</For>
                  </select>
                  <button class="btn btn-primary btn-sm" disabled={busy()} onClick={grant}>
                    grant
                  </button>
                  <button class="btn btn-ghost btn-sm" onClick={() => setSubject("")}>
                    cancel
                  </button>
                </div>
              </Show>
            </div>

            <div class="form-r">
              <label>re-sync from forge</label>
              <div class="tok-life">
                <select class="input" value={syncRepo()} onChange={(e) => setSyncRepo(e.currentTarget.value)}>
                  <option value="">choose a repository…</option>
                  <For each={projects() ?? []}>{(p) => <option value={p}>{p}</option>}</For>
                </select>
                <button class="btn btn-ghost btn-sm" disabled={busy() || !syncRepo()} onClick={resync}>
                  <Icon icon="rotate-cw" size={13} /> re-sync
                </button>
              </div>
              <p class="fieldnote">
                Seeds roles from the repository's forge collaborators — admin → admin, write →
                member, read → viewer — pinned to each person's forge account, and removes the
                forge-given role of anyone the forge no longer lists. Native grants and
                revokes made here are never touched.
              </p>
            </div>
          </Show>
        </Show>
      </div>
    </div>
  );
}
