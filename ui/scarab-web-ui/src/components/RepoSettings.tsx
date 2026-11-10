// A repository's Settings tab: what the server knows about where the
// repository lives, with Disable for those who may unbind it, then Access,
// the people who hold a role on it.
//
// Disable is the org page's: the same confirmation and unbind call
// (DisableRepository), offered under the rule the unbind endpoint applies:
// the caller administers the org and the connection is not managed by
// configuration. The repository's project (`GET /v1/repos`, which the page
// already loads) names its connection; the panel finds that connection in
// the org's connection list for its health and whether configuration manages
// it. No forge is asked. Once it is disabled the repository has no page, so
// the tab goes to the org page.
//
// A Forgejo repository has its own webhook on the forge, so the panel also
// offers Register webhook: the retry for when enabling could not register it.
// GitHub delivers every repository's events to the App, so there is none.
//
// Access is one table: the repository's own role bindings, each with where it
// came from (granted in Scarab, or seeded from the forge's collaborators by a
// re-sync) and Revoke, then, under a "Through their org role" group row and
// greyed, the people whose org role reaches the repository. Org roles are
// managed under Settings › Members; this panel grants (Grant access, top
// right, in a dialog), revokes and re-syncs at this repository's scope only.
//
// The bindings list and grant/revoke are org-admin actions on the server, so
// the panel shows them to org admins only; anyone else sees a line saying so.
//
// Last, the repository's own Library policies and Plugin policies
// (PolicyLists, the one place a project's lists are edited).
import { For, Show, createResource, createSignal } from "solid-js";
import { A, useNavigate } from "@solidjs/router";
import {
  grantRole,
  importFromForge,
  listBindings,
  listConnections,
  registerRepoWebhook,
  revokeRole,
  type Binding,
  type Project,
} from "../api/client";
import { me } from "../session";
import { administers } from "../settings-nav";
import { repoAccessRows, resyncDiff, type ResyncChange, type RoleSource } from "../identities";
import { forgeName } from "../connections";
import { absTime, relTime } from "../fmt";
import { UNREACHABLE_NOTE, orgPageHref } from "../org-page";
import Icon from "./Icon";
import ConfirmDestructive from "./ConfirmDestructive";
import DisableRepository, { type DisableTarget } from "./DisableRepository";
import Modal, { ModalMessage, ModalSection } from "./Modal";
import { Field, Select } from "./Field";
import PrincipalSearch from "./PrincipalSearch";
import { PersonCell } from "./Members";
import PolicyLists from "./PolicyLists";
import { LIBRARY_KINDS, LIBRARY_POLICIES_ANCHOR, PLUGIN_KINDS, PLUGIN_POLICIES_ANCHOR } from "../policies";

const ROLES = ["viewer", "member", "admin", "owner"];

