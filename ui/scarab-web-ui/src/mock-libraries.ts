// ADR-0076 fixtures for mock mode (`just ui-mock`): libraries with every
// badge, a yank's impact, an upgrade to review, Runs whose Steps come from
// libraries (one with a yanked version), a plugin gate in each of its four
// states, a Pipeline that does not compile, and refused triggers in the Runs
// list. Page query parameters pick a variant, so each screen can be
// captured:
//
//   /settings?libs=empty | ?libs=missing        the two empty states
//   /acme/edge-gateway/runs/<PLUGIN>?gate=pass | exp | kill
//   /acme/orders-api/run                         the compile errors (pick release)
//   /acme/orders-api?status=refused              refused rows
import type { components } from "./api/schema";

type S = components["schemas"];
type RunStatusDto = S["RunStatusResponse"];
type StepStatusDto = S["StepStatusDto"];

const now = Date.now();
const min = 60_000;
const day = 86_400_000;
const ago = (ms: number) => now - ms;
const h = (prefix: string) => prefix.padEnd(64, "0");
const page = () => new URLSearchParams(window.location.search);

export const LIB_RUN_ID = "0190f8a2000071fb8c00a0760421aaaa";
export const YANK_RUN_ID = "0190f8a2000071fb8c00a0760096bbbb";
export const PLUGIN_RUN_ID = "0190f8a2000071fb8c00a0760422cccc";

const CLONE = h("a3c9e1f7b5d2");
const CLONE_NEW = h("b4d0f2a8c6e3");
const HELM = h("5d1e0c7a9b3f");
const HELM_NEW = h("0c4e7b2a91f5");
const PROBE = h("2c8f61d0a9e4");
const CLONE_SRC = "github.com/thulasi-ram/scarab-ci//libs/clone.yaml@1f0e9d8c7b6a5f4e3d2c1b0a9f8e7d6c5b4a3f2e";
const HELM_SRC = "github.com/acme/scarab-libs//deploy-helm.yaml@8c2f4e1a9d7b3c5e0f6a2b8d4c1e7f9a3b5d2c6e";
const HELM_NEW_SRC = "github.com/acme/scarab-libs//deploy-helm.yaml@d41f07b29c3e8a6f5b1d0c9e7a2f4b8c6d3e1a90";
const YANK_REASON =
  "scarab-clone 0.4.0 writes the forge token to the log when a fetch fails; upgrade to b4d0f2a8c6e3 (v0.4.1)";

// ── Settings › Libraries ─────────────────────────────────────────────────
const installation = (o: Partial<S["LibraryInstallationDto"]> & Pick<S["LibraryInstallationDto"], "publisher" | "name" | "content_hash" | "source">): S["LibraryInstallationDto"] => ({
  first_party: false,
  installed_by: "a.kim",
  installed_at: ago(6 * day),
  yanked: null,
  update: null,
  ...o,
});

