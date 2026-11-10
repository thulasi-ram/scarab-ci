// Policy fixtures for mock mode (`just ui mock`): the org's CEL Policies (one
// in audit mode, two enforced, one deleted) and a library list, their
// version histories, a decision log, the checkpoint schemas, and a Run whose
// deploy Step a Policy refused at launch:
//
//   /orgs/acme/settings/policies                the list, with 7-day counts
//   /orgs/acme/settings/policies/pol-1          the editor (pol-1?tab=history: the history)
//   /orgs/acme/settings/policies/new            a new policy
//   /acme/orders-api/runs/<POLICY_RUN_ID>       the Run page's Policies strip
//
// The checkpoint schemas are the server's own `GET /v1/policy-checkpoints`
// answer, captured once. The mock does not evaluate CEL: its checker finds
// unknown fields and unguarded optional ones by pattern, and "Try it" marks
// the newest decisions at the draft's checkpoint as changed. Writes are kept
// for the page load.
import checkpoints from "./mock-policy-checkpoints.json";

const now = Date.now();
const min = 60_000;
const day = 86_400_000;
const ago = (ms: number) => now - ms;

export const POLICY_RUN_ID = "0190f8a2000071fb8c00b01c7e0a5e11";

type Body = Record<string, unknown> & {
  name: string;
  checkpoint: string;
  mode?: string;
  match?: Record<string, string[]>;
  rules?: { when: string; effect: unknown; message: string }[];
  setting?: Record<string, Record<string, string[]>>;
};

type MockPolicy = {
  id: string;
  level: string;
  org: string;
  project?: string;
  name: string;
  version: number;
  policy: Body;
  content_hash: string;
  created_at: number;
  updated_at: number;
  deleted: boolean;
};

type MockVersion = {
  version: number;
  policy: Body;
  content_hash: string;
  author: string;
  reason: string;
  step_up_ref: string | null;
  deleted: boolean;
  at: number;
};

const EMPTY_MATCH: Record<string, string[]> = { projects: [], except_projects: [], environments: [], trigger_kinds: [] };

const cel = (
  name: string,
  checkpoint: string,
  mode: string,
  when: string,
  message: string,
  match: Record<string, string[]> = {},
): Body => ({
  name,
  checkpoint,
  match: { ...EMPTY_MATCH, ...match },
  mode,
  rules: [{ when, effect: "deny", message }],
});

const FORK_DEPLOYS = "trigger.fork && pipeline.deploy";
const FORK_MESSAGE = "A pull request from a fork may not deploy to production.";
const LATEST = "step.image.endsWith(':latest')";
const LATEST_MESSAGE = "Pin the image by tag or digest, not :latest.";

/** Each Policy's versions, oldest first; the Policy is its last. */
const history: Record<string, MockVersion[]> = {
  "pol-1": [
    version(1, cel("no-fork-deploys", "run.create", "audit", FORK_DEPLOYS, FORK_MESSAGE), 30, "first draft"),
    version(
      2,
      cel("no-fork-deploys", "run.create", "audit", FORK_DEPLOYS, FORK_MESSAGE, { environments: ["production"] }),
      12,
      "production only",
    ),
    version(
      3,
      cel("no-fork-deploys", "run.create", "enforce", FORK_DEPLOYS, FORK_MESSAGE, { environments: ["production"] }),
      2,
      "no would-denies in a week of audit",
    ),
  ],
  "pol-2": [
    version(
      1,
      cel(
        "release-from-main",
        "run.create",
        "audit",
        "pipeline.deploy && has(trigger.ref) && trigger.ref != 'refs/heads/main'",
        "Deploy from main.",
        { projects: ["orders-*", "inventory-*"], except_projects: ["inventory-legacy"] },
      ),
      5,
      "see who deploys from branches",
    ),
  ],
  "pol-3": [
    version(
      1,
      {
        name: "approved-libraries",
        checkpoint: "run.create",
        match: EMPTY_MATCH,
        mode: "enforce",
        setting: { libraries_allowed: { sources: ["github.com/thulasi-ram/*", "github.com/acme/*"] } },
      },
      20,
      "",
    ),
  ],
  "pol-4": [
    version(1, cel("old-rule", "run.create", "enforce", "false", "never"), 40, ""),
    { ...version(2, cel("old-rule", "run.create", "enforce", "false", "never"), 35, "unused"), deleted: true, step_up_ref: "conf-1" },
  ],
  "pol-5": [
    version(1, cel("no-latest-images", "step.admit", "audit", "step.image.endsWith('latest')", LATEST_MESSAGE), 9, ""),
    { ...version(2, cel("no-latest-images", "step.admit", "enforce", LATEST, LATEST_MESSAGE), 3, "tag suffix only"), step_up_ref: null },
  ],
};

