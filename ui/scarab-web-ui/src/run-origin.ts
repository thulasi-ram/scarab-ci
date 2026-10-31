// Where a Run's Steps come from, and a plugin gate's wait (ADR-0076), as the
// Run page words them. Pure: the page and its tests agree.
import { idSegments, type OriginLink } from "./dag-layout";
import { shortHash } from "./libraries";

/** One part of the origin line under the step pane's stamp. */
export type OriginPart =
  | { kind: "file"; text: string }
  | { kind: "step"; text: string }
  | { kind: "lib"; text: string; hash: string; yanked: boolean };

/** The origin line: the Pipeline file, then each invoke Step and the library
 *  it invoked, then the Step's own id. Empty for a Step written in the
 *  Pipeline file. */
export function originParts(pipeline: string | null | undefined, s: { id: string; origin?: OriginLink[] | null }): OriginPart[] {
  const links = (s.origin ?? []).filter((o) => o.kind === "library" || o.kind === "file");
  const plugin = (s.origin ?? []).find((o) => o.kind === "plugin");
  if (links.length === 0 && !plugin) return [];
  const segs = idSegments(s.id);
  const out: OriginPart[] = [{ kind: "file", text: pipeline ? `.scarab/${pipeline}.yaml` : "the Pipeline file" }];
  segs.forEach((seg, i) => {
    out.push({ kind: "step", text: seg });
    const o = links[i];
    if (o && i < segs.length - 1) {
      out.push({ kind: "lib", text: `${o.kind === "library" ? "lib:" : "file:"}${o.name}`, hash: shortHash(o.content_hash), yanked: !!o.yanked });
    }
  });
  if (plugin) out.push({ kind: "lib", text: `plugin:${plugin.name}`, hash: shortHash(plugin.content_hash), yanked: false });
  return out;
}

/** Where a plugin gate's wait stands. */
export type PluginGateState = "waiting" | "killed" | "released" | "expired" | "rejected";

export type PluginGateLike = {
  status: string;
  plugin_gate?: { plugin_state?: string | null } | null;
  gate_resolution?: { passed: boolean; reason?: string | null; by?: string | null } | null;
};

export function pluginGateState(s: PluginGateLike): PluginGateState {
  const open = s.status === "pending" || s.status === "ready" || s.status === "waiting";
  if (open) return s.plugin_gate?.plugin_state === "killed" ? "killed" : "waiting";
  const r = s.gate_resolution;
  if (r?.passed || s.status === "succeeded") return "released";
  if (r && /expire/i.test(r.reason ?? "")) return "expired";
  return "rejected";
}

/** `m:ss`, or `h:mm:ss` from an hour, of a duration in ms (never negative). */
export function clock(ms: number): string {
  const t = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(t / 3600);
  const m = Math.floor((t % 3600) / 60);
  const s = String(t % 60).padStart(2, "0");
  return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${s}` : `${m}:${s}`;
}

/** How far through its expiry window a wait is, 0-100. */
export function expiryPercent(since: number, expiresAt: number, now: number): number {
  const span = expiresAt - since;
  if (span <= 0) return 100;
  return Math.max(0, Math.min(100, Math.round((100 * (now - since)) / span)));
}

/** Under five minutes left: the clock turns red. */
export const clockLow = (leftMs: number) => leftMs < 5 * 60 * 1000;

/** The Steps a Rerun from `step` launches again: it and everything
 *  downstream of it. */
export function rerunScope(step: string, steps: { id: string; needs: string[] }[]): Set<string> {
  const scope = new Set([step]);
  let grew = true;
  while (grew) {
    grew = false;
    for (const s of steps) {
      if (!scope.has(s.id) && s.needs.some((n) => scope.has(n))) {
        scope.add(s.id);
        grew = true;
      }
    }
  }
  return scope;
}

export type RunYank = { name: string; content_hash: string; reason: string; yanked_at: number; steps: string[] };

/** The yanked version a Rerun from `step` would launch a Step of, if any:
 *  that Rerun is refused at launch every time, since it reuses the Steps the
 *  Run was compiled with. */
export function rerunYank(
  step: string | null,
  steps: { id: string; needs: string[] }[],
  yanked: RunYank[],
): RunYank | null {
  if (!step || yanked.length === 0) return null;
  const scope = rerunScope(step, steps);
  return yanked.find((y) => y.steps.some((s) => scope.has(s))) ?? null;
}

/** The note the Run page shows when a version it uses is yanked. */
export function yankNote(
  yanked: RunYank[],
  steps: { id: string; status: string }[],
  time: (ms: number) => string,
): string | null {
  const y = yanked[0];
  if (!y) return null;
  const lib = `lib:${y.name} ${shortHash(y.content_hash)}`;
  const statusOf = (id: string) => steps.find((s) => s.id === id)?.status;
  const ran = y.steps.filter((s) => statusOf(s) === "succeeded");
  const refused = y.steps.filter((s) => statusOf(s) === "failed");
  const parts = [`${lib} was yanked at ${time(y.yanked_at)}: ${y.reason}.`];
  if (ran.length) parts.push(`${ran.join(", ")} ${ran.length === 1 ? "had" : "have"} run.`);
  if (refused.length) parts.push(`${refused.join(", ")} ${refused.length === 1 ? "was" : "were"} refused at launch.`);
  parts.push("A Rerun reuses the Steps this Run was compiled with, so a Step from this version is refused every time. Start a new run to compile against the installed version.");
  return parts.join(" ");
}

/** Is an attempt's failure a yank refusal (ADR-0076 §6)? */
export const isYankRefusal = (detail: string | null | undefined) => /yanked/i.test(detail ?? "");
