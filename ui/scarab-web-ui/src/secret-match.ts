// "Check a leaked value" (Org settings › Secrets): an org Admin pastes text
// found outside Scarab and the server answers with the names of the current
// values it holds that occur in it. This module is the wording around that
// answer, with no DOM: where a match lives, who gets it, the byte counter, and
// what a refusal says. The pasted text never reaches this module's output.

import type { components } from "./api/schema";
import { leakChecksOffText } from "./leak-checks";

type Schemas = components["schemas"];

/** The scope a matching secret value is stored at. */
export type SecretMatchScope = Schemas["SecretMatchScope"];

/** One stored value that occurs in the pasted text: a secret or a connection's credential. */
export type SecretMatch = Schemas["SecretMatch"];

/** A stored secret value that occurs in the pasted text. */
export type SecretValueMatch = Extract<SecretMatch, { kind: "secret" }>;

/** A forge or plugin connection whose credential occurs in the pasted text. */
export type ConnectionMatch = Extract<SecretMatch, { kind: "connection" }>;

/** `POST /v1/orgs/{org}/secrets/match`. */
export type SecretMatchAnswer = Schemas["SecretMatchResponse"];

/** The most a check takes, in UTF-8 bytes. */
export const MAX_MATCH_BYTES = 4096;

/** The text's size as the server counts it: UTF-8 bytes. */
export function byteCount(text: string): number {
  return new TextEncoder().encode(text).length;
}

/** The counter under the box: `118 / 4096 B`. */
export function byteLabel(text: string): string {
  return `${byteCount(text)} / ${MAX_MATCH_BYTES} B`;
}

/** Blank as the server trims it: ASCII whitespace only. */
const isBlank = (text: string) => /^[\t\n\f\r ]*$/.test(text);

/** Check is offered for text that is not blank and fits the limit. */
export function canCheck(text: string): boolean {
  return !isBlank(text) && byteCount(text) <= MAX_MATCH_BYTES;
}

/** Where a match lives, as its row's mono line. */
export function whereText(m: SecretMatch): string {
  if (m.kind === "connection") {
    // A plugin installation can bind several connections: the slot tells them apart.
    const kind =
      m.connection === "forge" ? "forge connection" : m.slot ? `plugin connection ${m.slot}` : "plugin connection";
    return m.base_url ? `${kind} · ${m.base_url}` : kind;
  }
  switch (m.scope) {
    case "org":
      return "org default";
    case "org_environment":
      return `${m.environment ?? ""} (org)`;
    case "repo":
      return `${m.repo ?? ""} default`;
    case "repo_environment":
      return `${m.repo ?? ""} · ${m.environment ?? ""}`;
  }
}

/** The name a row leads with: the key, or the connection's name. */
export function matchName(m: SecretMatch): string {
  return m.kind === "secret" ? m.key : m.name;
}

/** `a`, `a and b`, `a, b and c`. */
function listOf(items: string[]): string {
  if (items.length <= 1) return items.join("");
  return `${items.slice(0, -1).join(", ")} and ${items[items.length - 1]}`;
}

/**
 * Who gets a match today, as one line under its row. `warn` when no Run gets
 * the value: every place that would get it sets its own, so rotating it here
 * changes no Run, but the credential still works where it was issued.
 */
export function usageLine(m: SecretMatch): { text: string; warn: boolean } {
  if (m.kind === "connection") {
    return {
      text:
        m.connection === "forge"
          ? "Scarab signs in to the forge with it."
          : "Scarab sends it on this plugin connection's calls.",
      warn: false,
    };
  }
  const over = m.overridden_in;
  if (!m.in_use) {
    const why = over.length > 0 ? `; every place that would get it sets its own: ${listOf(over)}` : "";
    return {
      text: `No Run gets this value today${why}. It still works where it was issued.`,
      warn: true,
    };
  }
  const env = m.environment ?? "";
  const repo = m.repo ?? "";
  const who =
    m.scope === "org"
      ? "Used by Runs of every repository"
      : m.scope === "org_environment"
        ? `Used by Runs in ${env}`
        : m.scope === "repo"
          ? `Used by Runs of ${repo}`
          : `Used by Runs of ${repo} in ${env}`;
  const except = over.length > 0 ? `, except ${listOf(over)}, which set their own` : "";
  return { text: `${who}${except}.`, warn: false };
}

const plural = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`;

/** The heading of an answer: `3 current values match`, or that none does. */
export function resultHeading(answer: SecretMatchAnswer): string {
  const n = answer.matches.length;
  if (n === 0) return "No current value matches";
  return n === 1 ? "1 current value matches" : `${n} current values match`;
}

/** What was compared: `Compared with 9 secret values, 2 repositories and 3 connections.` */
export function searchedText(answer: SecretMatchAnswer): string {
  const s = answer.searched;
  return (
    `Compared with ${plural(s.secrets, "secret value", "secret values")} across ` +
    `${plural(s.repositories, "repository", "repositories")} and ` +
    `${plural(s.connections, "connection", "connections")}.`
  );
}

/** A refused check: its status, and what the server's body said. */
export type MatchRefusal = {
  status: number;
  /** The body's `code`: `rate_limited`, `step_up_required`, … */
  code?: string;
  /** The server's own message, when it sent one. */
  message?: string;
  /** For a 429: when the next check is allowed (unix ms). */
  retryAt?: number;
  /** The org checked against, for a refusal that names it. */
  org?: string;
};

/** `14:05`, in the viewer's time zone. */
function clockTime(ms: number): string {
  const d = new Date(ms);
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
}

/** `at 14:05`, `tomorrow at 14:05` or `on 27 Nov at 14:05`, seen from `now`.
 * The daily limit can put the next check on another day. */
function whenText(ms: number, now: number): string {
  const day = (t: number) => {
    const d = new Date(t);
    return new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
  };
  const days = Math.round((day(ms) - day(now)) / 86_400_000);
  if (days <= 0) return `at ${clockTime(ms)}`;
  if (days === 1) return `tomorrow at ${clockTime(ms)}`;
  const d = new Date(ms);
  const month = d.toLocaleString("en-GB", { month: "short" });
  return `on ${d.getDate()} ${month} at ${clockTime(ms)}`;
}

/** What a refused check says above the dialog's buttons. */
export function refusalText(r: MatchRefusal, now: number = Date.now()): string {
  if (r.status === 429) {
    // The hourly or the daily limit: `retryAt` is when both allow a check.
    const when = r.retryAt ? ` The next check is allowed ${whenText(r.retryAt, now)}.` : "";
    return `You have run as many checks as the org allows each person for now.${when}`;
  }
  if (r.status === 413) {
    return `The text is longer than ${MAX_MATCH_BYTES} bytes. Paste only the part you found.`;
  }
  if (r.status === 403) {
    if (r.code === "leak_checks_off") return leakChecksOffText(r.org ?? "this org");
    if (r.code === "step_up_required") return "The check needs a passkey confirmation, and none was given.";
    if (r.code === "fresh_login_required") return r.message ?? "Log in again to confirm with a passkey.";
    return "Only an org Admin can check a value against the org's secrets.";
  }
  if (r.status === 400) return r.message ?? "Paste the text to check.";
  return r.message ?? `The check failed (${r.status}).`;
}
