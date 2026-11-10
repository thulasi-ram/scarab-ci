// Libraries — the Settings section where an org admin
// installs a library from a git source, upgrades it after reading the diff,
// removes it, and yanks a version.
//
// The panel is a table of installations. Installing and upgrading happen on
// their own page (`/orgs/<org>/settings/libraries/review`, routes/LibraryReview.tsx):
// fetch, read the diff and the checks, then install, which asks for a
// step-up. Removing and yanking tighten and ask for nothing; lifting a yank
// asks for a step-up. Yanking opens a dialog that says which Runs in
// progress use the version and can cancel them.
//
// Every library installs through the review page, whoever publishes it; the
// libraries Scarab's release manifest lists are offered there as sources.
// A new org gets the manifest's defaults when its first repo is bound; if
// their source could not be read, the empty state says what is missing, and
// the admin can retry, point at a mirror, or paste a copy (checked against
// the manifest's hash).
import { createResource, createSignal, For, Show } from "solid-js";
import { A, useNavigate } from "@solidjs/router";
import {
  listLibraries,
  removeLibrary,
  retryLibraryDefaults,
  unyankLibrary,
  yankImpact,
  yankLibrary,
  type LibraryInstallation,
  type OrgLibraries,
} from "../api/client";
import {
  missingLabel,
  reviewHref,
  shortHash,
  trustNote,
  impactRunLabel,
  updateLine,
  yankButtonLabel,
  yankImpactLine,
  yankReasonError,
} from "../libraries";
import { relTime, absTime } from "../fmt";
import Doodle from "./Doodle";
import Icon from "./Icon";
import Modal, { ModalMessage } from "./Modal";
import ConfirmDestructive from "./ConfirmDestructive";
import { Checkbox, Field, Input, Textarea } from "./Field";