const fullLibraries: S["OrgLibrariesDto"] = {
  installations: [
    installation({
      publisher: "thulasi-ram",
      name: "clone",
      content_hash: CLONE,
      source: CLONE_SRC,
      first_party: true,
      installed_by: "scarab:defaults",
      installed_at: ago(20 * day),
      update: { content_hash: CLONE_NEW, source: CLONE_SRC.replace(/@.*/, "@2a1b0c9d8e7f6a5b4c3d2e1f0a9b8c7d6e5f4a3b"), release: "v0.4.1", security_fix: true },
    }),
    installation({ publisher: "acme", name: "deploy-helm", content_hash: HELM, source: HELM_SRC }),
    installation({ publisher: "acme", name: "http-probe", content_hash: PROBE, source: "github.com/acme/scarab-libs//http-probe.yaml@8c2f4e1a9d7b3c5e0f6a2b8d4c1e7f9a3b5d2c6e", installed_by: "j.lee" }),
    installation({ publisher: "acme", name: "git-tag", content_hash: h("6a0d3e9f2b17"), source: "github.com/acme/scarab-libs//git-tag.yaml@51c3b8e0f2a4d6c8e0b2a4c6e8f0a2b4c6d8e0f2", installed_at: ago(3 * day) }),
    installation({
      publisher: "platform",
      name: "kaniko-build",
      content_hash: h("7e2b9a4c1d08"),
      source: "git.acme.dev/platform/scarab-libs//kaniko-build.yaml@0d9e6c2b7a15f3e1d9c7b5a3f1e9d7c5b3a1f9e7",
      installed_by: "j.lee",
      installed_at: ago(12 * day),
      yanked: {
        content_hash: h("7e2b9a4c1d08"),
        reason: "pins kaniko v1.23.0, which prints registry credentials when a push fails",
        yanked_by: "a.kim",
        yanked_at: ago(2 * 3_600_000),
      },
    }),
  ],
  yanks: [
    { content_hash: h("7e2b9a4c1d08"), reason: "pins kaniko v1.23.0, which prints registry credentials when a push fails", yanked_by: "a.kim", yanked_at: ago(2 * 3_600_000) },
    { content_hash: h("91f03ac2d7e5"), reason: "ran helm without --atomic; a failed upgrade left payments-api half rolled out", yanked_by: "a.kim", yanked_at: ago(9 * day) },
  ],
  first_party: [
    { publisher: "thulasi-ram", name: "clone", content_hash: CLONE_NEW, source: CLONE_SRC, release: "v0.4.1", security_fix: true, installed: true },
    { publisher: "thulasi-ram", name: "build-image", content_hash: h("c1d2e3f4a5b6"), source: "github.com/thulasi-ram/scarab-ci//libs/build-image.yaml@2a1b0c9d8e7f6a5b4c3d2e1f0a9b8c7d6e5f4a3b", release: "v0.4.1", security_fix: false, installed: false },
  ],
  manifest_error: null,
  defaults: { state: "installed", missing: [], mirror: null, updated_at: ago(20 * day) },
  trusted_sources: [{ source: "github.com/acme/scarab-libs//git-tag.yaml", trusted_by: "a.kim", trusted_at: ago(3 * day) }],
};

function orgLibraries(): S["OrgLibrariesDto"] {
  const v = page().get("libs");
  if (v === "empty") {
    return { ...fullLibraries, installations: [], yanks: [], trusted_sources: [], defaults: { state: "offered", missing: [], mirror: null, updated_at: 0 }, first_party: fullLibraries.first_party.map((f) => ({ ...f, installed: false })) };
  }
  if (v === "missing") {
    const err = (path: string) =>
      `GET https://github.com/thulasi-ram/scarab-ci/raw/1f0e9d8c7b6a…/libs/${path}: dial tcp 140.82.121.4:443: i/o timeout`;
    return {
      ...fullLibraries,
      installations: [],
      yanks: [],
      trusted_sources: [],
      first_party: fullLibraries.first_party.map((f) => ({ ...f, installed: false })),
      defaults: {
        state: "missing",
        mirror: null,
        updated_at: ago(5 * min),
        missing: [
          { publisher: "thulasi-ram", name: "clone", error: err("clone.yaml") },
          { publisher: "thulasi-ram", name: "build-image", error: err("build-image.yaml") },
        ],
      },
    };
  }
  return fullLibraries;
}

const yankImpact: S["YankImpactDto"] = {
  content_hash: CLONE,
  running_steps: 1,
  runs: [
    { id: "0190f8a20000418", project: "payments-api", run_number: 418, status: "running", steps: ["checkout/git"], running_steps: [] },
    { id: YANK_RUN_ID, project: "orders-api", run_number: 96, status: "running", steps: ["checkout/git", "charts/git"], running_steps: ["checkout/git"] },
    { id: "0190f8a20000057", project: "edge-gateway", run_number: 57, status: "suspended", steps: ["checkout/git"], running_steps: [] },
  ],
};