export default function RepoSettings(props: {
  org: string;
  repo: string;
  /** The repository's project from `GET /v1/repos`; null when it has none. */
  project: Project | null;
}) {
  const canAdmin = () => administers(me(), props.org);
  // Connections are listed to org admins only. A failure leaves the rows out.
  const [connection] = createResource(
    () => (canAdmin() && props.project ? { org: props.org, id: props.project.connection_id } : false),
    async (k) => (await listConnections(k.org).catch(() => [])).find((c) => c.id === k.id) ?? null,
  );
  const forge = () => forgeName(connection()?.kind);
  // What Disable unbinds, when the caller may: the connection is listed only
  // to org admins, and one managed by configuration is read-only here.
  const binding = (): DisableTarget | null => {
    const c = connection();
    const p = props.project;
    if (!c || !p || c.managed_by_config) return null;
    return { connection_id: c.id, owner: p.owner, name: p.name, project: p.project };
  };
  const [disabling, setDisabling] = createSignal(false);
  const navigate = useNavigate();

  const [hooking, setHooking] = createSignal(false);
  const [hookNote, setHookNote] = createSignal<string | null>(null);
  const [hookError, setHookError] = createSignal<string | null>(null);
  const registerWebhook = async (b: DisableTarget) => {
    setHooking(true);
    setHookNote(null);
    setHookError(null);
    try {
      await registerRepoWebhook(b.connection_id, b.owner, b.name);
      setHookNote("Webhook registered.");
    } catch (e) {
      setHookError(e instanceof Error ? e.message : "Could not register the webhook.");
    } finally {
      setHooking(false);
    }
  };

  return (
    <>
      <div class="panel">
        <div class="panel-h"><span>Repository</span></div>
        <div class="settings-body">
          <Show
            when={props.project?.repo_url}
            fallback={<p class="empty">{props.org}/{props.repo} is not bound to a forge connection.</p>}
          >
            {(url) => (
              <div class="set-row">
                <span class="k">Forge</span>
                <a class="mono" href={url()} target="_blank" rel="noopener noreferrer">
                  {url()}
                </a>
              </div>
            )}
          </Show>
          <Show when={connection()}>
            {(c) => (
              <>
                <div class="set-row">
                  <span class="k">Connection</span>
                  <span>
                    {forgeName(c().kind)}{" "}
                    <A class="mono" href={`/orgs/${encodeURIComponent(props.org)}/settings/connections`}>
                      {c().id}
                    </A>
                  </span>
                </div>
                <div class="set-row">
                  <span class="k">Credential</span>
                  <span class={c().credential_present ? "" : "error"}>
                    {c().credential_present ? "present" : "missing"}
                  </span>
                </div>
                <div class="set-row">
                  <span class="k">Last webhook delivery</span>
                  <Show
                    when={c().last_delivery_at}
                    fallback={<span class="subtle">none yet</span>}
                  >
                    {(at) => (
                      <span title={`${absTime(at())}. Recorded per forge kind, across all ${forgeName(c().kind)} connections.`}>
                        {relTime(at())}
                      </span>
                    )}
                  </Show>
                </div>
              </>
            )}
          </Show>
          <Show when={binding()}>
            {(b) => (
              <div class="repo-disable">
                <Show when={connection()?.kind !== "github"}>
                  <button
                    class="btn btn-ghost btn-sm"
                    type="button"
                    disabled={hooking()}
                    title="Register (or register again) this repository's webhook on the forge"
                    onClick={() => void registerWebhook(b())}
                  >
                    <Icon icon="rotate-cw" size={12} /> {hooking() ? "Registering…" : "Register webhook"}
                  </button>
                </Show>
                <button class="btn btn-danger btn-sm" type="button" onClick={() => setDisabling(true)}>
                  Disable repository
                </button>
                <Show when={props.project?.unreachable_since != null}>
                  <span class="repo-unreachable-note">
                    <Icon icon="alert-triangle" size={12} /> {UNREACHABLE_NOTE}
                  </span>
                </Show>
              </div>
            )}
          </Show>
          <Show when={hookNote()}>
            <p class="subtle"><small>{hookNote()}</small></p>
          </Show>
          <Show when={hookError()}>
            <p class="error">{hookError()}</p>
          </Show>
        </div>
      </div>
      <Show when={disabling() ? binding() : null}>
        {(r) => (
          <DisableRepository
            org={props.org}
            repo={r()}
            onDisabled={() => navigate(orgPageHref(props.org))}
            onClose={() => setDisabling(false)}
          />
        )}
      </Show>

      <Show
        when={canAdmin()}
        fallback={
          <div class="panel">
            <div class="panel-h"><span>Access</span></div>
            <div class="settings-body">
              <p class="empty">Only an admin of {props.org} can see and change who has access to {props.repo}.</p>
            </div>
          </div>
        }
      >
        <RepoAccess org={props.org} repo={props.repo} forge={forge()} />
      </Show>

      <PolicyLists
        org={props.org}
        project={props.repo}
        kinds={LIBRARY_KINDS}
        anchor={LIBRARY_POLICIES_ANCHOR}
        oldAnchor="project-lists"
        title="Library policies"
      />
      <PolicyLists
        org={props.org}
        project={props.repo}
        kinds={PLUGIN_KINDS}
        anchor={PLUGIN_POLICIES_ANCHOR}
        title="Plugin policies"
      />
    </>
  );
}

