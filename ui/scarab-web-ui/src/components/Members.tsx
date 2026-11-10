// Settings › Members: who holds which role in the org. An org role applies to
// every repository of the org.
//
// One table row per person: their name (a link to their page) with the
// subject a grant stores under it and "not signed in yet" for a user a forge
// re-sync created, their org role, and how many repositories they hold a role
// in. Someone with
// repository roles only is listed too: they are a member of the org in
// practice. Grant role, top right, grants an org role in a dialog. Changing
// or revoking one is on the person's page (`…/settings/members/<subject>/
// manage`, routes/MemberManage.tsx), which also lists their repository roles.
import { For, Show, createResource, createSignal } from "solid-js";
import { A } from "@solidjs/router";
import { grantRole, listBindings } from "../api/client";
import { memberHref, memberRows, ORG_ROLES, repoCountLabel } from "../identities";
import Icon from "./Icon";
import Chips from "./Chips";
import Modal, { ModalMessage } from "./Modal";
import { Field, Select } from "./Field";
import PrincipalSearch from "./PrincipalSearch";

export default function Members(props: { org: string }) {
  const [bindings, { refetch }] = createResource(() => listBindings(props.org));
  const [granting, setGranting] = createSignal(false);
  const [notice, setNotice] = createSignal<string | null>(null);

  const rows = () => (bindings.error ? [] : memberRows(bindings() ?? []));
  // `null` is a deployment with no role store (authorization off).
  const available = () => !bindings.loading && !bindings.error && bindings() !== null;

  return (
    <div class="panel">
      <div class="panel-h">
        <span>
          Members · {props.org} <span class="tok-count">{rows().length}</span>
        </span>
        <Show when={available()}>
          <button class="btn btn-primary btn-sm" onClick={() => setGranting(true)}>
            <Icon icon="plus" size={14} /> Grant role
          </button>
        </Show>
      </div>
      <div class="secrets-body">
        <Show when={bindings.error}>
          <p class="error">Could not load members.</p>
        </Show>
        <Show when={notice()}>
          <p class="notice-ok">{notice()}</p>
        </Show>

        <Show when={!bindings.loading || bindings()} fallback={<p class="empty">loading…</p>}>
          <Show
            when={bindings.error || bindings() !== null}
            fallback={
              <p class="empty">
                This deployment has no role store: authorization is off, so there is nobody to
                grant anything to.
              </p>
            }
          >
            <Show
              when={rows().length > 0}
              fallback={
                <Show when={!bindings.error}>
                  <p class="empty">Nobody holds a role in {props.org} yet.</p>
                </Show>
              }
            >
              <div class="table-scroll">
                <table class="table lib-table person-table" aria-label="Members">
                  <thead>
                    <tr>
                      <th>Person</th>
                      <th>Org role</th>
                      <th>Repositories</th>
                      <th />
                    </tr>
                  </thead>
                  <tbody>
                    <For each={rows()}>
                      {(r) => (
                        <tr>
                          <td class="keep">
                            <PersonCell
                              display={r.display}
                              subject={r.subject}
                              pending={r.pending}
                              href={memberHref(props.org, r.subject)}
                            />
                          </td>
                          <td>
                            <Show when={r.orgRole} fallback={<span class="subtle">no org role</span>}>
                              {(role) => <span class="tok-role">{role()}</span>}
                            </Show>
                          </td>
                          <td>
                            <Show when={r.repos.length > 0} fallback={<span class="subtle">none</span>}>
                              {repoCountLabel(r.repos.length)}
                            </Show>
                          </td>
                          <td class="acts keep">
                            <A class="btn btn-ghost btn-sm" href={memberHref(props.org, r.subject)}>
                              Manage
                            </A>
                          </td>
                        </tr>
                      )}
                    </For>
                  </tbody>
                </table>
              </div>
            </Show>
          </Show>
        </Show>
      </div>

      <Show when={granting()}>
        <GrantRoleDialog
          org={props.org}
          onClose={() => setGranting(false)}
          onGranted={(subject, role) => {
            setGranting(false);
            setNotice(`${subject} is ${role} on all of ${props.org}.`);
            void refetch();
          }}
        />
      </Show>
    </div>
  );
}

/** Grant an org role: find the user, pick the role. A grant to someone who
 *  holds an org role already replaces it. */
export function GrantRoleDialog(props: {
  org: string;
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
      await grantRole(props.org, { subject: subject(), role: role() });
      props.onGranted(subject(), role());
    } catch (e) {
      setError(e instanceof Error ? e.message : "The server refused the grant.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      eyebrow={`${props.org} · members`}
      title="Grant role"
      purpose={`An org role applies to every repository in ${props.org}.`}
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
            {busy() ? "Granting…" : "Grant role"}
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
      <Field label="Role" help={`Held on all of ${props.org}. A grant to someone with an org role replaces it.`}>
        <Select value={role()} onValue={setRole}>
          <For each={ORG_ROLES}>{(r) => <option value={r}>{r}</option>}</For>
        </Select>
      </Field>
    </Modal>
  );
}

const NOT_SIGNED_IN = [
  {
    label: "not signed in yet",
    tone: "muted" as const,
    title: "Imported from the forge; becomes theirs when they sign in with that forge account",
  },
];

/** A person in a table cell: their name (a link to `href` when given), the
 *  subject a grant stores under it, and "not signed in yet" for a user a forge
 *  re-sync created. Members and a repository's Access both use it. */
export function PersonCell(props: { display: string; subject: string; pending: boolean; href?: string }) {
  return (
    <>
      <span class="person-line">
        <Show when={props.href} fallback={<span class="person-name">{props.display}</span>}>
          {(href) => (
            <A class="person-name oenv-link" href={href()}>
              {props.display}
            </A>
          )}
        </Show>
        <Show when={props.pending}>
          <Chips chips={NOT_SIGNED_IN} />
        </Show>
      </span>
      <span class="person-id mono">{props.subject}</span>
    </>
  );
}