const OLD = `kind: library
interface:
  inputs:
    - { name: release, type: string, validate: 'value.matches("^[a-z0-9-]+$")' }
    - { name: chart,   type: string, required: false, default: ./chart }
    - { name: image,   type: string }
steps:
  - id: render
    image: alpine/helm:3.15.4@sha256:4f1c…a07e
    command: [helm, template, "\${{ inputs.release }}", "\${{ inputs.chart }}"]
    env:
      - [IMAGE, "\${{ inputs.image }}"]
  - id: helm
    needs: [render]
    image: alpine/helm:3.15.4@sha256:4f1c…a07e
    command: [helm, upgrade, --install, --wait, "\${{ inputs.release }}", "\${{ inputs.chart }}"]
    env:
      - [IMAGE, "\${{ inputs.image }}"]
  - id: smoke
    needs: [helm]
    invoke: lib:acme/http-probe
    with: { url: "https://\${{ inputs.release }}.internal.acme.dev/healthz" }`;
const NEW = `kind: library
interface:
  inputs:
    - { name: release, type: string, validate: 'value.matches("^[a-z0-9-]+$")' }
    - { name: chart,   type: string, required: false, default: ./chart }
    - { name: image,   type: string }
    - { name: timeout, type: number, required: false, default: 300 }
steps:
  - id: render
    image: alpine/helm:3.16.2@sha256:b83e…19c4
    command: [helm, template, "\${{ inputs.release }}", "\${{ inputs.chart }}"]
    env:
      - [IMAGE, "\${{ inputs.image }}"]
  - id: helm
    needs: [render]
    image: alpine/helm:3.16.2@sha256:b83e…19c4
    command: [helm, upgrade, --install, --atomic, --wait,
              --timeout, "\${{ inputs.timeout }}s",
              "\${{ inputs.release }}", "\${{ inputs.chart }}"]
    env:
      - [IMAGE, "\${{ inputs.image }}"]
      - [HELM_MAX_HISTORY, "20"]
  - id: smoke
    needs: [helm]
    invoke: lib:acme/http-probe
    with: { url: "https://\${{ inputs.release }}.internal.acme.dev/healthz" }`;

/** The server's line diff (LCS), as `line_diff` writes it. */
function lineDiff(a: string, b: string): string {
  const x = a.split("\n");
  const y = b.split("\n");
  const L = Array.from({ length: x.length + 1 }, () => new Array<number>(y.length + 1).fill(0));
  for (let i = x.length - 1; i >= 0; i--)
    for (let j = y.length - 1; j >= 0; j--) L[i]![j] = x[i] === y[j] ? L[i + 1]![j + 1]! + 1 : Math.max(L[i + 1]![j]!, L[i]![j + 1]!);
  const out: string[] = [];
  let i = 0;
  let j = 0;
  while (i < x.length && j < y.length) {
    if (x[i] === y[j]) out.push(` ${x[i++]}`), j++;
    else if (L[i + 1]![j]! >= L[i]![j + 1]!) out.push(`-${x[i++]}`);
    else out.push(`+${y[j++]}`);
  }
  while (i < x.length) out.push(`-${x[i++]}`);
  while (j < y.length) out.push(`+${y[j++]}`);
  return out.join("\n") + "\n";
}

const fetchedUpgrade: S["FetchedLibraryDto"] = {
  publisher: "acme",
  name: "deploy-helm",
  content_hash: HELM_NEW,
  source: HELM_NEW_SRC,
  content: NEW,
  first_party: false,
  current: { content_hash: HELM, source: HELM_SRC },
  diff: lineDiff(OLD, NEW),
};