function version(v: number, policy: Body, daysAgo: number, reason: string): MockVersion {
  return {
    version: v,
    policy,
    content_hash: `9f2c${v}`.padEnd(64, "0"),
    author: "a.kim",
    reason,
    step_up_ref: null,
    deleted: false,
    at: ago(daysAgo * day),
  };
}

const policies: MockPolicy[] = Object.entries(history).map(([id, vs]) => {
  const last = vs[vs.length - 1]!;
  return {
    id,
    level: "org",
    org: "acme",
    name: last.policy.name,
    version: last.version,
    policy: last.policy,
    content_hash: last.content_hash,
    created_at: vs[0]!.at,
    updated_at: last.at,
    deleted: last.deleted,
  };
});

// ── Decisions ────────────────────────────────────────────────────────────────

type Outcome = "not_matched" | "passed" | "denied" | "would_deny";

const result = (policyId: string, kind: Outcome, message = "") => {
  const p = policies.find((x) => x.id === policyId)!;
  return {
    policy_id: policyId,
    version: p.version,
    name: p.name,
    level: "org",
    mode: p.policy.mode ?? "enforce",
    outcome: kind === "denied" || kind === "would_deny" ? { kind, messages: [message] } : { kind },
  };
};

const runCreateInput = (project: string, ref: string, fork: boolean) => ({
  org: { name: "acme" },
  project: { name: project },
  trigger: { kind: fork ? "pull_request" : "push", ref, sha: "4e1d2c3b", fork, actor: "a.kim", principal: "a.kim" },
  pipeline: { name: "deploy", environment: "production", deploy: true },
  steps: [
    { id: "build", image: "ghcr.io/acme/builder:1.4", deploy: false, secrets: [], origin: [], upstream_gates: [] },
    { id: "deploy", image: "ghcr.io/acme/deployer:latest", deploy: true, secrets: ["KUBECONFIG"], origin: [], upstream_gates: [] },
  ],
  gates: [],
  time: new Date(ago(40 * min)).toISOString(),
});

const stepAdmitInput = (step: string, image: string, at: number) => ({
  org: { name: "acme" },
  project: { name: "orders-api" },
  step: { id: step, image, deploy: step === "deploy", secrets: step === "deploy" ? ["KUBECONFIG"] : [], uses_environment_authority: step === "deploy", origin: [] },
  attempt: 1,
  run: { id: POLICY_RUN_ID, ref: "refs/heads/release-2", environment: "production" },
  environment: { has_approvers: false, ref_allowed: true, max_approval_age_s: 86400 },
  approvals: [],
  time: new Date(at).toISOString(),
});

type Result = ReturnType<typeof result>;