export default function Libraries(props: { org: string }) {
  const nav = useNavigate();
  const [epoch, setEpoch] = createSignal(0);
  const [libs] = createResource(
    () => ({ org: props.org, epoch: epoch() }),
    (k) => listLibraries(k.org),
  );
  const [yanking, setYanking] = createSignal<LibraryInstallation | null>(null);
  const [error, setError] = createSignal<string | null>(null);
  const [notice, setNotice] = createSignal<string | null>(null);
  const reload = () => setEpoch((e) => e + 1);

  const unavailable = () => !libs.loading && !libs.error && libs() === null;
  const rows = () => libs()?.installations ?? [];
  const missing = () => libs()?.defaults?.state === "missing";

  const act = async (what: () => Promise<unknown>, fallback: string, done?: string) => {
    setError(null);
    setNotice(null);
    try {
      await what();
      if (done) setNotice(done);
      reload();
    } catch (e) {
      setError(e instanceof Error ? e.message : fallback);
    }
  };

  // The library the confirm dialog is about to remove. A refusal shows in the
  // dialog.
  const [removing, setRemoving] = createSignal<LibraryInstallation | null>(null);
  const libId = (l: LibraryInstallation) => `lib:${l.publisher}/${l.name}`;
  const remove = async (l: LibraryInstallation) => {
    setError(null);
    setNotice(null);
    await removeLibrary(props.org, l.publisher, l.name);
    setNotice(`${libId(l)} is removed.`);
    reload();
  };

  const unyank = (hash: string) =>
    void act(
      () => unyankLibrary(props.org, hash),
      "Could not lift the yank.",
      `Version ${shortHash(hash)} can run again.`,
    );

  return (
    <div class="panel">
      <div class="panel-h">
        <span>Libraries · {props.org}</span>
        <span class="tok-count">{rows().length}</span>
      </div>
      <div class="secrets-body">
        <Show when={libs.error}>
          <p class="error">Could not load the org's libraries.</p>
        </Show>
        <Show when={error()}>
          <p class="error">{error()}</p>
        </Show>
        <Show when={notice()}>
          <p class="notice-ok">{notice()}</p>
        </Show>

        <Show when={!libs.loading} fallback={<p class="empty">loading…</p>}>
          <Show when={!unavailable()} fallback={<p class="empty">This server does not manage libraries.</p>}>
            <Show when={libs()?.defaults?.state === "pending"}>
              <p class="subtle">
                <Icon icon="rotate-cw" size={12} /> Installing the first-party libraries…
              </p>
            </Show>
            <Show when={libs()?.manifest_error}>
              {(e) => (
                <p class="subtle">
                  <small>
                    <Icon icon="alert-triangle" size={12} /> The release manifest could not be read,
                    so first-party libraries are not marked: {e()}
                  </small>
                </p>
              )}
            </Show>

            <Show when={missing()}>
              <div class="lib-well">
                <Doodle icon="package" size={104} opacity={0.3} />
                <MissingDefaults
                  org={props.org}
                  libs={libs()!}
                  onDone={(msg) => {
                    setError(null);
                    setNotice(msg);
                    reload();
                  }}
                />
              </div>
            </Show>

            <Show
              when={rows().length > 0}
              fallback={
                <Show when={!missing()}>
                  <div class="lib-well">
                    <Doodle icon="package" size={104} opacity={0.3} />
                    <div class="lib-empty">
                      <div class="t1">
                        <Icon icon="package" size={14} /> No libraries installed
                      </div>
                      <p class="t2">
                        A Pipeline uses an installed library with{" "}
                        <span class="mono">invoke: lib:&lt;publisher&gt;/&lt;name&gt;</span>. Checking
                        out a repository needs <span class="mono">lib:thulasi-ram/clone</span>.
                      </p>
                      <div class="acts">
                        <button class="btn btn-primary btn-sm" onClick={() => nav(reviewHref(props.org, {}))}>
                          <Icon icon="plus" size={14} /> Install library
                        </button>
                      </div>
                    </div>
                  </div>
                </Show>
              }
            >
              <table class="table lib-table">
                <thead>
                  <tr>
                    <th>Library</th>
                    <th>Version</th>
                    <th>Source</th>
                    <th>Installed</th>
                    <th />
                  </tr>
                </thead>
                <tbody>
                  <For each={rows()}>
                    {(l) => (
                      <LibraryRow
                        library={l}
                        trust={trustNote(l, libs()?.trusted_sources ?? [])}
                        onUpgrade={(source) =>
                          nav(reviewHref(props.org, { source: source ?? l.source, publisher: l.publisher, name: l.name }))
                        }
                        onRemove={() => setRemoving(l)}
                        onYank={() => setYanking(l)}
                        onUnyank={() => unyank(l.content_hash)}
                      />
                    )}
                  </For>
                </tbody>
              </table>
            </Show>

            <Show when={(libs()?.yanks ?? []).some((y) => !rows().some((r) => r.content_hash === y.content_hash))}>
              <p class="subtle">Yanked versions not installed now:</p>
              <ul class="tok-list">
                <For each={(libs()?.yanks ?? []).filter((y) => !rows().some((r) => r.content_hash === y.content_hash))}>
                  {(y) => (
                    <li class="tok tok-dead">
                      <div class="tok-head">
                        <span class="mono">{shortHash(y.content_hash)}</span>
                        <span class="plg-msg">{y.reason}</span>
                        <span class="subtle" title={absTime(y.yanked_at)}>
                          by {y.yanked_by} · {relTime(y.yanked_at)}
                        </span>
                        <span class="oenv-actions">
                          <button class="btn btn-ghost btn-sm" onClick={() => unyank(y.content_hash)}>
                            lift yank
                          </button>
                        </span>
                      </div>
                    </li>
                  )}
                </For>
              </ul>
            </Show>

            <Show when={(libs()?.trusted_sources ?? []).length > 0}>
              <p class="subtle">
                Trusted with forge credentials, besides the first-party clone library:
              </p>
              <ul class="tok-list">
                <For each={libs()?.trusted_sources ?? []}>
                  {(t) => (
                    <li class="tok">
                      <div class="tok-head">
                        <span class="mono">{t.source}</span>
                        <span class="subtle" title={absTime(t.trusted_at)}>
                          by {t.trusted_by} · {relTime(t.trusted_at)}
                        </span>
                      </div>
                    </li>
                  )}
                </For>
              </ul>
            </Show>

            <Show when={rows().length > 0}>
              <div class="secrets-actions">
                <button class="btn btn-primary btn-sm" onClick={() => nav(reviewHref(props.org, {}))}>
                  <Icon icon="plus" size={14} /> Install library
                </button>
              </div>
            </Show>
            <p class="subtle">
              <small>
                <Icon icon="shield-check" size={12} /> An installation is one file at one commit,
                stored in Scarab: a Pipeline never fetches it. Installing and upgrading, and lifting
                a yank, ask you to confirm with your passkey.
              </small>
            </p>
          </Show>
        </Show>
      </div>

      <Show when={yanking()}>
        {(l) => (
          <YankDialog
            org={props.org}
            library={l()}
            onClose={() => setYanking(null)}
            onYanked={(msg, upgrade) => {
              setYanking(null);
              setError(null);
              setNotice(msg);
              reload();
              if (upgrade) nav(reviewHref(props.org, { source: upgrade, publisher: l().publisher, name: l().name }));
            }}
          />
        )}
      </Show>
      <Show when={removing()}>
        {(l) => (
          <ConfirmDestructive
            eyebrow={`${props.org} · libraries`}
            title={`Remove ${libId(l())}?`}
            purpose="Pipelines that invoke it stop compiling until it is installed again."
            name={libId(l())}
            confirmLabel="Remove library"
            onConfirm={() => remove(l())}
            onClose={() => setRemoving(null)}
          >
            <p>Runs already created keep the Steps they were compiled with.</p>
          </ConfirmDestructive>
        )}
      </Show>
    </div>
  );
}