function RepoAccess(props: { org: string; repo: string; forge: string }) {
  const [bindings, { refetch }] = createResource(
    () => props.org,
    (org) => listBindings(org),
  );
  // A resource read in its error state throws, so an error lists nobody.
  const rows = () => repoAccessRows(bindings.error ? [] : (bindings() ?? []), props.repo);
  const [notice, setNotice] = createSignal<string | null>(null);
  const [granting, setGranting] = createSignal(false);
  const [revoking, setRevoking] = createSignal<string | null>(null);
  const [resyncing, setResyncing] = createSignal(false);
  // `null` is a deployment with no role store (authorization off).
  const available = () => !bindings.loading && !bindings.error && bindings() !== null;
  const source = (s: RoleSource) =>
    s === "forge" ? `from ${props.forge} collaborators` : s === "scarab" ? "granted in Scarab" : "";

  // A refusal shows in the confirm dialog.
  const revoke = async (s: string) => {
    await revokeRole(props.org, s, props.repo);
    setNotice(null);
    await refetch();
  };

  return (
    <div class="panel access-panel">
      <div class="panel-h">
        <span>Access · {props.repo}</span>
        <Show when={available()}>
          <span class="oenv-actions">
            <button class="btn btn-ghost btn-sm" onClick={() => setResyncing(true)}>
              <Icon icon="rotate-cw" size={13} /> Re-sync collaborators
            </button>
            <button class="btn btn-primary btn-sm" onClick={() => setGranting(true)}>
              <Icon icon="plus" size={14} /> Grant access
            </button>
          </span>
        </Show>
      </div>
      <div class="secrets-body">
        <Show when={bindings.error}>
          <p class="error">Could not load who has access.</p>
        </Show>
        <Show when={notice()}>
          <p class="notice-ok">{notice()}</p>
        </Show>
        <Show when={!bindings.loading || bindings()} fallback={<p class="empty">loading…</p>}>
          <Show
            when={bindings.error || bindings() !== null}
            fallback={
              <p class="empty">
                This deployment has no role store: authorization is off (dev), so there is
                nobody to grant anything to.
              </p>
            }
          >
            <Show when={rows().direct.length === 0 && !bindings.error}>
              <p class="empty">
                Nobody holds a role on {props.repo} itself. Grant someone access, or re-sync
                the repository's collaborators from {props.forge}.
              </p>
            </Show>
            <Show when={rows().direct.length + rows().viaOrg.length > 0}>
              <div class="table-scroll">
                <table class="table lib-table person-table access-table" aria-label={`Access to ${props.repo}`}>
                  <thead>
                    <tr>
                      <th>Person</th>
                      <th>Role</th>
                      <th>Source</th>
                      <th />
                    </tr>
                  </thead>
                  <tbody>
                    <For each={rows().direct}>
                      {(r) => (
                        <tr>
                          <td class="keep">
                            <PersonCell display={r.display} subject={r.subject} pending={r.pending} />
                          </td>
                          <td>
                            <span class="tok-role">{r.role}</span>
                            <Show when={r.orgRole}>
                              {(o) => <span class="access-note">also org role {o()}; the higher applies</span>}
                            </Show>
                          </td>
                          <td>{source(r.source)}</td>
                          <td class="acts keep">
                            <button class="btn btn-danger btn-sm" onClick={() => setRevoking(r.subject)}>
                              Revoke
                            </button>
                          </td>
                        </tr>
                      )}
                    </For>
                    <Show when={rows().viaOrg.length > 0}>
                      <tr class="access-group">
                        <th colSpan={4} scope="colgroup">
                          Through their org role
                        </th>
                      </tr>
                      <For each={rows().viaOrg}>
                        {(r) => (
                          <tr class="access-via-org">
                            <td class="keep">
                              <PersonCell display={r.display} subject={r.subject} pending={r.pending} />
                            </td>
                            <td>
                              via org role <span class="tok-role">{r.orgRole}</span>
                            </td>
                            <td>all of {props.org}</td>
                            <td />
                          </tr>
                        )}
                      </For>
                    </Show>
                  </tbody>
                </table>
              </div>
            </Show>
            <Show when={rows().viaOrg.length > 0}>
              <p class="f-help">
                Org roles apply to every repository in {props.org}. Change them under{" "}
                <A href={`/orgs/${encodeURIComponent(props.org)}/settings/members`}>Settings › Members</A>.
              </p>
            </Show>
          </Show>
        </Show>
      </div>

      <Show when={granting()}>
        <GrantAccessDialog
          org={props.org}
          repo={props.repo}
          onClose={() => setGranting(false)}
          onGranted={(subject, role) => {
            setGranting(false);
            setNotice(`${subject} is ${role} on ${props.repo}.`);
            void refetch();
          }}
        />
      </Show>
      <Show when={revoking()}>
        {(s) => (
          <ConfirmDestructive
            title={`Revoke ${s()}'s role on ${props.repo}?`}
            purpose="A revoke made here is kept: a later forge re-sync does not give the role back. An org role they hold still applies."
            name={s()}
            confirmLabel="Revoke role"
            onConfirm={() => revoke(s())}
            onClose={() => setRevoking(null)}
          />
        )}
      </Show>
      <Show when={resyncing()}>
        <ResyncDialog
          org={props.org}
          repo={props.repo}
          forge={props.forge}
          onDone={() => refetch()}
          onClose={() => setResyncing(false)}
        />
      </Show>
    </div>
  );
}

