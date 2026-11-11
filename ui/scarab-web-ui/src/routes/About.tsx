// The About page, opened from the user menu: which Scarab this is and how the
// deployment runs, read from `GET /v1/about`, with the links someone reporting
// a problem needs: the release notes for this version, the commit it was built
// from, the docs, the API description and the source.
import { Show, createResource } from "solid-js";
import { aboutServer, type About } from "../api/client";
import { absTime, relTime } from "../fmt";
import { HomeCrumb } from "../components/PageCrumbs";

const REPO = "https://github.com/thulasi-ram/scarab-ci";
const DOCS = "https://thulasi-ram.github.io/scarab-ci/";

const ROLES: Record<string, string> = {
  converged: "converged: API, scheduler and executor in one process",
  api: "API only",
  scheduler: "scheduler only",
  executor: "executor only",
};

const EXECUTORS: Record<string, string> = {
  k8s: "Kubernetes: one Pod per step",
  local: "local processes on the server's host",
};

const onOff = (on: boolean, yes: string, no: string) => (on ? yes : no);

export default function AboutPage() {
  const [about] = createResource(aboutServer);
  return (
    <section class="page">
      <div class="page-head">
        <h1 class="crumb-head">
          <HomeCrumb size={20} sepClass="crumb-head-sep" />
          <span>About Scarab</span>
        </h1>
      </div>
      <p class="page-sub">the server's version and how this deployment runs</p>
      <Show
        when={about()}
        fallback={
          <p class="subtle">
            {about.error
              ? `Could not read the server's version: ${(about.error as Error).message}`
              : "loading…"}
          </p>
        }
      >
        {(a) => (
          <>
            <ServerPanel about={a()} />
            <LinksPanel about={a()} />
          </>
        )}
      </Show>
    </section>
  );
}

function ServerPanel(props: { about: About }) {
  const a = () => props.about;
  return (
    <div class="panel">
      <div class="panel-h">
        <span>This server</span>
      </div>
      <dl class="kv" aria-label="This server">
        <dt>Version</dt>
        <dd class="m">{a().version}</dd>
        <dt>Commit</dt>
        <dd>
          <Show
            when={a().commit}
            fallback={<span class="subtle">not recorded (a local build)</span>}
          >
            {(sha) => (
              <a
                class="mono"
                href={`${REPO}/commit/${sha()}`}
                target="_blank"
                rel="noreferrer"
                title={sha()}
              >
                {sha().slice(0, 12)}
              </a>
            )}
          </Show>
        </dd>
        <dt>Started</dt>
        <dd title={absTime(a().started_at)}>
          {absTime(a().started_at)}{" "}
          <span class="subtle">({relTime(a().started_at)})</span>
        </dd>
        <dt>Role</dt>
        <dd>
          {a().role ? (
            (ROLES[a().role!] ?? a().role)
          ) : (
            <span class="subtle">not recorded</span>
          )}
        </dd>
        <dt>Steps run on</dt>
        <dd>
          {a().executor ? (
            (EXECUTORS[a().executor!] ?? a().executor)
          ) : (
            <span class="subtle">not recorded</span>
          )}
        </dd>
        <dt>Sign-in</dt>
        <dd>
          {onOff(
            a().login,
            "through an OAuth or OIDC provider",
            "off: no sign-in provider is configured",
          )}
        </dd>
        <dt>GitHub App</dt>
        <dd>
          {onOff(
            a().github_app,
            "on: GitHub connections come from installing the App",
            "off",
          )}
        </dd>
        <dt>Plugins</dt>
        <dd>{onOff(a().plugins, "on", "off")}</dd>
      </dl>
    </div>
  );
}

function LinksPanel(props: { about: About }) {
  return (
    <div class="panel">
      <div class="panel-h">
        <span>Links</span>
      </div>
      <dl class="kv" aria-label="Links">
        <dt>Release notes</dt>
        <dd>
          <a
            href={`${REPO}/releases/tag/v${props.about.version}`}
            target="_blank"
            rel="noreferrer"
          >
            Scarab {props.about.version} on GitHub
          </a>
        </dd>
        <dt>Documentation</dt>
        <dd>
          <a href={DOCS} target="_blank" rel="noreferrer">
            {DOCS.replace("https://", "")}
          </a>
        </dd>
        <dt>API</dt>
        <dd>
          <a class="mono" href="/openapi.json" target="_blank" rel="noreferrer">
            /openapi.json
          </a>
          <span class="subtle"> · the OpenAPI description of this server</span>
        </dd>
        <dt>Source code</dt>
        <dd>
          <a href={REPO} target="_blank" rel="noreferrer">
            {REPO.replace("https://", "")}
          </a>
          <span class="subtle"> · AGPL-3.0-only</span>
        </dd>
      </dl>
    </div>
  );
}
