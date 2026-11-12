// Libraries: the Settings section listing an org's installed libraries, one
// row each, with its version, its state tags and a link to its own page.
//
// Install, top right, opens a dialog for the source; the file is then read
// and reviewed on its own page (`…/settings/libraries/review`,
// routes/LibraryReview.tsx) before it is installed. Everything done to one
// installation (upgrade, trust its source, yank, remove) is on its manage
// page (`…/settings/libraries/<id>/manage`,
// routes/LibraryManage.tsx).
//
// Two things belong to no installation, so they stay here: a yank of a
// library with no installation at all (each library's manage page lists the
// yanks of its own versions, earlier ones included), and trust in a source
// no installation comes from. Lifting a yank asks for a step-up; stopping
// trust does not.
import { createResource, createSignal, For, Show } from "solid-js";
import { A } from "@solidjs/router";
import { listLibraries, untrustLibrarySource, unyankLibrary, type TrustedSource } from "../api/client";
import {
  libraryChips,
  libraryId,
  libraryManageHref,
  removedLibraryYanks,
  shortHash,
  strayTrust,
  grantLabel,
  untrustEffect,
  versionLabel,
} from "../libraries";
import { relTime, absTime } from "../fmt";
import Chips from "./Chips";
import Icon from "./Icon";
import InstallLibraryDialog from "./LibraryInstall";
import { ConfirmAction } from "./ConfirmDestructive";

export default function Libraries(props: { org: string }) {
  const [epoch, setEpoch] = createSignal(0);
  const [libs] = createResource(
    () => ({ org: props.org, epoch: epoch() }),
    (k) => listLibraries(k.org),
  );
  const [installing, setInstalling] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [notice, setNotice] = createSignal<string | null>(null);
  const [untrusting, setUntrusting] = createSignal<TrustedSource | null>(null);
  const reload = () => setEpoch((e) => e + 1);

  const data = () => (libs.error ? undefined : libs());
  const unavailable = () => !libs.loading && !libs.error && libs() === null;
  const rows = () => data()?.installations ?? [];
  const trusted = () => data()?.trusted_sources ?? [];
  const yanks = () => removedLibraryYanks(data()?.yanks ?? [], rows());
  const loneTrust = () => strayTrust(trusted(), rows());

  const unyank = async (hash: string) => {
    setError(null);
    setNotice(null);
    try {
      await unyankLibrary(props.org, hash);
      setNotice(`Version ${shortHash(hash)} can run again.`);
      reload();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not lift the yank.");
    }
  };

  return (
    <div class="panel">
      <div class="panel-h">
        <span>
          Libraries · {props.org} <span class="tok-count">{rows().length}</span>
        </span>
        <Show when={!unavailable()}>
          <button class="btn btn-primary btn-sm" onClick={() => setInstalling(true)}>
            <Icon icon="plus" size={14} /> Install library
          </button>
        </Show>
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

        <Show when={!libs.loading || data()} fallback={<p class="empty">loading…</p>}>
          <Show when={!unavailable()} fallback={<p class="empty">This server does not manage libraries.</p>}>
            <Show
              when={rows().length > 0}
              fallback={
                <p class="empty">
                  No libraries installed. A library is a file of Steps that a Pipeline runs with{" "}
                  <span class="mono">invoke: lib:&lt;name&gt;</span>. To install one,
                  choose Install library and enter the file and the branch, tag or commit to read it at.
                </p>
              }
            >
              <div class="table-scroll">
                <table class="table lib-table" aria-label="Installed libraries">
                  <thead>
                    <tr>
                      <th>Library</th>
                      <th>Version</th>
                      <th>State</th>
                      <th>Installed</th>
                      <th />
                    </tr>
                  </thead>
                  <tbody>
                    <For each={rows()}>
                      {(l) => (
                        <tr classList={{ dead: !!l.yanked }}>
                          <td>
                            <A class="mono oenv-link" href={libraryManageHref(props.org, l)}>
                              {libraryId(l)}
                            </A>
                          </td>
                          <td>
                            <span class="lib-hash" title={l.content_hash}>
                              {shortHash(l.content_hash)}
                            </span>
                          </td>
                          <td class="keep">
                            <Chips chips={libraryChips(l, trusted())} />
                          </td>
                          <td title={absTime(l.installed_at)}>
                            <span class="mono">{l.installed_by}</span>{" "}
                            <span class="subtle">· {relTime(l.installed_at)}</span>
                          </td>
                          <td class="acts keep">
                            <A class="btn btn-ghost btn-sm" href={libraryManageHref(props.org, l)}>
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

            <Show when={yanks().length > 0}>
              <h3 class="lib-sub-h">Yanks of removed libraries</h3>
              <ul class="tok-list" aria-label="Yanks of removed libraries">
                <For each={yanks()}>
                  {(y) => (
                    <li class="tok">
                      <div class="tok-head">
                        <span class="tok-name mono">{libraryId(y)}</span>
                        <span class="mono lib-src" title={y.source}>
                          {versionLabel(y.source)}
                        </span>
                        <span class="oenv-actions">
                          <button class="btn btn-ghost btn-sm" onClick={() => void unyank(y.content_hash)}>
                            Lift yank
                          </button>
                        </span>
                      </div>
                      <div class="tok-facts" title={absTime(y.yanked_at)}>
                        <span>{y.reason}</span>
                        <span>
                          yanked by {y.yanked_by} · {relTime(y.yanked_at)}
                        </span>
                      </div>
                    </li>
                  )}
                </For>
              </ul>
            </Show>

            <Show when={loneTrust().length > 0}>
              <h3 class="lib-sub-h">Trusted sources no library comes from now</h3>
              <ul class="tok-list" aria-label="Trusted sources no library comes from now">
                <For each={loneTrust()}>
                  {(t) => (
                    <li class="tok">
                      <div class="tok-head">
                        <span class="mono lib-src">{t.source}</span>
                        <span class="oenv-actions">
                          <button class="btn btn-ghost btn-sm" onClick={() => setUntrusting(t)}>
                            Stop trusting
                          </button>
                        </span>
                      </div>
                      <div class="tok-facts" title={absTime(t.trusted_at)}>
                        <span>
                          {grantLabel(t.grant)} · trusted by {t.trusted_by} · {relTime(t.trusted_at)}
                        </span>
                      </div>
                    </li>
                  )}
                </For>
              </ul>
            </Show>

            <p class="subtle">
              <small>
                <Icon icon="shield-check" size={12} /> An installation is one file at one commit,
                stored in Scarab: a Pipeline never fetches it. No library receives a long-lived forge
                token until the org trusts its source. Installing and upgrading, trusting a source,
                and lifting a yank ask you to confirm with your passkey.
              </small>
            </p>
          </Show>
        </Show>
      </div>

      <Show when={installing()}>
        <InstallLibraryDialog org={props.org} onClose={() => setInstalling(false)} />
      </Show>
      <Show when={untrusting()}>
        {(t) => (
          <ConfirmAction
            eyebrow={`${props.org} · libraries`}
            title="Stop trusting this source?"
            purpose={<span class="mono">{t().source}</span>}
            confirmLabel="Stop trusting"
            danger
            onConfirm={async () => {
              await untrustLibrarySource(props.org, t().source, t().grant);
              setError(null);
              setNotice(`${t().source} is no longer trusted with ${grantLabel(t().grant)}.`);
              reload();
            }}
            onClose={() => setUntrusting(null)}
          >
            <p>{untrustEffect(t().grant)}</p>
          </ConfirmAction>
        )}
      </Show>
    </div>
  );
}
