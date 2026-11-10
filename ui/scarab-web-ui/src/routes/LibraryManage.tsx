// One installed library's page, `/orgs/<org>/settings/libraries/<id>/manage`,
// inside the settings frame with Libraries marked in the sidebar. The id is
// the installation's; an upgrade keeps it. An old `<publisher>~<name>` URL is
// sent on to the installation's id URL when that library is installed.
//
// It holds everything done to the installation: its version and source,
// Upgrade (the install dialog with the source filled in, then the review
// page), trust in its source with forge credentials, yanking the installed
// version and lifting the yank, and Remove, which asks for the name to be
// typed. Trusting and lifting a yank ask for a step-up; stopping trust,
// yanking and removing tighten and ask for nothing.
//
// The server keeps one version per installation and no history of earlier
// ones, so the page shows the installed version only.
import { createMemo, createResource, createSignal, For, Show, type JSX } from "solid-js";
import { A, Navigate, useNavigate, useParams } from "@solidjs/router";
import {
  getLibraryInstallation,
  listLibraries,
  removeLibrary,
  trustLibrarySource,
  untrustLibrarySource,
  unyankLibrary,
  yankImpact,
  yankLibrary,
  type LibraryInstallation,
  type OrgLibraries,
} from "../api/client";
import {
  impactRunLabel,
  libraryChips,
  libraryId,
  libraryManageHref,
  parseLibraryKey,
  shortHash,
  trustKey,
  yankButtonLabel,
  yankImpactLine,
  yankReasonError,
} from "../libraries";
import { absTime, relTime } from "../fmt";
import { decodeParam, orgHref } from "../settings-nav";
import { SettingsCrumbs, SettingsFrame } from "./Settings";
import Chips from "../components/Chips";
import Icon from "../components/Icon";
import InstallLibraryDialog from "../components/LibraryInstall";
import Modal, { ModalMessage } from "../components/Modal";
import ConfirmDestructive, { ConfirmAction } from "../components/ConfirmDestructive";
import { Checkbox, Field, Textarea } from "../components/Field";

/** `/orgs/:org/settings/libraries/:id/manage`. */
export default function LibraryManage() {
  const params = useParams<{ org: string; id: string }>();
  const org = () => decodeParam(params.org);
  const id = () => decodeParam(params.id);
  const key = createMemo(() => JSON.stringify([org(), id()]));
  return (
    <SettingsFrame kind="org" org={org()} section="libraries">
      {/* Keyed: another library in the URL is a new page. */}
      <Show when={key()} keyed>
        {(_key) => <Manage org={org()} id={id()} />}
      </Show>
    </SettingsFrame>
  );
}

function Manage(props: { org: string; id: string }) {
  // A URL from before installations had ids names `<publisher>~<name>`.
  const legacy = parseLibraryKey(props.id);
  return (
    <Show when={legacy} fallback={<ById org={props.org} id={props.id} />}>
      {(l) => <Legacy org={props.org} segment={props.id} publisher={l().publisher} name={l().name} />}
    </Show>
  );
}