const decision = (
  id: string,
  at: number,
  checkpoint: string,
  project: string,
  runId: string | null,
  results: Result[],
  input: unknown,
  step?: string,
) => {
  const messages = results.flatMap((r) =>
    r.outcome.kind === "denied" ? ((r.outcome as { messages?: string[] }).messages ?? []) : [],
  );
  return {
    id,
    org: "acme",
    project,
    run_id: runId,
    step_id: step ?? null,
    attempt: step ? 1 : null,
    at,
    decision: {
      checkpoint,
      schema_version: 1,
      input,
      policy_set_digest: "ab12".padEnd(64, "0"),
      results,
      outcome: messages.length > 0 ? { kind: "deny", messages } : { kind: "allow" },
    },
  };
};

/** The org log, newest first. */
const decisions = [
  decision("dec-7", ago(35 * min), "step.admit", "orders-api", POLICY_RUN_ID,
    [result("pol-5", "denied", LATEST_MESSAGE)], stepAdmitInput("deploy", "ghcr.io/acme/deployer:latest", ago(35 * min)), "deploy"),
  decision("dec-6", ago(39 * min), "step.admit", "orders-api", POLICY_RUN_ID,
    [result("pol-5", "passed")], stepAdmitInput("build", "ghcr.io/acme/builder:1.4", ago(39 * min)), "build"),
  decision("dec-5", ago(40 * min), "run.create", "orders-api", POLICY_RUN_ID,
    [result("pol-1", "passed"), result("pol-2", "would_deny", "Deploy from main."), result("pol-3", "passed")],
    runCreateInput("orders-api", "refs/heads/release-2", false)),
  decision("dec-4", ago(2 * 60 * min), "run.create", "scarab", null,
    [result("pol-1", "denied", FORK_MESSAGE), result("pol-2", "not_matched"), result("pol-3", "passed")],
    runCreateInput("scarab", "refs/pull/131/head", true)),
  decision("dec-3", ago(26 * 60 * min), "run.create", "inventory-svc", "0190f8a2000071fb8c00000000000d03",
    [result("pol-1", "passed"), result("pol-2", "would_deny", "Deploy from main."), result("pol-3", "passed")],
    runCreateInput("inventory-svc", "refs/heads/hotfix", false)),
  decision("dec-2", ago(50 * 60 * min), "run.create", "scarab", "0190f8a2000071fb8c00000000000d02",
    [result("pol-1", "passed"), result("pol-2", "not_matched"), result("pol-3", "passed")],
    runCreateInput("scarab", "refs/heads/main", false)),
  decision("dec-1", ago(9 * day), "run.create", "orders-api", "0190f8a2000071fb8c00000000000d01",
    [result("pol-1", "passed"), result("pol-2", "would_deny", "Deploy from main."), result("pol-3", "passed")],
    null),
];

// ── The Run a Policy refused a Step of ───────────────────────────────────────

const policyRunStatus = {
  id: POLICY_RUN_ID,
  status: "failed",
  run_number: 88,
  pipeline: "deploy",
  trigger_title: "Deploy release-2 to production",
  origin_pr_base: null,
  params: {},
  snapshot_retention: { retention_days: 14, expired: false, pinned: false },
  steps: [
    {
      id: "build",
      status: "succeeded",
      attempts: 1,
      needs: [],
      attempt_list: [{ id: "a1", started_at: ago(39 * min), failed: false, outcome: "succeeded" }],
    },
    {
      id: "deploy",
      status: "failed",
      attempts: 1,
      deploy: true,
      needs: ["build"],
      attempt_list: [
        {
          id: "a1",
          started_at: ago(35 * min),
          failed: true,
          failure: "step",
          failure_detail: `policy \`no-latest-images\`: ${LATEST_MESSAGE}`,
          outcome: "failed",
        },
      ],
    },
  ],
};

const sse = (at: number, kind: unknown) =>
  `data: ${JSON.stringify({ version: 1, run: POLICY_RUN_ID, at, kind })}\n\n`;