// ── Runs ──────────────────────────────────────────────────────────────────
const lib = (name: string, hash: string, source: string, yanked = false): S["OriginLinkDto"] => ({ kind: "library", name, content_hash: hash, source, yanked });
const attempt = (startedMin: number, outcome = "succeeded", failure?: { failure: string; detail: string }): S["AttemptDto"] => ({
  id: "a1",
  started_at: ago(startedMin * min),
  failed: !!failure,
  outcome,
  ...(failure ? { failure: failure.failure, failure_detail: failure.detail } : {}),
});
const step = (o: Partial<StepStatusDto> & Pick<StepStatusDto, "id" | "status">): StepStatusDto => ({
  attempts: o.attempt_list?.length ?? 0,
  attempt_list: [],
  needs: [],
  services: [],
  uses: [],
  ...o,
});
const runBase = (id: string, n: number, status: string, pipeline: string, title: string, steps: StepStatusDto[]): RunStatusDto => ({
  id,
  status,
  run_number: n,
  pipeline,
  trigger_title: title,
  origin_pr_base: null,
  params: {},
  snapshot_retention: { retention_days: 14, expired: false, pinned: false },
  steps,
});

const cloneO = (y = false) => [lib("thulasi-ram/clone", y ? CLONE : CLONE_NEW, CLONE_SRC, y)];
const helmO = [lib("acme/deploy-helm", HELM, HELM_SRC)];

const libRun = runBase(LIB_RUN_ID, 421, "running", "deploy", "Bump chart to 1.8.2", [
  step({ id: "checkout/git", status: "succeeded", origin: cloneO(), attempt_list: [attempt(6)] }),
  step({ id: "test", status: "succeeded", needs: ["checkout/git"], attempt_list: [attempt(5)] }),
  step({ id: "build", status: "succeeded", needs: ["checkout/git"], attempt_list: [attempt(5)] }),
  step({ id: "ship/render", status: "succeeded", needs: ["test", "build"], origin: helmO, attempt_list: [attempt(2)] }),
  step({ id: "ship/helm", status: "running", needs: ["ship/render"], origin: helmO, attempt_list: [attempt(1, "running")] }),
  step({
    id: "ship/smoke/probe",
    status: "pending",
    needs: ["ship/helm"],
    origin: [...helmO, lib("acme/http-probe", PROBE, "github.com/acme/scarab-libs//http-probe.yaml@8c2f4e1a9d7b3c5e0f6a2b8d4c1e7f9a3b5d2c6e")],
  }),
]);

const yankDetail =
  "step `charts/git` uses `lib:thulasi-ram/clone` version a3c9e1f7b5d2, which an org admin yanked: " + YANK_REASON;
const yankRun: RunStatusDto = {
  ...runBase(YANK_RUN_ID, 96, "failed", "deploy", "Pin the payments chart", [
    step({ id: "checkout/git", status: "succeeded", origin: cloneO(true), attempt_list: [attempt(12)] }),
    step({ id: "test", status: "succeeded", needs: ["checkout/git"], attempt_list: [attempt(11)] }),
    step({ id: "charts/git", status: "failed", needs: ["test"], origin: cloneO(true), attempt_list: [attempt(10, "failed", { failure: "config", detail: yankDetail })] }),
    step({ id: "ship/helm", status: "pending", needs: ["charts/git"], origin: helmO }),
  ]),
  yanked_libraries: [{ name: "thulasi-ram/clone", content_hash: CLONE, reason: YANK_REASON, yanked_by: "a.kim", yanked_at: ago(11 * min), steps: ["checkout/git", "charts/git"] }],
};

