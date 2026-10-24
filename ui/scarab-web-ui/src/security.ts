// Security pages, as the UI has to reason about them (ADR-0073). Pure — no
// DOM, no fetch — so the shaping the Account → Security panel, Settings →
// Security, the confirm page and the app banner share is testable without
// rendering anything.

/** An Org's step-up posture, structurally (`OrgSecuritySummary`). */
export type SummaryLike = {
  enforced: boolean;
  admins_without_passkey: string[];
  admins_total: number;
};

/**
 * The app-wide banner: "step-up enforced; N admins have no passkey", or
 * `null` when there is nothing to act on.
 *
 * Hidden when step-up is off (nothing is refused, so nothing to warn about)
 * and when every admin holds a passkey — a banner that is always there when
 * all is well is a banner nobody reads on the day it matters. The admins it
 * counts are exactly the ones every crown jewel is refused for until they
 * enroll.
 */
export function bannerText(s: SummaryLike | null | undefined): string | null {
  if (!s || !s.enforced) return null;
  const n = s.admins_without_passkey.length;
  if (n === 0) return null;
  return `step-up enforced; ${n} ${n === 1 ? "admin has" : "admins have"} no passkey`;
}

/** The posture in one line for the Settings → Security header. */
export function postureText(s: SummaryLike): string {
  if (!s.enforced) return "Step-up is off on this server: crown-jewel changes need no passkey.";
  const missing = s.admins_without_passkey.length;
  const have = Math.max(0, s.admins_total - missing);
  return `Step-up is enforced. ${have} of ${s.admins_total} ${
    s.admins_total === 1 ? "admin holds" : "admins hold"
  } a passkey.`;
}

/** The event kinds the server writes, in the order the filter offers them. */
export const EVENT_KINDS: readonly string[] = [
  "step_up.confirmed",
  "step_up.refused",
  "op.authorised",
  "op.completed",
  "request.raised",
  "request.confirmed",
  "request.executed",
  "request.failed",
  "passkey.enrolled",
  "passkey.removed",
  "passkey.reset",
  "passkey.break_glass_reset",
  "passkey.clone_suspected",
  "guard.not_consulted",
];

const KIND_LABELS: Record<string, string> = {
  "step_up.confirmed": "Confirmed with passkey",
  "step_up.refused": "Refused: step-up required",
  "op.authorised": "Authorised",
  "op.completed": "Completed",
  "request.raised": "Request raised",
  "request.confirmed": "Request confirmed",
  "request.executed": "Request ran",
  "request.failed": "Request failed",
  "passkey.enrolled": "Passkey enrolled",
  "passkey.removed": "Passkey removed",
  "passkey.reset": "Passkeys reset",
  "passkey.break_glass_reset": "Passkeys reset (break-glass)",
  "passkey.clone_suspected": "Passkey clone suspected",
  "guard.not_consulted": "Guard not consulted",
};

/** A kind in words; an unknown (newer) kind is shown verbatim. */
export function eventKindLabel(kind: string): string {
  return KIND_LABELS[kind] ?? kind;
}

/** Kinds that should catch the eye in a table. */
export function eventTone(kind: string): "danger" | "warn" | "ok" | "plain" {
  if (kind === "passkey.clone_suspected" || kind === "guard.not_consulted") return "danger";
  if (
    kind === "step_up.refused" ||
    kind === "request.failed" ||
    kind === "passkey.reset" ||
    kind === "passkey.break_glass_reset" ||
    kind === "passkey.removed"
  )
    return "warn";
  if (kind === "step_up.confirmed" || kind === "op.completed" || kind === "request.executed")
    return "ok";
  return "plain";
}

/** How the caller reached the server, in words. */
export function callerLabel(caller: string): string {
  if (caller === "session:cookie") return "browser";
  if (caller === "session:bearer") return "CLI session";
  if (caller === "operator:break-glass") return "operator";
  if (caller.startsWith("api_token:")) return `API token ${caller.slice("api_token:".length)}`;
  return caller;
}

/** An event, structurally (`SecurityEventDto`). */
export type EventLike = {
  seq: number;
  at: number;
  kind: string;
  subject: string;
  caller: string;
  op?: string | null;
  target?: string | null;
  org?: string | null;
  request_id?: string | null;
  detail?: unknown;
};

/** One table row's cells. */
export type EventRow = {
  key: number;
  at: number;
  kind: string;
  label: string;
  tone: ReturnType<typeof eventTone>;
  who: string;
  via: string;
  what: string;
  note: string;
};

/** The "what" cell: the op and its target, whichever the event has. */
function whatOf(e: EventLike): string {
  const op = e.op ?? "";
  const target = e.target ?? "";
  if (op && target) return `${op} · ${target}`;
  return op || target || "—";
}

/** The "note" cell: the refusal mode and reason the server recorded, which is
 * the whole answer to "why was I asked". */