const policyRunEvents = [
  sse(ago(40 * min), {
    Raw: {
      trigger: {
        event: {
          kind: "push",
          actor: "a.kim",
          branch: "release-2",
          ref: "refs/heads/release-2",
          sha: "4e1d2c3b5a69788796a5b4c3d2e1f0a9",
          repo: { owner: "acme", name: "orders-api" },
        },
      },
    },
  }),
  sse(ago(39 * min), { AttemptStarted: { step: "build", attempt: "a1" } }),
  sse(ago(36 * min), { AttemptFinished: { step: "build", attempt: "a1" } }),
  sse(ago(36 * min), { StepTransitioned: { step: "build", from: "running", to: "succeeded" } }),
  sse(ago(35 * min), { AttemptStarted: { step: "deploy", attempt: "a1" } }),
  sse(ago(35 * min), { AttemptFinished: { step: "deploy", attempt: "a1", failure: "step" } }),
  sse(ago(35 * min), { StepTransitioned: { step: "deploy", from: "running", to: "failed" } }),
].join("");

// ── The mock checker ─────────────────────────────────────────────────────────

type Schema = {
  variables: { name: string; ty: { kind: string; of?: unknown }; optional: boolean }[];
  structs: Record<string, { fields: { name: string; ty: { kind: string; of?: unknown }; optional: boolean }[] }>;
};
const schemaOf = (c: string) => checkpoints.find((x) => x.checkpoint === c)?.schema as unknown as Schema | undefined;

type Diag = { severity: string; at: string; line: number; column: number; expr: string; message: string };
const diag = (severity: string, at: string, message: string, line = 0, column = 0, expr = ""): Diag => ({
  severity,
  at,
  line,
  column,
  expr,
  message,
});

/** The 1-based line and column of `index` in `text`. */
function position(text: string, index: number): [number, number] {
  const before = text.slice(0, index).split("\n");
  return [before.length, before[before.length - 1]!.length + 1];
}

function check(body: Body | null): unknown {
  if (!body || typeof body.name !== "string" || typeof body.checkpoint !== "string") {
    return {
      ok: false,
      policy: null,
      diagnostics: [diag("error", "", "invalid Policy: missing field `name` or `checkpoint`")],
      rules: [],
    };
  }
  const out: Diag[] = [];
  const reads: { index: number; predicate: string; reads: string[] }[] = [];
  if (!/^[A-Za-z0-9._-]{1,100}$/.test(body.name)) {
    out.push(diag("error", "name", `\`${body.name}\`: a Policy name is 1 to 100 letters, digits, \`-\`, \`_\` or \`.\``));
  }
  const schema = schemaOf(body.checkpoint);
  if (!schema) out.push(diag("error", "checkpoint", `checkpoint \`${body.checkpoint}\` is not yet supported`));
  if (body.checkpoint !== "run.create" && (body.match?.trigger_kinds ?? []).length > 0) {
    out.push(diag("error", "match.trigger_kinds", `\`trigger_kinds\` is matched at \`run.create\` only, not at \`${body.checkpoint}\``));
  }
  const rules = body.rules ?? [];
  if (!body.setting && rules.length === 0) out.push(diag("error", "rules", "a Policy needs at least one rule"));
  const warnings: Diag[] = [];
  rules.forEach((r, i) => {
    const at = `rules[${i}]`;
    if (!r.message?.trim()) out.push(diag("error", `${at}.message`, "a message cannot be empty"));
    if (!r.when?.trim()) {
      out.push(diag("error", `${at}.when`, "does not parse: the condition is empty", 1, 1));
      return;
    }
    if (!schema) return;
    let failed = false;
    const seen = new Set<string>();
    for (const m of r.when.matchAll(/\b([a-z_]+)\.([A-Za-z_]+)/g)) {
      const v = schema.variables.find((x) => x.name === m[1]);
      if (!v) continue;
      seen.add(v.name);
      if (v.ty.kind !== "struct") continue;
      const field = schema.structs[v.ty.of as string]?.fields.find((f) => f.name === m[2]);
      const [line, column] = position(r.when, m.index!);
      if (!field) {
        out.push(diag("error", `${at}.when`, `\`${v.ty.of}\` has no field \`${m[2]}\``, line, column, m[0]));
        failed = true;
        continue;
      }
      seen.add(`${m[1]}.${m[2]}`);
      if (field.optional && !r.when.includes(`has(${m[0]})`)) {
        warnings.push(
          diag("warning", `${at}.when`, `\`${m[0]}\` may be absent; test it with \`has(${m[0]})\` first, or a Run without it is denied`, line, column, m[0]),
        );
      }
    }
    if (!failed) reads.push({ index: i, predicate: "cel", reads: [...seen].sort() });
  });
  if (body.setting) reads.push({ index: 0, predicate: "typed", reads: [] });
  const errors = out.filter((d) => d.severity === "error");
  return { ok: errors.length === 0, policy: body, diagnostics: [...errors, ...warnings], rules: reads };
}