function pluginRun(): RunStatusDto {
  const v = page().get("gate") ?? "wait";
  const reached = ago(6 * min - 19_000);
  const gateStep = step({
    id: "live",
    gate: "plugin",
    needs: ["bump"],
    status: v === "pass" ? "succeeded" : v === "exp" ? "failed" : "pending",
    origin: [{ kind: "plugin", name: "argocd-prod/wait-live", content_hash: h("e40b7c19d2a6") }],
    attempt_list: [{ id: "a1", started_at: reached, failed: v === "exp", outcome: v === "pass" ? "succeeded" : v === "exp" ? "failed" : "running", ...(v === "exp" ? { failure: "gate", failure_detail: "expired after 1800s with no release from plugin:argocd-prod" } : {}) }],
    plugin_gate: {
      releaser: "plugin:argocd-prod",
      installation: "argocd-prod",
      gate: "wait-live",
      plugin_state: v === "kill" ? "killed" : "enabled",
      ...(v === "kill" ? { killed_by: "a.kim", killed_at: ago(3 * min) } : {}),
      expires_after: 1800,
      waiting_since: v === "exp" ? ago(31 * min) : reached,
      expires_at: (v === "exp" ? ago(31 * min) : reached) + 1800 * 1000,
      inputs: { app: "payments-api", image: "ghcr.io/acme/payments-api:1.8.2@sha256:9f2c41ab" } as never,
      outputs: [{ name: "revision", type: "string", ...(v === "pass" ? { value: "4c1d9e2a7b30f68e5d1c" as never } : {}) }],
    },
    ...(v === "pass"
      ? { gate_resolution: { passed: true, reason: "released by plugin:argocd-prod", at: ago(2 * min), attempt: "a1", by: "plugin:argocd-prod" } }
      : v === "exp"
        ? { gate_resolution: { passed: false, reason: "expired after 1800s with no release from plugin:argocd-prod", at: ago(min), attempt: "a1" } }
        : {}),
  });
  return runBase(PLUGIN_RUN_ID, 422, v === "exp" ? "failed" : "running", "deploy-prod", "Deploy 1.8.2 to production", [
    step({ id: "checkout/git", status: "succeeded", origin: cloneO(), attempt_list: [attempt(9)] }),
    step({ id: "build", status: "succeeded", needs: ["checkout/git"], attempt_list: [attempt(8)] }),
    step({ id: "bump", status: "succeeded", deploy: true, needs: ["build"], attempt_list: [attempt(7)] }),
    gateStep,
    step({ id: "sign-off", gate: "manual", status: "pending", needs: ["bump"], attempt_list: [{ id: "a1", started_at: reached, failed: false, outcome: "running" }] }),
    step({ id: "announce", status: "pending", needs: ["live", "sign-off"] }),
  ]);
}

function stepOrigin(run: string, stepId: string): S["StepOriginDto"] | null {
  const r = run === LIB_RUN_ID ? libRun : run === YANK_RUN_ID ? yankRun : run === PLUGIN_RUN_ID ? pluginRun() : null;
  const s = r?.steps.find((x) => x.id === stepId);
  if (!r || !s) return null;
  const helmInputs: S["BoundInputDto"][] = [
    { name: "release", kind: "literal", bound: "payments-api" as never },
    { name: "chart", kind: "literal", bound: "./chart" as never },
    { name: "image", kind: "template", bound: "${{ outputs.build.image }}" as never },
  ];
  return {
    step: s.id,
    pipeline: r.pipeline,
    chain: s.origin ?? [],
    inputs: s.id.startsWith("ship/")
      ? helmInputs
      : s.id === "live"
        ? [
            { name: "app", kind: "literal", bound: "payments-api" as never },
            { name: "image", kind: "template", bound: "${{ outputs.bump.image }}" as never },
          ]
        : (s.origin ?? []).length
          ? [{ name: "ref", kind: "template", bound: "${{ event.sha }}" as never }]
          : [],
    ...(s.status === "pending" || s.plugin_gate
      ? { render_note: "the Step renders when it launches" }
      : {
          rendered: {
            image: "alpine/helm:3.15.4@sha256:4f1ca07e",
            command: ["helm", "upgrade", "--install", "--wait", "payments-api", "./chart"],
            env: [["IMAGE", "ghcr.io/acme/payments-api@sha256:9f2c41ab"]],
          },
        }),
  };
}

