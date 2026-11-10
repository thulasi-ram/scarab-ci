// One member's page, `/orgs/<org>/settings/members/<id>/manage`, inside
// the settings frame with Members marked in the sidebar. The URL carries the
// Principal's id, never the handle: a handle can be an email address.
//
// It shows who the person is and whether anyone has signed in as them yet,
// their org role with Change role (a grant replaces the role) and Revoke,
// which asks for the handle to be typed, and their repository roles,
// read-only: each is changed on that repository's Settings › Access, next to
// the forge re-sync that seeds them.
import { createMemo, createResource, createSignal, For, Show } from "solid-js";
import { A, useNavigate, useParams } from "@solidjs/router";
import { getMember, grantRole, revokeRole } from "../api/client";
import { memberRepoRoles, memberRows, ORG_ROLES, repoAccessHref, type MemberRow } from "../identities";
import { decodeParam, orgHref } from "../settings-nav";
import { SettingsCrumbs, SettingsFrame } from "./Settings";
import { Section } from "./LibraryManage";
import ConfirmDestructive from "../components/ConfirmDestructive";
import { Select } from "../components/Field";

/** `/orgs/:org/settings/members/:id/manage`. */
export default function MemberManage() {
  const params = useParams<{ org: string; id: string }>();
  const org = () => decodeParam(params.org);
  const id = () => decodeParam(params.id);
  const key = createMemo(() => JSON.stringify([org(), id()]));
  return (
    <SettingsFrame kind="org" org={org()} section="members">
      {/* Keyed: another person in the URL is a new page. */}
      <Show when={key()} keyed>
        {(_key) => <Manage org={org()} id={id()} />}
      </Show>
    </SettingsFrame>
  );
}

function Manage(props: { org: string; id: string }) {
  const back = () => orgHref(props.org, "members");
  const [member, { refetch }] = createResource(() => getMember(props.org, props.id));
  const bindings = () => (member.error ? [] : (member()?.bindings ?? []));
  const row = (): MemberRow | undefined => {
    const m = member();
    if (!m) return undefined;
    return (
      memberRows(bindings())[0] ?? {
        id: m.id,
        handle: m.handle,
        display: m.display_name || m.handle,
        pending: m.claimed === false,
        orgRole: null,
        repos: [],
      }
    );
  };
  const holdsRole = () => bindings().length > 0;
  const [notice, setNotice] = createSignal<string | null>(null);

  return (
    <>
      <SettingsCrumbs
        trail={[
          { label: "Members", href: back() },
          { label: row()?.display ?? "member", mono: !!row() && row()!.display === row()!.handle },
        ]}
        sub={
          <>
            <span class="mono">{props.org}</span>
            <Show when={row()}>
              {(r) => (
                <>
                  {" "}
                  · <span class="mono">{r().handle}</span>
                </>
              )}
            </Show>{" "}
            · manage a member
          </>
        }
      />
      <Show when={notice()}>
        <p class="notice-ok">{notice()}</p>
      </Show>
      <Show when={!member.error} fallback={<p class="error">Could not load this member.</p>}>
        <Show when={!member.loading || member()} fallback={<p class="empty">loading…</p>}>
          <Show
            when={row()}
            fallback={
              <p class="empty">
                No such person in {props.org}. <A href={back()}>All members</A>
              </p>
            }
          >
            {(r) => (
              <Show
                when={holdsRole()}
                fallback={
                  <p class="empty">
                    <b class="mono">{r().handle}</b> holds no role in {props.org}.{" "}
                    <A href={back()}>All members</A>
                  </p>
                }
              >
                <Member
                  org={props.org}
                  row={r()}
                  repos={memberRepoRoles(bindings(), props.id)}
                  onDone={(msg) => {
                    setNotice(msg);
                    void refetch();
                  }}
                />
              </Show>
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
      await grantRole(props.org, { subject: r().id, role: role() });
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
          <dt>Handle</dt>
          <dd class="m">{r().handle}</dd>
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
          title={`Revoke ${r().handle}'s role on all of ${props.org}?`}
          purpose="Their roles on single repositories stay. A revoke made here is kept: a later forge re-sync does not give the role back."
          name={r().handle}
          confirmLabel="Revoke role"
          onConfirm={async () => {
            await revokeRole(props.org, r().id);
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
