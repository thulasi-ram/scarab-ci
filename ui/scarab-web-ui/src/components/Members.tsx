// Settings › Members: who holds which role in the org. An org role applies to
// every repository of the org.
//
// One row per person: their name (a link to their page), the subject a grant
// stores, "not signed in yet" for a user a forge re-sync created, their org
// role, and how many repositories they hold a role in. Someone with
// repository roles only is listed too: they are a member of the org in
// practice. Grant role, top right, grants an org role in a dialog. Changing
// or revoking one is on the person's page (`…/settings/members/<subject>/
// manage`, routes/MemberManage.tsx), which also lists their repository roles.
import { For, Show, createResource, createSignal } from "solid-js";
import { A } from "@solidjs/router";
import { grantRole, listBindings } from "../api/client";
import { memberHref, memberRows, ORG_ROLES, repoCountLabel } from "../identities";
import Icon from "./Icon";
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
              <ul class="tok-list" aria-label="Members">
                <For each={rows()}>
                  {(r) => (
                    <li class="tok">
                      <div class="tok-head">
                        <Icon icon="user" size={15} />
                        <A class="tok-name oenv-link" href={memberHref(props.org, r.subject)}>
                          {r.display}
                        </A>
                        <span class="tok-scope mono">{r.subject}</span>
                        <Show when={r.pending}>
                          <span
                            class="tok-state tok-expired"
                            title="Imported from the forge; becomes theirs when they sign in with that forge account"
                          >
                            not signed in yet
                          </span>
                        </Show>
                        <span class="oenv-actions">
                          <A class="btn btn-ghost btn-sm" href={memberHref(props.org, r.subject)}>
                            Manage
                          </A>
                        </span>
                      </div>
                      <div class="tok-facts">
                        <Show when={r.orgRole} fallback={<span class="subtle">no org role</span>}>
                          {(role) => (
                            <span>
                              <span class="tok-role">{role()}</span> all of {props.org}
                            </span>
                          )}
                        </Show>
                        <Show when={r.repos.length > 0}>
                          <span>{repoCountLabel(r.repos.length)}</span>
                        </Show>
                      </div>
                    </li>
                  )}
                </For>
              </ul>
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