const sse = (run: string) => (at: number, kind: unknown) => `data: ${JSON.stringify({ version: 1, run, at, kind })}\n\n`;
function eventsOf(r: RunStatusDto): string {
  const e = sse(r.id);
  const out = [e(ago(15 * min), { Raw: { trigger: { event: { kind: "push", actor: "r.okafor", branch: "main", ref: "refs/heads/main", sha: "3f9a2c1e8d7b6a5f4e3d2c1b0a9f8e7d6c5b4a3f", repo: { owner: "acme", name: "orders-api" } } } } })];
  for (const s of r.steps) {
    for (const a of s.attempt_list ?? []) {
      out.push(e(a.started_at, { AttemptStarted: { step: s.id, attempt: a.id } }));
      if (a.outcome !== "running") {
        out.push(e(a.started_at + 40_000, { AttemptFinished: { step: s.id, attempt: a.id, ...(a.failure ? { failure: a.failure } : {}) } }));
      }
    }
  }
  return out.join("");
}

// ── Runs list: refused triggers ──────────────────────────────────────────
const refused: S["RefusedRunDto"][] = [
  {
    id: 2,
    pipeline: ".scarab/release.yaml",
    trigger_kind: "push",
    actor: "j.lee",
    git_ref: "refs/heads/main",
    sha: "3f9a2c1e8d7b6a5f4e3d2c1b0a9f8e7d6c5b4a3f",
    title: "Tag releases from the dispatch form",
    reason_kind: "compile",
    message: "pipeline: pipeline validation failed: …",
    created_at: ago(2 * min),
    errors: [] as S["CompileErrorDto"][],
  },
  {
    id: 1,
    pipeline: ".scarab/deploy.yaml",
    trigger_kind: "push",
    actor: "r.okafor",
    git_ref: "refs/heads/main",
    sha: "8d20f4a1c3e5b7d9f1a3c5e7b9d1f3a5c7e9b1d3",
    title: "Bump chart to 1.8.2",
    reason_kind: "yanked",
    message: "this Run uses a yanked library: " + yankDetail,
    created_at: ago(40 * min),
    errors: [{ kind: "other", message: "`checkout/git` uses `lib:thulasi-ram/clone` a3c9e1f7b5d2, which is yanked: " + YANK_REASON }],
  },
];

const compileErrors: S["CompileErrorDto"][] = [
  {
    kind: "command_value",
    step: "tag",
    command_index: 3,
    read: "${{ inputs.version }}",
    message:
      "step `tag`: `command[3]` reads `${{ inputs.version }}`, a value from outside the Pipeline that is not of a fixed shape (a `choice`, boolean or number input, or `event.sha`, `event.number`, `event.issue`, `event.fork` or `event.kind`); Scarab puts no such value into a command. Pass it in `env:` (`env: [[VALUE, \"${{ inputs.version }}\"]]`) and read it from the environment (`\"$VALUE\"` in a shell script)",
  },
  {
    kind: "command_value",
    step: "ship",
    library: "acme/deploy-helm",
    library_step: "helm",
    command_index: 4,
    read: "${{ inputs.release }}",
    input: "release",
    bound: "${{ inputs.service }}",
    message:
      "library `acme/deploy-helm`, step `helm`: `command[4]` reads `${{ inputs.release }}`, which step `ship` binds with `with.release: ${{ inputs.service }}`, a value from outside the Pipeline that is not of a fixed shape (a `choice`, boolean or number input, or `event.sha`, `event.number`, `event.issue`, `event.fork` or `event.kind`); Scarab puts no such value into a command. Pass it in `env:` (`env: [[VALUE, \"${{ inputs.release }}\"]]`) and read it from the environment (`\"$VALUE\"` in a shell script)",
  },
];
refused[0]!.errors = compileErrors;

