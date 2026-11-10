// One member's page, `/orgs/<org>/settings/members/<subject>/manage`, inside
// the settings frame with Members marked in the sidebar. A role binding has
// no id of its own, so the subject is the key.
//
// It shows who the person is and whether anyone has signed in as them yet,
// their org role with Change role (a grant replaces the role) and Revoke,
// which asks for the subject to be typed, and their repository roles,
// read-only: each is changed on that repository's Settings › Access, next to
// the forge re-sync that seeds them.
import { createMemo, createResource, createSignal, For, Show } from "solid-js";
import { A, useNavigate, useParams } from "@solidjs/router";
import { grantRole, listBindings, revokeRole } from "../api/client";
import { memberRepoRoles, memberRows, ORG_ROLES, repoAccessHref, type MemberRow } from "../identities";
import { decodeParam, orgHref } from "../settings-nav";
import { SettingsCrumbs, SettingsFrame } from "./Settings";
import { Section } from "./LibraryManage";
import ConfirmDestructive from "../components/ConfirmDestructive";
import { Select } from "../components/Field";

/** `/orgs/:org/settings/members/:subject/manage`. */
export default function MemberManage() {
  const params = useParams<{ org: string; subject: string }>();
  const org = () => decodeParam(params.org);
  const subject = () => decodeParam(params.subject);
  const key = createMemo(() => JSON.stringify([org(), subject()]));
  return (
    <SettingsFrame kind="org" org={org()} section="members">
      {/* Keyed: another person in the URL is a new page. */}
      <Show when={key()} keyed>
        {(_key) => <Manage org={org()} subject={subject()} />}
      </Show>
    </SettingsFrame>
  );
}

function Manage(props: { org: string; subject: string }) {
  const back = () => orgHref(props.org, "members");
  const [bindings, { refetch }] = createResource(() => listBindings(props.org));
  const all = () => (bindings.error ? [] : (bindings() ?? []));
  const row = () => memberRows(all()).find((r) => r.subject === props.subject);
  const [notice, setNotice] = createSignal<string | null>(null);

  return (
    <>
      <SettingsCrumbs
        trail={[
          { label: "Members", href: back() },
          { label: row()?.display ?? props.subject, mono: !row() || row()!.display === props.subject },
        ]}
        sub={
          <>
            <span class="mono">{props.org}</span> · <span class="mono">{props.subject}</span> · manage a
            member
          </>
        }
      />
      <Show when={notice()}>
        <p class="notice-ok">{notice()}</p>
      </Show>
      <Show when={!bindings.error} fallback={<p class="error">Could not load members.</p>}>
        <Show when={!bindings.loading || bindings()} fallback={<p class="empty">loading…</p>}>
          <Show
            when={row()}
            fallback={
              <p class="empty">
                <b class="mono">{props.subject}</b> holds no role in {props.org}.{" "}
                <A href={back()}>All members</A>
              </p>
            }
          >
            {(r) => (
              <Member
                org={props.org}
                row={r()}
                repos={memberRepoRoles(all(), props.subject)}
                onDone={(msg) => {
                  setNotice(msg);
                  void refetch();
                }}
              />
            )}
          </Show>
        </Show>
      </Show>
    </>
  );
}

function Member(props: {
  org: string;
  row: MemberRow;
  repos: ReturnType<typeof memberRepoRoles>;
  onDone: (msg: string) => void;
}) {
  const nav = useNavigate();
  const r = () => props.row;
  const [role, setRole] = createSignal(r().orgRole ?? "member");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [revoking, setRevoking] = createSignal(false);

  const grant = async () => {
    setBusy(true);
    setError(null);
    try {
      await grantRole(props.org, { subject: r().subject, role: role() });
      props.onDone(`${r().display} is ${role()} on all of ${props.org}.`);
    } catch (e) {
      setError(e instanceof Error ? e.message : "The server refused the grant.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <Section title="Member">
        <dl class="kv">
          <dt>Name</dt>
          <dd>{r().display}</dd>
          <dt>Subject</dt>
          <dd class="m">{r().subject}</dd>
          <dt>Sign-in</dt>
          <dd>
            <Show when={r().pending} fallback="Signed in">
              <span class="tok-state tok-expired">not signed in yet</span>{" "}
              Imported from the forge. These roles become theirs when they sign in with that forge
              account, and nobody else's.
            </Show>
          </dd>
        </dl>
      </Section>

      <Section
        title="Org role"
        foot={
          <>
            <Select aria-label="Org role" value={role()} onValue={setRole}>
              <For each={ORG_ROLES}>{(x) => <option value={x}>{x}</option>}</For>
            </Select>
            <button
              class="btn btn-primary btn-sm"
              disabled={busy() || role() === r().orgRole}
              onClick={() => void grant()}
            >
              {r().orgRole ? "Change role" : "Grant role"}
            </button>
            <Show when={r().orgRole}>
              <button class="btn btn-danger btn-sm" disabled={busy()} onClick={() => setRevoking(true)}>
                Revoke
              </button>
            </Show>
          </>
        }
      >
        <dl class="kv">
          <dt>Role</dt>
          <dd>
            <Show when={r().orgRole} fallback={<span class="subtle">no org role</span>}>
              {(x) => (
                <>
                  <span class="tok-role">{x()}</span> all of {props.org}
                </>
              )}
            </Show>
          </dd>
        </dl>
        <Show when={error()}>
          <p class="error kv-note">{error()}</p>
        </Show>
      </Section>

      <Section title="Repository roles">
        <Show
          when={props.repos.length > 0}
          fallback={<p class="kv-note kv-lead">No roles on single repositories.</p>}
        >
          <dl class="kv" aria-label="Repository roles">
            <For each={props.repos}>
              {(x) => (
                <>
                  <dt>
                    <A class="mono" href={repoAccessHref(props.org, x.repo)}>
                      {x.repo}
                    </A>
                  </dt>
                  <dd>
                    <span class="tok-role">{x.role}</span>
                    <Show when={x.source}>
                      {(src) => (
                        <span class="subtle">
                          {" "}
                          {src() === "forge" ? "from the forge's collaborators" : "granted in Scarab"}
                        </span>
                      )}
                    </Show>
                  </dd>
                </>
              )}
            </For>
          </dl>
        </Show>
        <p class="kv-note">
          A role on one repository is changed on that repository's Settings › Access, next to the
          forge re-sync that seeds them. An org role reaches every repository; where both apply, the
          higher one counts.
        </p>
      </Section>

      <Show when={revoking()}>
        <ConfirmDestructive
          eyebrow={`${props.org} · members`}
          title={`Revoke ${r().subject}'s role on all of ${props.org}?`}
          purpose="Their roles on single repositories stay. A revoke made here is kept: a later forge re-sync does not give the role back."
          name={r().subject}
          confirmLabel="Revoke role"
          onConfirm={async () => {
            await revokeRole(props.org, r().subject);
            // With no repository role left they are no longer a member: back
            // to the list.
            if (props.repos.length === 0) nav(orgHref(props.org, "members"));
            else props.onDone(`${r().display} no longer holds an org role in ${props.org}.`);
          }}
          onClose={() => setRevoking(false)}
        />
      </Show>
    </>
  );
}