/** One installation: two table rows, the second only for its notices. */
function LibraryRow(props: {
  library: LibraryInstallation;
  trust: string | null;
  onUpgrade: (source?: string) => void;
  onRemove: () => void;
  onYank: () => void;
  onUnyank: () => void;
}) {
  const l = () => props.library;
  const update = () => updateLine(l().update);
  const hasNotice = () => !!l().yanked || !!update();
  return (
    <>
      <tr classList={{ dead: !!l().yanked }}>
        <td>
          <div class="mono" style={{ color: "var(--soft-white)" }}>
            lib:{l().publisher}/{l().name}
          </div>
          <div class="lib-badges" style={{ "margin-top": "4px" }}>
            <Show when={l().first_party}>
              <span class="tok-role" title="This version is listed in Scarab's release manifest.">
                first-party
              </span>
            </Show>
            <Show when={props.trust}>
              {(t) => (
                <span class="tok-state lib-trust" title={t()}>
                  forge_token
                </span>
              )}
            </Show>
            <Show when={l().yanked}>
              <span class="tok-state tok-revoked">yanked</span>
            </Show>
            <Show when={l().update}>
              {(u) => (
                <span class={`tok-state ${u().security_fix ? "lib-sec" : "lib-upd"}`}>
                  {u().security_fix ? "security fix" : "update"}
                </span>
              )}
            </Show>
          </div>
        </td>
        <td>
          <span class="lib-hash" title={l().content_hash}>
            {shortHash(l().content_hash)}
          </span>
        </td>
        <td class="lib-src">{l().source}</td>
        <td class="mono" title={absTime(l().installed_at)}>
          {l().installed_by}
          <br />
          <span class="subtle">{relTime(l().installed_at)}</span>
        </td>
        <td class="acts keep">
          <button class="btn btn-ghost btn-sm" onClick={() => props.onUpgrade()}>
            upgrade
          </button>
          <Show when={!l().yanked}>
            <button class="btn btn-danger btn-sm" onClick={props.onYank}>
              yank
            </button>
          </Show>
          <button class="btn btn-ghost btn-sm" onClick={props.onRemove}>
            remove
          </button>
        </td>
      </tr>
      <Show when={hasNotice()}>
        <tr class="sub">
          <td colspan={5} class="keep">
            <Show when={l().yanked}>
              {(y) => (
                <div class="lib-notice yank" title={absTime(y().yanked_at)}>
                  <Icon icon="alert-triangle" size={12} /> yanked by {y().yanked_by} ·{" "}
                  {relTime(y().yanked_at)}: {y().reason}
                  <button class="btn btn-ghost btn-sm" onClick={props.onUnyank}>
                    lift yank
                  </button>
                </div>
              )}
            </Show>
            <Show when={update()}>
              {(line) => (
                <div class={`lib-notice ${l().update?.security_fix ? "sec" : "upd"}`}>
                  <Icon icon={l().update?.security_fix ? "alert-triangle" : "arrow-up"} size={12} />{" "}
                  {line()} <span class="lib-hash">{shortHash(l().update?.content_hash ?? "")}</span>
                  <button class="btn btn-ghost btn-sm" onClick={() => props.onUpgrade(l().update?.source)}>
                    review the upgrade
                  </button>
                </div>
              )}
            </Show>
          </td>
        </tr>
      </Show>
    </>
  );
}

/** A new org whose first-party defaults could not be read: retry, a mirror,
 *  or pasted copies. Drawn inside the empty-state well. */