/** An old `<publisher>~<name>` URL: on to the installation's id URL. */
function Legacy(props: { org: string; segment: string; publisher: string; name: string }) {
  const back = () => orgHref(props.org, "libraries");
  const [libs] = createResource(() => listLibraries(props.org));
  const found = () =>
    libs.error ? undefined : libs()?.installations.find((l) => l.publisher === props.publisher && l.name === props.name);
  return (
    <>
      <SettingsCrumbs
        trail={[{ label: "Libraries", href: back() }, { label: libraryId(props), mono: true }]}
        sub={
          <>
            <span class="mono">{props.org}</span> · manage a library
          </>
        }
      />
      <Show when={!libs.error} fallback={<p class="error">Could not load the org's libraries.</p>}>
        <Show when={!libs.loading} fallback={<p class="empty">loading…</p>}>
          <Show
            when={found()}
            fallback={
              <p class="empty">
                There is no library <b class="mono">{libraryId(props)}</b> installed in {props.org}.{" "}
                <A href={back()}>All libraries</A>
              </p>
            }
          >
            {(l) => <Navigate href={libraryManageHref(props.org, l())} />}
          </Show>
        </Show>
      </Show>
    </>
  );
}

function ById(props: { org: string; id: string }) {
  const nav = useNavigate();
  const back = () => orgHref(props.org, "libraries");
  const [installed, { refetch: refetchOne }] = createResource(() => getLibraryInstallation(props.org, props.id));
  // The org's trusted sources and yanks, for the tags and the trust panel.
  const [libs, { refetch: refetchAll }] = createResource(() => listLibraries(props.org));
  const lib = () => (installed.error ? undefined : (installed() ?? undefined));
  const title = () => {
    const l = lib();
    return l ? libraryId(l) : props.id;
  };

  const [notice, setNotice] = createSignal<string | null>(null);
  const [error, setError] = createSignal<string | null>(null);
  const done = (msg: string) => {
    setError(null);
    setNotice(msg);
    void refetchOne();
    void refetchAll();
  };

  return (
    <>
      <SettingsCrumbs
        trail={[{ label: "Libraries", href: back() }, { label: title(), mono: true }]}
        sub={
          <>
            <span class="mono">{props.org}</span> · manage a library
          </>
        }
      />
      <Show when={notice()}>
        <p class="notice-ok">{notice()}</p>
      </Show>
      <Show when={error()}>
        <p class="error">{error()}</p>
      </Show>
      <Show
        when={!installed.error && !libs.error}
        fallback={<p class="error">Could not load the org's libraries.</p>}
      >
        <Show when={(!installed.loading && !libs.loading) || (lib() && libs())} fallback={<p class="empty">loading…</p>}>
          <Show
            when={lib() && libs() ? { l: lib()!, all: libs()! } : undefined}
            fallback={
              <p class="empty">
                There is no library <b class="mono">{props.id}</b> installed in {props.org}.{" "}
                <A href={back()}>All libraries</A>
              </p>
            }
          >
            {(x) => (
              <Installation
                org={props.org}
                lib={x().l}
                libs={x().all}
                onDone={done}
                onError={setError}
                onRemoved={() => nav(back())}
              />
            )}
          </Show>
        </Show>
      </Show>
    </>
  );
}

function Installation(props: {
  org: string;
  lib: LibraryInstallation;
  libs: OrgLibraries;
  onDone: (msg: string) => void;
  onError: (msg: string) => void;
  onRemoved: () => void;
}) {
  const l = () => props.lib;
  const trust = () => props.libs.trusted_sources.find((t) => t.source === trustKey(l().source));
  const [upgrading, setUpgrading] = createSignal(false);
  const [trusting, setTrusting] = createSignal(false);
  const [untrusting, setUntrusting] = createSignal(false);
  const [yanking, setYanking] = createSignal(false);
  const [removing, setRemoving] = createSignal(false);

  const unyank = async () => {
    try {
      await unyankLibrary(props.org, l().content_hash);
      props.onDone(`Version ${shortHash(l().content_hash)} can run again.`);
    } catch (e) {
      props.onError(e instanceof Error ? e.message : "Could not lift the yank.");
    }
  };

  return (
    <>
      <div class="run-toolbar">
        <button class="btn btn-primary btn-sm" onClick={() => setUpgrading(true)}>
          <Icon icon="arrow-up" size={14} /> Upgrade
        </button>
      </div>

      <Section title="Library" aside={<Chips chips={libraryChips(l(), props.libs.trusted_sources)} />}>
        <dl class="kv">
          <dt>Name</dt>
          <dd class="m">{libraryId(l())}</dd>
          <dt>Version</dt>
          <dd>
            <span class="lib-hash" title={l().content_hash}>
              {shortHash(l().content_hash)}
            </span>
          </dd>
          <dt>Source</dt>
          <dd class="m">{l().source}</dd>
          <dt>Installed</dt>
          <dd title={absTime(l().installed_at)}>
            by <span class="mono">{l().installed_by}</span> · {relTime(l().installed_at)}
          </dd>
        </dl>
      </Section>

      <Section
        title="Forge credentials"
        foot={
          <Show
            when={trust()}
            fallback={
              <button class="btn btn-ghost btn-sm" onClick={() => setTrusting(true)}>
                <Icon icon="key-round" size={14} /> Trust source
              </button>
            }
          >
            <button class="btn btn-ghost btn-sm" onClick={() => setUntrusting(true)}>
              Stop trusting
            </button>
          </Show>
        }
      >
        <dl class="kv">
          <dt>Source</dt>
          <dd class="m">{trustKey(l().source)}</dd>
          <dt>Trust</dt>
          <dd>
            <Show
              when={trust()}
              fallback="Not trusted. A Step from this library that asks for forge_token does not receive a long-lived token."
            >
              {(t) => (
                <span title={absTime(t().trusted_at)}>
                  Trusted by <span class="mono">{t().trusted_by}</span> · {relTime(t().trusted_at)}. It holds
                  across upgrades from the same file.
                </span>
              )}
            </Show>
          </dd>
        </dl>
      </Section>

      <Section
        title="Yank"
        foot={
          <Show
            when={l().yanked}
            fallback={
              <button class="btn btn-danger btn-sm" onClick={() => setYanking(true)}>
                <Icon icon="ban" size={14} /> Yank this version
              </button>
            }
          >
            <button class="btn btn-ghost btn-sm" onClick={() => void unyank()}>
              Lift yank
            </button>
          </Show>
        }
      >
        <dl class="kv">
          <Show
            when={l().yanked}
            fallback={
              <>
                <dt>State</dt>
                <dd>Not yanked. Yanking stops this version from running in {props.org}.</dd>
              </>
            }
          >
            {(y) => (
              <>
                <dt>State</dt>
                <dd class="lib-notice yank" title={absTime(y().yanked_at)}>
                  Yanked by {y().yanked_by} · {relTime(y().yanked_at)}
                </dd>
                <dt>Reason</dt>
                <dd>{y().reason}</dd>
              </>
            )}
          </Show>
        </dl>
      </Section>

      <Section
        title="Remove"
        danger
        foot={
          <button class="btn btn-danger btn-sm" onClick={() => setRemoving(true)}>
            Remove library
          </button>
        }
      >
        <p class="kv-note kv-lead">Pipelines that invoke it stop compiling until it is installed again.</p>
      </Section>

      <Show when={upgrading()}>
        <InstallLibraryDialog
          org={props.org}
          upgrade
          initial={{ source: l().source, publisher: l().publisher, name: l().name }}
          onClose={() => setUpgrading(false)}
        />
      </Show>
      <Show when={yanking()}>
        <YankDialog
          org={props.org}
          library={l()}
          onClose={() => setYanking(false)}
          onYanked={(msg) => {
            setYanking(false);
            props.onDone(msg);
          }}
        />
      </Show>
      <Show when={removing()}>
        <ConfirmDestructive
          eyebrow={`${props.org} · libraries`}
          title={`Remove ${libraryId(l())}?`}
          purpose="Pipelines that invoke it stop compiling until it is installed again."
          name={libraryId(l())}
          confirmLabel="Remove library"
          onConfirm={async () => {
            await removeLibrary(props.org, l().publisher, l().name);
            props.onRemoved();
          }}
          onClose={() => setRemoving(false)}
        >
          <p>Runs already created keep the Steps they were compiled with.</p>
        </ConfirmDestructive>
      </Show>
      <Show when={trusting()}>
        <ConfirmAction
          eyebrow={`${props.org} · libraries`}
          title="Trust this source with forge credentials?"
          purpose={<span class="mono">{trustKey(l().source)}</span>}
          confirmLabel="Trust source"
          onConfirm={async () => {
            await trustLibrarySource(props.org, trustKey(l().source));
            props.onDone(`${trustKey(l().source)} is trusted with forge credentials.`);
          }}
          onClose={() => setTrusting(false)}
        >
          <p>
            A Step from any version installed from this file that asks for{" "}
            <span class="mono">forge_token</span> may receive a long-lived token of a Forgejo or
            GitHub PAT connection. Trust holds across upgrades from the same file; review each
            upgrade's diff. This asks you to confirm with your passkey.
          </p>
        </ConfirmAction>
      </Show>
      <Show when={untrusting()}>
        <ConfirmAction
          eyebrow={`${props.org} · libraries`}
          title="Stop trusting this source?"
          purpose={<span class="mono">{trustKey(l().source)}</span>}
          confirmLabel="Stop trusting"
          danger
          onConfirm={async () => {
            await untrustLibrarySource(props.org, trustKey(l().source));
            props.onDone(`${trustKey(l().source)} is no longer trusted with forge credentials.`);
          }}
          onClose={() => setUntrusting(false)}
        >
          <p>
            Runs created from now on refuse a Step from this source that asks for a long-lived forge
            token.
          </p>
        </ConfirmAction>
      </Show>
    </>
  );
}

/** A panel of the manage page: a heading with an optional tag on the right,
 *  a label and value grid, and an optional footer row of actions. */
export function Section(props: {
  title: string;
  aside?: JSX.Element;
  foot?: JSX.Element;
  /** Marks a panel whose action cannot be undone. */
  danger?: boolean;
  children: JSX.Element;
}) {
  return (
    <div class="panel" classList={{ "env-danger": !!props.danger }}>
      <div class="panel-h">
        <span>{props.title}</span>
        <Show when={props.aside}>{props.aside}</Show>
      </div>
      {props.children}
      <Show when={props.foot}>
        <div class="panel-foot">{props.foot}</div>
      </Show>
    </div>
  );
}

/** Yank a version: the Runs in progress that use it, what happens, a
 *  required reason, and an opt-in cancel of those Runs. */
function YankDialog(props: {
  org: string;
  library: LibraryInstallation;
  onClose: () => void;
  onYanked: (msg: string) => void;
}) {
  const l = () => props.library;
  const [impact] = createResource(() => yankImpact(props.org, l().content_hash));
  const [reason, setReason] = createSignal("");
  const [cancel, setCancel] = createSignal(false);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [reasonError, setReasonError] = createSignal<string | null>(null);
  const runs = () => impact()?.runs ?? [];

  const submit = async () => {
    const problem = yankReasonError(reason());
    setReasonError(problem);
    if (problem) return;
    setBusy(true);
    setError(null);
    try {
      const y = await yankLibrary(props.org, l().content_hash, reason().trim(), cancel() && runs().length > 0);
      const cancelled = y.cancelled_runs?.length ?? 0;
      props.onYanked(
        `Version ${shortHash(l().content_hash)} is yanked in ${props.org}.` +
          (cancelled ? ` ${cancelled} ${cancelled === 1 ? "Run was" : "Runs were"} cancelled.` : ""),
      );
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not yank the version.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      size="wide"
      icon="ban"
      eyebrow={`${props.org} · libraries`}
      title={
        <>
          Yank {libraryId(l())} <span class="mono">{shortHash(l().content_hash)}</span>
        </>
      }
      purpose="Stop this version from running in this org."
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
          <button
            class="btn btn-danger"
            type="button"
            disabled={busy() || reason().trim() === ""}
            onClick={() => void submit()}
          >
            <Icon icon="ban" size={14} /> {busy() ? "Yanking…" : yankButtonLabel(cancel(), runs().length)}
          </button>
        </>
      }
    >
      <div class="yk-impact">
        <Show when={!impact.loading} fallback={<div class="yk-count subtle">Counting the Runs that use it…</div>}>
          <Show when={impact()} fallback={<div class="yk-count subtle">Could not count the Runs that use it.</div>}>
            {(i) => (
              <div class="yk-count">
                {yankImpactLine(i())}
                <Show when={runs().length > 0}>
                  <span class="runs">
                    <For each={runs()}>
                      {(r, k) => (
                        <>
                          {k() > 0 ? " · " : ""}
                          <A href={`/${props.org}/${r.project}/runs/${r.id}`}>{impactRunLabel(r)}</A>
                          {r.running_steps.length > 0 ? ` (${r.running_steps.join(", ")} running)` : ""}
                        </>
                      )}
                    </For>
                  </span>
                </Show>
              </div>
            )}
          </Show>
        </Show>
        <ul class="yk-lines">
          <li>
            <span>
              {cancel() && runs().length > 0
                ? "The Runs above are cancelled, and their running Steps stop."
                : "A Step that is running finishes."}
            </span>
          </li>
          <li>
            <span>
              A later Step from this version fails as a config error, and Steps that need it do not
              run. Gates from this version cannot be approved; a timer gate fails.
            </span>
          </li>
          <li>
            <span>New Runs and Reruns that use this version are refused.</span>
          </li>
        </ul>
        <Show when={runs().length > 0}>
          <Checkbox
            checked={cancel()}
            onChecked={setCancel}
            label={`Cancel ${runs().length === 1 ? "this Run" : `these ${runs().length} Runs`} now`}
          />
        </Show>
      </div>
      <Field
        label="Reason"
        required
        help="Shown with every refusal. Other orgs on this server are not affected. Yanking asks for no passkey; lifting a yank does."
        error={reasonError()}
      >
        <Textarea rows={3} autofocus value={reason()} onValue={setReason} />
      </Field>
    </Modal>
  );
}