/** A `kind: policy` YAML file of the shape the editor's Copy YAML writes:
 *  `key: value` lines, `match:` and `setting:` maps of flow lists, and
 *  `rules:` items with a `|-` block or a quoted condition. JSON is read
 *  too. Anything else is not a Policy here. */
function readYaml(text: string): Body | null {
  try {
    return JSON.parse(text) as Body;
  } catch {
    /* YAML below */
  }
  const value = (v: string): unknown => {
    const t = v.trim();
    try {
      return JSON.parse(t);
    } catch {
      return t.replace(/^'(.*)'$/, "$1");
    }
  };
  const out: Record<string, unknown> = {};
  const lines = text.split("\n");
  let section: "match" | "rules" | "setting" | null = null;
  let settingKind = "";
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i]!;
    if (line.trim() === "" || line.trim().startsWith("#")) continue;
    const top = /^([a-z_]+):\s*(.*)$/.exec(line);
    if (top) {
      section = null;
      if (top[2] === "") {
        section = top[1] as "match" | "rules" | "setting";
        if (section === "rules") out.rules = [];
        else out[top[1]!] = {};
      } else if (top[1] !== "kind") {
        out[top[1]!] = value(top[2]!);
      }
      continue;
    }
    if (section === "rules") {
      const item = /^\s*(-\s+)?([a-z_]+):\s*(.*)$/.exec(line);
      if (!item) return null;
      const rules = out.rules as Record<string, unknown>[];
      if (item[1]) rules.push({});
      const rule = rules[rules.length - 1];
      if (!rule) return null;
      if (item[3] === "|-" || item[3] === "|") {
        const block: string[] = [];
        const indent = (lines[i + 1] ?? "").length - (lines[i + 1] ?? "").trimStart().length;
        while (i + 1 < lines.length && (lines[i + 1]!.trim() === "" || lines[i + 1]!.length - lines[i + 1]!.trimStart().length >= indent)) {
          block.push(lines[++i]!.slice(indent));
        }
        rule[item[2]!] = block.join("\n").replace(/\n+$/, "");
      } else {
        rule[item[2]!] = value(item[3]!);
      }
      continue;
    }
    const nested = /^(\s+)([a-z_]+):\s*(.*)$/.exec(line);
    if (!nested) return null;
    if (section === "match") (out.match as Record<string, unknown>)[nested[2]!] = value(nested[3]!);
    else if (section === "setting") {
      if (nested[3] === "") {
        settingKind = nested[2]!;
        (out.setting as Record<string, unknown>)[settingKind] = {};
      } else {
        ((out.setting as Record<string, Record<string, unknown>>)[settingKind] ??= {})[nested[2]!] = value(nested[3]!);
      }
    } else return null;
  }
  return out as Body;
}

// ── Routes ───────────────────────────────────────────────────────────────────

const verdict = (r: { mode: string; outcome: { kind: string } } | undefined) =>
  !r ? "allow" : r.outcome.kind === "denied" ? "deny" : r.outcome.kind === "would_deny" ? "would_deny" : "allow";