function noteOf(e: EventLike): string {
  const d = e.detail as Record<string, unknown> | null | undefined;
  if (!d || typeof d !== "object") return "";
  const parts: string[] = [];
  if (typeof d.mode === "string") parts.push(`mode ${d.mode}`);
  if (typeof d.reason === "string") parts.push(d.reason);
  if (typeof d.kind === "string") parts.push(d.kind);
  return parts.join(" · ");
}

export function eventRow(e: EventLike): EventRow {
  return {
    key: e.seq,
    at: e.at,
    kind: e.kind,
    label: eventKindLabel(e.kind),
    tone: eventTone(e.kind),
    who: e.subject,
    via: callerLabel(e.caller),
    what: whatOf(e),
    note: noteOf(e),
  };
}

/** A passkey, structurally (`PasskeyDto`). */
export type PasskeyLike = {
  label: string;
  created_at: number;
  last_used_at?: number | null;
  backup_eligible: boolean;
  backup_state: boolean;
  clone_suspected: boolean;
};

/** The badges a passkey row carries. A clone-suspected passkey is refused for
 * good — that is the one to say loudly. */
export function passkeyBadges(p: PasskeyLike): { text: string; tone: "danger" | "plain" }[] {
  const out: { text: string; tone: "danger" | "plain" }[] = [];
  if (p.clone_suspected) out.push({ text: "clone suspected — refused", tone: "danger" });
  if (p.backup_state) out.push({ text: "synced", tone: "plain" });
  else if (p.backup_eligible) out.push({ text: "syncable", tone: "plain" });
  else out.push({ text: "device-bound", tone: "plain" });
  return out;
}

/** `?enroll=1` on the Account → Security URL starts enrollment. */
export function wantsEnroll(search: string): boolean {
  return new URLSearchParams(search).get("enroll") === "1";
}

/** The label a new passkey gets when the person leaves it blank (the server
 * defaults the same way). */
export function passkeyLabel(raw: string): string | undefined {
  const t = raw.trim();
  return t ? t.slice(0, 64) : undefined;
}

// --- StepUpConfirmationRequest (the confirm page) -------------------------

/** A request's state in words. */
export function requestStateText(state: string): string {
  switch (state) {
    case "pending":
      return "Waiting for your confirmation.";
    case "executing":
      return "Confirmed — running now.";
    case "executed":
      return "Confirmed and run.";
    case "failed":
      return "Confirmed, but the operation failed when it ran.";
    case "expired":
      return "Expired without being confirmed — the requester has to ask again.";
    default:
      return state;
  }
}

/** Is it still waiting for a confirmation? (Expiry is checked too: a pending
 * row past its stamp can no longer be confirmed even before the reaper marks
 * it.) */
export function requestConfirmable(r: { state: string; expires_at: number }, now: number): boolean {
  return r.state === "pending" && r.expires_at > now;
}

/** The confirm endpoints' refusals, each in words. */
export function requestErrorText(status: number, body: string): string {
  const text = body.trim();
  if (status === 404) return "No such request for you. It may belong to someone else, or step-up is off.";
  if (status === 409) return `Already confirmed — it ran (or is running) once.${text ? ` ${text}` : ""}`;
  if (status === 410) return "This request has expired. Ask the requester to raise it again.";
  return text || `The confirmation failed (${status}).`;
}

/**
 * The one secret a result body may carry, to show once with a copy button —
 * a minted API token's plaintext (`{token, record}`). `null` for any other
 * result, which is shown as data.
 */
export function resultSecret(body: unknown): string | null {
  if (!body || typeof body !== "object") return null;
  const t = (body as Record<string, unknown>).token;
  return typeof t === "string" && t ? t : null;
}

/** A result body for display: pretty JSON, a string as-is, nothing for an
 * empty body. A secret (see `resultSecret`) is left out — it has its own
 * once-only box, and printing it twice is how it ends up in a screenshot. */
export function resultText(body: unknown): string {
  if (body == null || body === "") return "";
  if (typeof body === "string") return body;
  if (resultSecret(body) !== null) {
    const { token: _hidden, ...rest } = body as Record<string, unknown>;
    body = rest;
  }
  try {
    return JSON.stringify(body, null, 2);
  } catch {
    return String(body);
  }
}

// --- Settings → Security --------------------------------------------------

/** Only an Owner of the Org may reset a member's passkeys. */
export function canResetPasskeys(s: { caller_role?: string | null } | null | undefined): boolean {
  return s?.caller_role === "owner";
}

/** The members a reset can name: each subject once (a person may hold a
 * binding on the org and on several projects), sorted, with whether they are
 * one of the admins holding no passkey. */
export function resetCandidates(
  bindings: { subject: string; display_name?: string | null }[],
  withoutPasskey: string[],
): { subject: string; name: string; noPasskey: boolean }[] {
  const seen = new Map<string, string>();
  for (const b of bindings) {
    if (!seen.has(b.subject)) seen.set(b.subject, b.display_name || b.subject);
  }
  const missing = new Set(withoutPasskey);
  return [...seen.entries()]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([subject, name]) => ({ subject, name, noPasskey: missing.has(subject) }));
}
