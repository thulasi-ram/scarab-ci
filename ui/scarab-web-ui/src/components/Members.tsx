// Settings › Members: who holds which role in the org, and granting and
// revoking org roles. An org role applies to every repository of the org.
//
// Repository roles live on each repository's Settings › Access, with the
// forge re-sync that seeds them, so this list only names the repositories a
// person also holds a role in and links there. Someone with repository roles
// only is still listed: they are a member of the org in practice. "Not signed
// in yet" marks a user a re-sync created from their forge account: whoever
// signs in with exactly that account gets these roles, and nobody else can.
import { For, Show, createResource, createSignal } from "solid-js";
import { A } from "@solidjs/router";
import { grantRole, listBindings, revokeRole } from "../api/client";
import { memberRows } from "../identities";
import Icon from "./Icon";
import ConfirmDestructive from "./ConfirmDestructive";
import { Select } from "./Field";
import PrincipalSearch from "./PrincipalSearch";

const ROLES = ["viewer", "member", "admin", "owner"];

export default function Members(props: { org: string }) {
  const [bindings, { refetch }] = createResource(() => listBindings(props.org));
  const [error, setError] = createSignal<string | null>(null);
  const [subject, setSubject] = createSignal("");
  const [role, setRole] = createSignal("member");
  const [busy, setBusy] = createSignal(false);

  const rows = () => memberRows(bindings() ?? []);

  const grant = async () => {
    setBusy(true);
    setError(null);
    try {
      await grantRole(props.org, { subject: subject(), role: role() });
      setSubject("");
      await refetch();
    } catch (e) {
      setError(e instanceof Error ? e.message : "That did not work.");
    } finally {
      setBusy(false);
    }
  };

  // The person whose org role the confirm dialog is about to revoke. A
  // refusal shows in the dialog.
  const [revoking, setRevoking] = createSignal<string | null>(null);
  const revoke = async (s: string) => {
    setBusy(true);
    setError(null);
    try {
      await revokeRole(props.org, s);
      await refetch();
    } finally {
      setBusy(false);
    }
  };

  const repoHref = (repo: string) =>
    `/${encodeURIComponent(props.org)}/${encodeURIComponent(repo)}?tab=settings`;

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
              when={rows().length > 0}
              fallback={
                <p class="empty">
                  Nobody holds a role in {props.org} yet. Grant someone an org role below.
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
                        <Show
                          when={r.orgRole}
                          fallback={<span class="subtle">no org role</span>}
                        >
                          {(orgRole) => (
                            <span>
                              <span class="tok-role">{orgRole()}</span> all of {props.org}
                              <button
                                class="btn btn-ghost btn-sm"
                                disabled={busy()}
                                onClick={() => setRevoking(r.subject)}
                              >
                                revoke
                              </button>
                            </span>
                          )}
                        </Show>
                        <Show when={r.repos.length > 0}>
                          <span class="member-repos">
                            also has roles in {r.repos.length}{" "}
                            {r.repos.length === 1 ? "repository" : "repositories"}:{" "}
                            <For each={r.repos}>
                              {(repo, i) => (
                                <>
                                  {i() > 0 ? ", " : ""}
                                  <A href={repoHref(repo)}>{repo}</A>
                                </>
                              )}
                            </For>
                          </span>
                        </Show>
                      </div>
                    </li>
                  )}
                </For>
              </ul>
            </Show>

            <div class="f">
              <div class="f-l">
                <span class="f-lbl">Grant an org role</span>
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
                  <span class="subtle">all of {props.org}</span>
                  <button class="btn btn-primary btn-sm" disabled={busy()} onClick={grant}>
                    grant
                  </button>
                  <button class="btn btn-ghost btn-sm" onClick={() => setSubject("")}>
                    cancel
                  </button>
                </div>
              </Show>
              <p class="f-help">
                An org role applies to every repository in {props.org}. To give someone a role on one
                repository, use that repository's Settings › Access.
              </p>
            </div>

            <Show when={revoking()}>
              {(s) => (
                <ConfirmDestructive
                  title={`Revoke ${s()}'s role on all of ${props.org}?`}
                  purpose="Their roles on single repositories stay. A revoke made here is kept: a later forge re-sync does not give the role back."
                  name={s()}
                  confirmLabel="Revoke role"
                  onConfirm={() => revoke(s())}
                  onClose={() => setRevoking(null)}
                />
              )}
            </Show>
          </Show>
        </Show>
      </div>
    </div>
  );
}