/** Grant a role on one repository: find the user, pick the role. A grant to
 *  someone who holds a role here already replaces it. */
function GrantAccessDialog(props: {
  org: string;
  repo: string;
  onClose: () => void;
  onGranted: (subject: string, role: string) => void;
}) {
  const [subject, setSubject] = createSignal("");
  const [role, setRole] = createSignal("member");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  const grant = async () => {
    if (!subject() || busy()) return;
    setBusy(true);
    setError(null);
    try {
      await grantRole(props.org, { subject: subject(), role: role(), project: props.repo });
      props.onGranted(subject(), role());
    } catch (e) {
      setError(e instanceof Error ? e.message : "The server refused the grant.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      eyebrow={`${props.org}/${props.repo} · access`}
      title="Grant access"
      purpose={`A role on ${props.repo} only. Org roles are managed under Settings › Members.`}
      busy={busy()}
      onClose={props.onClose}
      footer={
        <>
          <Show when={error()}>
            <ModalMessage>{error()}</ModalMessage>
          </Show>
          <button class="btn btn-ghost" type="button" onClick={props.onClose} disabled={busy()}>
            Cancel
          </button>
          <button class="btn btn-primary" type="button" disabled={!subject() || busy()} onClick={grant}>
            {busy() ? "Granting…" : "Grant access"}
          </button>
        </>
      }
    >
      <Field
        label="User"
        required
        help="A user exists once they have signed in, or once a re-sync imported their forge account."
      >
        <Show when={subject()} fallback={<PrincipalSearch org={props.org} onPick={setSubject} />}>
          <div class="tok-life">
            <span class="tok-scope mono">{subject()}</span>
            <button class="btn btn-ghost btn-sm" type="button" onClick={() => setSubject("")}>
              Change
            </button>
          </div>
        </Show>
      </Field>
      <Field label="Role" help={`Held on ${props.repo}. A grant to someone with a role here replaces it.`}>
        <Select value={role()} onValue={setRole}>
          <For each={ROLES}>{(r) => <option value={r}>{r}</option>}</For>
        </Select>
      </Field>
    </Modal>
  );
}

// The re-sync is a forge round-trip that writes, with no dry run on the
// server, so the dialog explains what it does, then runs it and shows what
// changed. The change is read from the bindings listed before and after.
function ResyncDialog(props: {
  org: string;
  repo: string;
  forge: string;
  onDone: () => void;
  onClose: () => void;
}) {
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [result, setResult] = createSignal<ReturnType<typeof resyncDiff> | null>(null);
  // Set once the import itself succeeded, so a failed read afterwards is not
  // reported as a failed re-sync.
  const [done, setDone] = createSignal(false);

  const run = async () => {
    setBusy(true);
    setError(null);
    try {
      const before: Binding[] = (await listBindings(props.org)) ?? [];
      await importFromForge(props.org, props.repo);
      setDone(true);
      props.onDone();
      try {
        const after: Binding[] = (await listBindings(props.org)) ?? [];
        setResult(resyncDiff(before, after, props.repo));
      } catch {
        setError("Re-synced. Could not load what changed; the Access list shows the roles now.");
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : "The re-sync failed.");
    } finally {
      setBusy(false);
    }
  };

  const unchanged = () => {
    const r = result();
    return !!r && r.added.length + r.changed.length + r.removed.length === 0;
  };

  return (
    <Modal
      eyebrow={`${props.org}/${props.repo} · access`}
      title={`Re-sync collaborators from ${props.forge}`}
      purpose={`Reads ${props.repo}'s collaborators from ${props.forge} and updates the roles it gave them on ${props.repo}.`}
      icon="rotate-cw"
      busy={busy()}
      onClose={props.onClose}
      footer={
        <>
          <Show when={error()}>
            <ModalMessage>{error()}</ModalMessage>
          </Show>
          <Show
            when={done()}
            fallback={
              <>
                <button class="btn btn-ghost" type="button" onClick={props.onClose} disabled={busy()}>
                  Cancel
                </button>
                <button class="btn btn-primary" type="button" onClick={run} disabled={busy()}>
                  <Icon icon="rotate-cw" size={14} /> {busy() ? "Re-syncing…" : "Re-sync"}
                </button>
              </>
            }
          >
            <button class="btn btn-primary" type="button" onClick={props.onClose}>
              Done
            </button>
          </Show>
        </>
      }
    >
      <ModalSection n={1} title="What it does">
        <p class="dlg-text">
          Each collaborator gets a role on {props.repo} from their permission on {props.forge}:
        </p>
        <p class="dlg-ctx">
          admin → admin
          <br />
          write → member
          <br />
          read → viewer
        </p>
        <p class="dlg-text">
          The role is pinned to their {props.forge} account, so whoever signs in with that account
          gets it. Someone the forge no longer lists loses the role it gave them. Roles granted
          or revoked in Scarab are never changed.
        </p>
      </ModalSection>
      <Show when={result()}>
        {(r) => (
          <ModalSection n={2} title="What changed">
            <Show
              when={!unchanged()}
              fallback={<p class="dlg-text">Nothing changed: the roles already match {props.forge}.</p>}
            >
              <ChangeList label="Added" items={r().added} />
              <ChangeList label="Changed" items={r().changed} />
              <ChangeList label="Removed" items={r().removed} />
            </Show>
          </ModalSection>
        )}
      </Show>
    </Modal>
  );
}

function ChangeList(props: { label: string; items: ResyncChange[] }) {
  return (
    <Show when={props.items.length > 0}>
      <div>
        <p class="dlg-text">
          {props.label}: {props.items.length}
        </p>
        <ul class="resync-list">
          <For each={props.items}>
            {(c) => (
              <li>
                {c.display} <span class="mono subtle">{c.subject}</span>{" "}
                {c.was ? `${c.was} → ${c.role}` : c.role}
              </li>
            )}
          </For>
        </ul>
      </div>
    </Show>
  );
}