/** The library and ADR-0076 routes, or null to fall through. */
export function libraryRoute(
  p: string,
  method: string,
  json: (body: unknown, init?: ResponseInit) => Response,
): Response | null {
  let m: RegExpMatchArray | null;
  if (/^\/v1\/orgs\/[^/]+\/libraries$/.test(p)) return json(orgLibraries());
  if (/^\/v1\/orgs\/[^/]+\/libraries\/fetch$/.test(p)) return json(fetchedUpgrade);
  if (/^\/v1\/orgs\/[^/]+\/library-yanks\/[^/]+\/impact$/.test(p)) return json(yankImpact);
  if (/^\/v1\/orgs\/[^/]+\/library-yanks$/.test(p) && method === "POST") {
    return json({ content_hash: CLONE, reason: YANK_REASON, yanked_by: "a.kim", yanked_at: now });
  }
  for (const r of [libRun, yankRun]) {
    if (p === `/v1/runs/${r.id}`) return json(r);
    if (p === `/v1/runs/${r.id}/events`) return new Response(eventsOf(r), { headers: { "content-type": "text/event-stream" } });
  }
  if (p === `/v1/runs/${PLUGIN_RUN_ID}`) return json(pluginRun());
  if (p === `/v1/runs/${PLUGIN_RUN_ID}/events`) {
    return new Response(eventsOf(pluginRun()), { headers: { "content-type": "text/event-stream" } });
  }
  if ((m = p.match(/^\/v1\/runs\/([^/]+)\/(artifacts|services)$/)) && [LIB_RUN_ID, YANK_RUN_ID, PLUGIN_RUN_ID].includes(m[1]!)) {
    return json([]);
  }
  if ((m = p.match(/^\/v1\/runs\/([^/]+)\/steps\/([^/]+)\/origin$/))) {
    const o = stepOrigin(m[1]!, decodeURIComponent(m[2]!));
    return o ? json(o) : null;
  }
  if (/^\/v1\/repos\/[^/]+\/orders-api\/refused-runs$/.test(p)) return json({ refused });
  if (/^\/v1\/repos\/[^/]+\/[^/]+\/refused-runs$/.test(p)) return json({ refused: [] });
  if (/^\/v1\/repos\/[^/]+\/orders-api\/pipelines$/.test(p)) {
    return json({
      sha: "3f9a2c1e8d7b6a5f4e3d2c1b0a9f8e7d6c5b4a3f",
      pipelines: [
        { name: "deploy", manual: true, api: false },
        { name: "release", manual: true, api: false },
      ],
    });
  }
  if (/^\/v1\/repos\/[^/]+\/orders-api\/pipelines\/release\/interface$/.test(p)) {
    return json(
      {
        code: "compile_failed",
        message: "pipeline: pipeline validation failed:\n  - " + compileErrors.map((e) => e.message).join("\n  - "),
        sha: "3f9a2c1e8d7b6a5f4e3d2c1b0a9f8e7d6c5b4a3f",
        errors: compileErrors,
      },
      { status: 400 },
    );
  }
  return null;
}

/** Summaries of the fixture Runs, for the orders-api Runs list. */
export const libraryRunSummaries = [
  { id: LIB_RUN_ID, status: "running", run_number: 421, created_at: ago(6 * min), duration_ms: 0, deploy: false, pipeline: "deploy", trigger_kind: "push", trigger_title: "Bump chart to 1.8.2", actor: "r.okafor", sha: "3f9a2c1e8d7b", git_ref: "refs/heads/main" },
  { id: YANK_RUN_ID, status: "failed", run_number: 96, created_at: ago(15 * min), duration_ms: 5 * min, deploy: false, pipeline: "deploy", trigger_kind: "push", trigger_title: "Pin the payments chart", actor: "j.lee", sha: "8d20f4a1c3e5", git_ref: "refs/heads/main" },
];
