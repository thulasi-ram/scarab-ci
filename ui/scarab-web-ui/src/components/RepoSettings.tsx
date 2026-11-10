// A repository's Settings tab: what the server knows about where the
// repository lives, with Disable for those who may unbind it, then Access,
// the people who hold a role on it.
//
// Disable is the org page's: the same confirmation and unbind call
// (DisableRepository), offered under the rule the unbind endpoint applies:
// the caller administers the org and the connection is not managed by
// configuration. Both come from the org's connection list the panel already
// loads, which names the repository's binding; no forge is asked. Once it is
// disabled the repository has no page, so the tab goes to the org page.
//
// Access lists the repository's own role bindings, each with where it came
// from (granted in Scarab, or seeded from the forge's collaborators by a
// re-sync), and below them, greyed, the people whose org role reaches the
// repository. Org roles are managed under Settings › Members; this panel
// grants, revokes and re-syncs at this repository's scope only.
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
  revokeRole,
  type Binding,
} from "../api/client";
import { me } from "../session";
import { administers } from "../settings-nav";
import { repoAccessRows, resyncDiff, type ResyncChange } from "../identities";
import { connectionFor, forgeName } from "../connections";
import { absTime, relTime } from "../fmt";
import { UNREACHABLE_NOTE, orgPageHref } from "../org-page";
import Icon from "./Icon";
import ConfirmDestructive from "./ConfirmDestructive";
import DisableRepository, { type DisableTarget } from "./DisableRepository";
import Modal, { ModalMessage, ModalSection } from "./Modal";
import { Select } from "./Field";
import PrincipalSearch from "./PrincipalSearch";
import PolicyLists from "./PolicyLists";
import { LIBRARY_KINDS, LIBRARY_POLICIES_ANCHOR, PLUGIN_KINDS, PLUGIN_POLICIES_ANCHOR } from "../policies";

const ROLES = ["viewer", "member", "admin", "owner"];