function counts(since: number) {
  const by = new Map<string, { policy_id: string; matched: number; denied: number; would_deny: number }>();
  for (const d of decisions) {
    if (d.at < since) continue;
    for (const r of d.decision.results) {
      if (r.outcome.kind === "not_matched") continue;
      const c = by.get(r.policy_id) ?? { policy_id: r.policy_id, matched: 0, denied: 0, would_deny: 0 };
      c.matched += 1;
      if (r.outcome.kind === "denied") c.denied += 1;
      if (r.outcome.kind === "would_deny") c.would_deny += 1;
      by.set(r.policy_id, c);
    }
  }
  return [...by.values()].sort((a, b) => (a.policy_id < b.policy_id ? -1 : 1));
}

let nextId = 6;

function save(p: MockPolicy, body: Body, reason: string, deleted = false) {
  const enforced = p.policy.mode === "enforce";
  p.version += 1;
  p.policy = body;
  p.name = body.name;
  p.updated_at = Date.now();
  p.deleted = deleted;
  (history[p.id] ??= []).push({
    ...version(p.version, body, 0, reason),
    at: Date.now(),
    deleted,
    step_up_ref: enforced ? `conf-${p.version}` : null,
  });
}

/** The policy routes, or null to fall through. */
export function policyRoute(
  p: string,
  search: string,
  method: string,
  body: string,
  json: (body: unknown, init?: ResponseInit) => Response,
): Response | null {
  const q = new URLSearchParams(search);
  const read = (): Record<string, unknown> => {
    try {
      return JSON.parse(body) as Record<string, unknown>;
    } catch {
      return {};
    }
  };
  let m: RegExpMatchArray | null;

  if (p === "/v1/policy-checkpoints") return json(checkpoints);

  if (p === `/v1/runs/${POLICY_RUN_ID}`) return json(policyRunStatus);
  if (p === `/v1/runs/${POLICY_RUN_ID}/events`) {
    return new Response(policyRunEvents, { status: 200, headers: { "content-type": "text/event-stream" } });
  }
  if (p === `/v1/runs/${POLICY_RUN_ID}/artifacts` || p === `/v1/runs/${POLICY_RUN_ID}/services`) return json([]);
  if ((m = p.match(/^\/v1\/runs\/([^/]+)\/policy-decisions$/))) {
    const run = decodeURIComponent(m[1]!);
    return json(decisions.filter((d) => d.run_id === run).sort((a, b) => a.at - b.at));
  }

  if (/^\/v1\/orgs\/[^/]+\/policy-decisions\/counts$/.test(p)) {
    const since = Number(q.get("since") ?? 0);
    return json({ since, policies: counts(since) });
  }
  if (/^\/v1\/orgs\/[^/]+\/policy-decisions$/.test(p)) {
    const limit = Number(q.get("limit") ?? 200);
    const checkpoint = q.get("checkpoint");
    const policyId = q.get("policy_id");
    const since = q.get("since");
    const page = decisions
      .filter((d) => !checkpoint || d.decision.checkpoint === checkpoint)
      .filter((d) => !policyId || d.decision.results.some((r) => r.policy_id === policyId))
      .filter((d) => !since || d.at >= Number(since))
      .slice(0, limit);
    return json({ decisions: page, next: null });
  }

  if (/^\/v1\/orgs\/[^/]+\/policies\/check$/.test(p) && method === "POST") {
    const b = read();
    if (typeof b.yaml === "string") {
      const parsed = readYaml(b.yaml);
      return json(
        parsed
          ? check(parsed)
          : { ok: false, policy: null, diagnostics: [diag("error", "", "the YAML does not parse")], rules: [] },
      );
    }
    return json(check(b.policy as Body));
  }
  if (/^\/v1\/orgs\/[^/]+\/policy-test\/replay$/.test(p) && method === "POST") {
    const b = read();
    const draft = b.policy as Body;
    const id = b.policy_id as string | undefined;
    const after = draft.mode === "audit" ? "would_deny" : "deny";
    const at = decisions.filter((d) => d.decision.checkpoint === draft.checkpoint);
    const changed = at
      .filter((d) => d.decision.input !== null)
      .filter((d) => verdict(d.decision.results.find((r) => r.policy_id === id)) !== after)
      .slice(0, 2)
      .map((d) => ({
        decision_id: d.id,
        run_id: d.run_id,
        project: d.project,
        step_id: d.step_id,
        attempt: d.attempt,
        at: d.at,
        before: verdict(d.decision.results.find((r) => r.policy_id === id)),
        after,
        messages: (draft.rules ?? []).map((r) => r.message),
      }));
    return json({
      checkpoint: draft.checkpoint,
      read: at.length,
      evaluated: at.filter((d) => d.decision.input !== null).length,
      skipped: at.filter((d) => d.decision.input === null).length,
      changed,
    });
  }

  // Project typed settings: none until one is added on the page.
  if ((m = p.match(/^\/v1\/orgs\/([^/]+)\/projects\/([^/]+)\/policies(?:\/([^/]+))?$/))) {
    const project = decodeURIComponent(m[2]!);
    const id = m[3] && decodeURIComponent(m[3]);
    return crud(json, method, read, id, project);
  }
  if ((m = p.match(/^\/v1\/orgs\/[^/]+\/policies\/([^/]+)\/versions$/))) {
    const vs = history[decodeURIComponent(m[1]!)];
    return vs ? json(vs) : new Response("not found", { status: 404 });
  }
  if ((m = p.match(/^\/v1\/orgs\/[^/]+\/policies(?:\/([^/]+))?$/))) {
    return crud(json, method, read, m[1] && decodeURIComponent(m[1]), undefined, q);
  }
  return null;
}