function MissingDefaults(props: { org: string; libs: OrgLibraries; onDone: (msg: string) => void }) {
  const d = () => props.libs.defaults!;
  const [pane, setPane] = createSignal<"" | "mirror" | "paste">("");
  const [mirror, setMirror] = createSignal(d().mirror ?? "");
  const [copies, setCopies] = createSignal<Record<string, string>>({});
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const named = () => d().missing.filter((m) => m.name);
  const release = () => props.libs.first_party[0]?.release;

  const retry = async (from: "source" | "mirror" | "paste") => {
    if (busy()) return;
    setBusy(true);
    setError(null);
    try {
      const files =
        from === "paste"
          ? Object.fromEntries(Object.entries(copies()).filter(([, v]) => v.trim() !== ""))
          : {};
      const r = await retryLibraryDefaults(props.org, {
        mirror: from === "mirror" ? mirror().trim() || null : null,
        files,
      });
      props.onDone(
        r.state === "installed"
          ? "The first-party libraries are installed."
          : "Some first-party libraries are still missing; see the errors.",
      );
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not install the first-party libraries.");
    } finally {
      setBusy(false);
    }
  };
  const toggle = (p: "mirror" | "paste") => setPane((c) => (c === p ? "" : p));

  return (
    <div class="lib-empty">
      <div class="t1 warn">
        <Icon icon="alert-triangle" size={14} /> First-party libraries are not installed
      </div>
      <p class="t2">
        Scarab could not read them. Until <span class="mono">lib:thulasi-ram/clone</span> is
        installed, every Pipeline that checks out fails to compile.
      </p>
      <div class="acts">
        <button class="btn btn-primary btn-sm" disabled={busy()} onClick={() => void retry("source")}>
          <Icon icon="rotate-cw" size={14} /> {busy() ? "Installing…" : "Retry"}
        </button>
        <button class="btn btn-ghost btn-sm" classList={{ on: pane() === "mirror" }} onClick={() => toggle("mirror")}>
          Use a mirror
        </button>
        <Show when={named().length > 0}>
          <button class="btn btn-ghost btn-sm" classList={{ on: pane() === "paste" }} onClick={() => toggle("paste")}>
            Paste the files
          </button>
        </Show>
      </div>
      <details class="lib-errs">
        <summary>
          {d().missing.length} {d().missing.length === 1 ? "file failed" : "files failed"} · show errors
        </summary>
        <pre>{d().missing.map((m) => `${missingLabel(m)}  ${m.error}`).join("\n")}</pre>
      </details>
      <Show when={pane() === "mirror"}>
        <div class="lib-pane">
          <Field
            label="Mirror of Scarab's repository"
            help="Each file is read at the same path and commit, and must have the hash the release lists."
          >
            <div class="row">
              <Input
                mono
                placeholder="git.example.com/mirrors/scarab-ci"
                value={mirror()}
                onValue={setMirror}
              />
              <button
                class="btn btn-primary btn-sm"
                disabled={busy() || mirror().trim() === ""}
                onClick={() => void retry("mirror")}
              >
                <Icon icon="rotate-cw" size={14} /> Read from mirror
              </button>
            </div>
          </Field>
        </div>
      </Show>
      <Show when={pane() === "paste"}>
        <div class="lib-pane">
          <For each={named()}>
            {(m) => (
              <Field plainLabel label={<span class="mono">{missingLabel(m)}</span>}>
                <Textarea
                  mono
                  rows={2}
                  placeholder={`paste the file from the ${release() ?? "release"}`}
                  value={copies()[`${m.publisher}/${m.name}`] ?? ""}
                  onValue={(v) => setCopies((c) => ({ ...c, [`${m.publisher}/${m.name}`]: v }))}
                />
              </Field>
            )}
          </For>
          <p class="f-help" style={{ margin: 0 }}>
            Scarab checks each pasted file against the hash the release lists before it installs it.
          </p>
          <div>
            <button class="btn btn-primary btn-sm" disabled={busy()} onClick={() => void retry("paste")}>
              <Icon icon="plus" size={14} /> Install pasted files
            </button>
          </div>
        </div>
      </Show>
      <Show when={error()}>
        <p class="error">{error()}</p>
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
  onYanked: (msg: string, upgradeSource: string | null) => void;
}) {
  const l = () => props.library;
  const [impact] = createResource(() => yankImpact(props.org, l().content_hash));
  const [reason, setReason] = createSignal("");
  const [cancel, setCancel] = createSignal(false);
  const [upgrade, setUpgrade] = createSignal(!!l().update);
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
        upgrade() ? (l().update?.source ?? null) : null,
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
          Yank lib:{l().publisher}/{l().name} <span class="mono">{shortHash(l().content_hash)}</span>
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
      <Show when={l().update}>
        {(u) => (
          <Checkbox
            checked={upgrade()}
            onChecked={setUpgrade}
            label={
              <>
                Then review the upgrade to <span class="lib-hash">{shortHash(u().content_hash)}</span>
              </>
            }
            help={`${u().release}; asks for your passkey.`}
          />
        )}
      </Show>
    </Modal>
  );
}
