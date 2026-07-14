// Repositories — the landing tier. Enabled repos with a status dot, last-run
// provenance, and a run-history sparkline (representative until the forge/repo
// backend lands; see data/catalog). Clicking a card opens the repo's runs.
import { For } from "solid-js";
import { A } from "@solidjs/router";
import { listRepos } from "../data/catalog";
import Icon from "../components/Icon";
import Sparkline from "../components/Sparkline";
import Doodle from "../components/Doodle";

export default function Repos() {
  const repos = listRepos();
  return (
    <section class="page">
      <Doodle icon="boxes" size={240} rotate={-12} opacity={0.06} top="60px" right="40px" />

      <div class="page-head">
        <h1>Repositories</h1>
        <button class="btn btn-copper">
          <Icon icon="plus" size={15} /> Enable repository
        </button>
      </div>
      <p class="subtle page-sub">
        {repos.length} enabled · triggered by push, pull request, tag &amp; manual
      </p>

      <div class="repo-grid">
        <For each={repos}>
          {(r) => (
            <A href={`/${r.org}/${r.name}`} class="repo-card">
              <div class="repo-card-head">
                <span class={`sdot ${r.lastStatus}`} />
                <span class="repo-name">
                  <span class="repo-org">{r.org} /</span> {r.name}
                </span>
                <Icon icon="chevron-right" size={15} class="repo-go" />
              </div>
              <div class="repo-meta mono">
                <Icon icon="git-branch" size={12} /> {r.defaultBranch}
                <span class="dotsep">·</span>
                <span class={`facet ${r.lastStatus}`}>{r.lastStatus}</span>
              </div>
              <Sparkline runs={r.spark} />
            </A>
          )}
        </For>
      </div>
    </section>
  );
}