function crud(
  json: (body: unknown, init?: ResponseInit) => Response,
  method: string,
  read: () => Record<string, unknown>,
  id: string | undefined,
  project: string | undefined,
  q?: URLSearchParams,
): Response {
  const here = (x: MockPolicy) => x.project === project;
  if (!id) {
    if (method === "POST") {
      const b = read();
      const policy = b.policy as Body;
      const checked = check(policy) as { ok: boolean; diagnostics: Diag[] };
      if (!checked.ok) {
        return new Response(
          `the Policy does not compile:\n${checked.diagnostics.map((d) => `- ${d.at}: ${d.message}`).join("\n")}`,
          { status: 400 },
        );
      }
      if (policies.some((x) => here(x) && !x.deleted && x.name === policy.name)) {
        return new Response(`a policy named \`${policy.name}\` already exists here`, { status: 409 });
      }
      const created: MockPolicy = {
        id: `pol-${nextId++}`,
        level: project ? "project" : "org",
        org: "acme",
        ...(project ? { project } : {}),
        name: policy.name,
        version: 1,
        policy,
        content_hash: "9f2c".padEnd(64, "0"),
        created_at: Date.now(),
        updated_at: Date.now(),
        deleted: false,
      };
      policies.push(created);
      history[created.id] = [{ ...version(1, policy, 0, String(b.reason ?? "")), at: Date.now() }];
      return json(created, { status: 201 });
    }
    return json(policies.filter((x) => here(x) && !x.deleted).sort((a, b) => (a.name < b.name ? -1 : 1)));
  }
  const p = policies.find((x) => x.id === id && here(x));
  if (!p) return new Response("not found", { status: 404 });
  if (method === "PUT") {
    const b = read();
    if (b.expected_version !== p.version) {
      return new Response(`the policy is at version ${p.version}, not ${String(b.expected_version)}`, { status: 409 });
    }
    save(p, b.policy as Body, String(b.reason ?? ""));
    return json(p);
  }
  if (method === "DELETE") {
    save(p, p.policy, q?.get("reason") ?? "", true);
    return json(p);
  }
  return json(p);
}