export default function RepoSettings(props: { org: string; repo: string; repoUrl: string | null }) {
  const canAdmin = () => administers(me(), props.org);
  // Connections are listed to org admins only. A failure leaves the rows out.
  const [connection] = createResource(
    () => (canAdmin() ? { org: props.org, repo: props.repo } : false),
    async (k) => connectionFor(await listConnections(k.org).catch(() => []), k.org, k.repo),
  );
  const forge = () => forgeName(connection()?.kind);
  // The repository's binding on that connection.
  const bound = () =>
    connection()?.projects.find((x) => x.org === props.org && x.project === props.repo) ?? null;
  // What Disable unbinds, when the caller may: the connection is listed only
  // to org admins, and one managed by configuration is read-only here.
  const binding = (): DisableTarget | null => {
    const c = connection();
    const p = bound();
    if (!c || !p || c.managed_by_config) return null;
    return { connection_id: c.id, owner: p.owner, name: p.name, project: p.project };
  };
  const [disabling, setDisabling] = createSignal(false);
  const navigate = useNavigate();

  return (
    <>
      <div class="panel">
        <div class="panel-h"><span>Repository</span></div>
        <div class="settings-body">
          <Show
            when={props.repoUrl}
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
            <div class="repo-disable">
              <button class="btn btn-danger btn-sm" type="button" onClick={() => setDisabling(true)}>
                Disable repository
              </button>
              <Show when={bound()?.unreachable_since != null}>
                <span class="repo-unreachable-note">
                  <Icon icon="alert-triangle" size={12} /> {UNREACHABLE_NOTE}
                </span>
              </Show>
            </div>
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
  const rows = () => repoAccessRows(bindings() ?? [], props.repo);
  const [error, setError] = createSignal<string | null>(null);
  const [subject, setSubject] = createSignal("");
  const [role, setRole] = createSignal("member");
  const [busy, setBusy] = createSignal(false);
  const [revoking, setRevoking] = createSignal<string | null>(null);
  const [resyncing, setResyncing] = createSignal(false);

  const grant = async () => {
    setBusy(true);
    setError(null);
    try {
      await grantRole(props.org, { subject: subject(), role: role(), project: props.repo });
      setSubject("");
      await refetch();
    } catch (e) {
      setError(e instanceof Error ? e.message : "That did not work.");
    } finally {
      setBusy(false);
    }
  };

  const revoke = async (s: string) => {
    setBusy(true);
    setError(null);
    try {
      await revokeRole(props.org, s, props.repo);
      await refetch();
    } finally {
      setBusy(false);
    }
  };

  return (
    <div class="panel">
      <div class="panel-h">
        <span>Access · {props.repo}</span>
        <Show when={bindings()}>
          <button class="btn btn-ghost btn-sm" onClick={() => setResyncing(true)}>
            <Icon icon="rotate-cw" size={13} /> Re-sync collaborators
          </button>
        </Show>
      </div>
      <div class="secrets-body">
        <Show when={bindings.error}>
          <p class="error">Could not load who has access.</p>
        </Show>
        <Show when={error()}>
          <p class="error">{error()}</p>
        </Show>
        <Show when={!bindings.loading} fallback={<p class="empty">loading…</p>}>
          <Show
            when={bindings() !== null}
            fallback={
              <p class="empty">
                This deployment has no role store: authorization is off (dev), so there is
                nobody to grant anything to.
              </p>
            }
          >
            <Show
              when={rows().direct.length > 0}
              fallback={
                <p class="empty">
                  Nobody holds a role on {props.repo} itself. Grant someone a role below, or
                  re-sync the repository's collaborators from {props.forge}.
                </p>
              }
            >
              <ul class="tok-list">
                <For each={rows().direct}>
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
                        <span>
                          <span class="tok-role">{r.role}</span>
                          <Show when={r.source}>
                            {(src) => (
                              <span>
                                {src() === "forge"
                                  ? `from ${props.forge} collaborators`
                                  : "granted in Scarab"}
                              </span>
                            )}
                          </Show>
                          <button
                            class="btn btn-ghost btn-sm"
                            disabled={busy()}
                            onClick={() => setRevoking(r.subject)}
                          >
                            revoke
                          </button>
                        </span>
                        <Show when={r.orgRole}>
                          {(o) => <span>also org role {o()}; the higher applies</span>}
                        </Show>
                      </div>
                    </li>
                  )}
                </For>
              </ul>
            </Show>

            <div class="f">
              <div class="f-l">
                <span class="f-lbl">Grant a role on {props.repo}</span>
              </div>
              <Show
                when={subject()}
                fallback={<PrincipalSearch org={props.org} onPick={setSubject} />}
              >
                <div class="tok-life">
                  <span class="tok-scope mono">{subject()}</span>
                  <Select aria-label="role" value={role()} onValue={setRole}>
                    <For each={ROLES}>{(r) => <option value={r}>{r}</option>}</For>
                  </Select>
                  <button class="btn btn-primary btn-sm" disabled={busy()} onClick={grant}>
                    grant
                  </button>
                  <button class="btn btn-ghost btn-sm" onClick={() => setSubject("")}>
                    cancel
                  </button>
                </div>
              </Show>
            </div>

            <Show when={rows().viaOrg.length > 0}>
              <h3 class="access-h">Through their org role</h3>
              <ul class="tok-list">
                <For each={rows().viaOrg}>
                  {(r) => (
                    <li class="tok access-via-org">
                      <div class="tok-head">
                        <Icon icon="user" size={15} />
                        <span class="tok-name">{r.display}</span>
                        <span class="tok-scope mono">{r.subject}</span>
                      </div>
                      <div class="tok-facts">
                        <span>
                          via org role <span class="tok-role">{r.orgRole}</span>
                        </span>
                      </div>
                    </li>
                  )}
                </For>
              </ul>
              <p class="f-help">
                Org roles apply to every repository in {props.org}. Change them under{" "}
                <A href={`/orgs/${encodeURIComponent(props.org)}/settings/members`}>Settings › Members</A>.
              </p>
            </Show>
          </Show>
        </Show>
      </div>

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
