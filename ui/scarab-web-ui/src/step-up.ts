// Step-up confirmation in the UI (ADR-0073): reading the server's refusal,
// and retrying the refused request once the human has confirmed.
//
// A crown-jewel operation (writing a secret, changing a role, minting a token…)
// answers `403 {code: "step_up_required", mode, summary, challenge}` until the
// browser session holds a fresh StepUpConfirmation. The UI's job is to turn
// that 403 into a dialog, a passkey prompt, a `POST /v1/me/step-up`, and the
// SAME request again — so every call site in the app keeps its plain
// "await the API" shape and never learns step-up exists.
//
// This module is the part of that which is logic, not DOM: parsing the
// refusal, the retry loop, and sharing one ceremony among concurrent refusals.
// The dialog itself (components/StepUpDialog.tsx) and the wiring into the API
// client (api/client.ts) are thin around it.
import { createSignal } from "solid-js";
import type { components } from "./api/schema";

/** What is being confirmed, redacted by the server ("(hidden)" for secrets). */
export type OpSummary = components["schemas"]["OpSummary"];

/** A WebAuthn challenge ready for `navigator.credentials.*`. */
export type StepUpChallenge = {
  id: string;
  public_key: Record<string, unknown>;
  expires_at: number;
};

/**
 * `window`: a 5-minute window on this session will do (one confirmation covers
 * every window-mode op until it lapses). `action`: confirm exactly this
 * operation, once. `enroll`: the human has no passkey yet.
 */
export type StepUpMode = "window" | "action" | "enroll";

/** The body of a `403 step_up_required` (server `StepUpRefusal`). */
export type StepUpRequired = {
  code: "step_up_required";
  mode: StepUpMode;
  message: string;
  op: string;
  summary: OpSummary;
  challenge?: StepUpChallenge;
  enroll_url?: string;
  fresh_login_until?: number;
  confirmation_url?: string;
  request_id?: string;
  expires_at?: number;
};

/** The body of a `403 fresh_login_required`: a FIRST passkey needs a browser
 * login from the last 5 minutes. */
export type FreshLoginRequired = {
  code: "fresh_login_required";
  message: string;
  login_url: string;
};

export type StepUpParsed = StepUpRequired | FreshLoginRequired;

const MODES: readonly string[] = ["window", "action", "enroll"];

const str = (v: unknown): v is string => typeof v === "string";
const opt = <T>(v: T | null | undefined): T | undefined => (v == null ? undefined : v);

/**
 * Read a 403 body as a step-up refusal, or `null` if it is any other 403
 * ("insufficient role", a plain-text denial, a ceremony error).
 *
 * Strict on purpose: a refusal with an unknown `mode`, or one missing its
 * summary, is passed through as the ordinary 403 it would otherwise be rather
 * than guessed at — a future server mode should surface as an error the user
 * can read, not as a dialog that does the wrong ceremony.
 */
export function parseStepUp(body: unknown): StepUpParsed | null {
  if (typeof body === "string") {
    try {
      body = JSON.parse(body);
    } catch {
      return null;
    }
  }
  if (!body || typeof body !== "object") return null;
  const b = body as Record<string, unknown>;
  if (b.code === "fresh_login_required") {
    if (!str(b.login_url)) return null;
    return {
      code: "fresh_login_required",
      message: str(b.message) ? b.message : "Log in again.",
      login_url: b.login_url,
    };
  }
  if (b.code !== "step_up_required") return null;
  if (!str(b.mode) || !MODES.includes(b.mode)) return null;
  const s = b.summary as Record<string, unknown> | undefined;
  if (!s || !str(s.operation) || !str(s.target) || !Array.isArray(s.changes)) return null;
  const c = b.challenge as Record<string, unknown> | null | undefined;
  const challenge =
    c && str(c.id) && c.public_key && typeof c.public_key === "object"
      ? {
          id: c.id,
          public_key: c.public_key as Record<string, unknown>,
          expires_at: typeof c.expires_at === "number" ? c.expires_at : 0,
        }
      : undefined;
  return {
    code: "step_up_required",
    mode: b.mode as StepUpMode,
    message: str(b.message) ? b.message : "This needs a step-up confirmation.",
    op: str(b.op) ? b.op : "",
    summary: {
      operation: s.operation,
      target: s.target,
      changes: (s.changes as unknown[]).filter(str),
    },
    challenge,
    enroll_url: opt(b.enroll_url as string | null),
    fresh_login_until: opt(b.fresh_login_until as number | null),
    confirmation_url: opt(b.confirmation_url as string | null),
    request_id: opt(b.request_id as string | null),
    expires_at: opt(b.expires_at as number | null),
  };
}

/** Parse a response's body as a refusal without consuming it (a clone is
 * read), so the caller can still hand the original on. Non-403s are `null`. */
export async function readStepUp(res: Response): Promise<StepUpParsed | null> {
  if (res.status !== 403) return null;
  const text = await res
    .clone()
    .text()
    .catch(() => "");
  return parseStepUp(text);
}

/**
 * A ceremony's answer for one refusal:
 * - `true`: the server accepted a StepUpConfirmation for THIS refusal — retry
 *   the request; that spends one of its rounds;
 * - `false`: the human cancelled, or it cannot be done here (enroll mode, no
 *   challenge) — the caller gets the original 403;
 * - `"shared"`: this refusal waited on a ceremony another refusal had on
 *   screen (`shareCeremony`) and that one did not cancel — retry, but spend NO
 *   round: nothing was confirmed for this request, and counting it would starve
 *   a request that needs a window then its own action (git-bug 7a01b1c);
 * - `{ reissue }`: the server refused the confirmation. Its challenge is spent
 *   (the server consumes a challenge before it verifies the assertion), so
 *   "try again" over it can never succeed: re-send the original request for a
 *   fresh refusal with a fresh challenge, and show `reissue` — the server's
 *   reason — when the dialog opens again. Spends no round either.
 */
export type CeremonyAnswer = boolean | "shared" | { reissue: string };

/**
 * Run the confirmation for one refusal. `note` is the reason the previous
 * confirmation for this same request was refused (a `{ reissue }` answer), so
 * the reopened dialog can say why it is asking again.
 */
export type Ceremony = (refusal: StepUpRequired, note?: string) => Promise<CeremonyAnswer>;

/**
 * How many confirm-and-retry rounds one request gets. Two, because a window
 * then an action can chain: an op that is window-mode in general but carries
 * an always-ask part (e.g. loosening a rule) is refused once for the window
 * and, on retry, once more for the exact action. A third refusal means the
 * confirmations are not taking, and looping on it would trap the user in
 * dialogs.
 */
export const MAX_STEP_UP_ROUNDS = 2;

/**
 * How many retries one request gets that spend no round (`"shared"` and
 * `{ reissue }` answers). Each of those is driven by a human answering a
 * dialog, so this is a backstop against a loop, not a policy.
 */
export const MAX_STEP_UP_FREE_RETRIES = 5;

/**
 * The retry loop. `first` is the response to the original request; `retry`
 * re-sends that same request (a fresh copy each time — a body can be read
 * once). Anything other than a step-up refusal is returned untouched.
 */
export async function withStepUp(
  first: Response,
  retry: () => Promise<Response>,
  ceremony: Ceremony,
): Promise<Response> {
  let current = first;
  let rounds = 0;
  let free = 0;
  let note: string | undefined;
  for (;;) {
    const refusal = await readStepUp(current);
    if (!refusal || refusal.code !== "step_up_required") return current;
    if (rounds >= MAX_STEP_UP_ROUNDS || free >= MAX_STEP_UP_FREE_RETRIES) return current;
    const answer = await ceremony(refusal, note);
    if (answer === false) return current;
    note = undefined;
    if (answer === true) rounds++;
    else {
      free++;
      if (typeof answer === "object") note = answer.reissue;
    }
    current = await retry();
  }
}

/**
 * One ceremony at a time, shared: a refusal that arrives while a ceremony is
 * already on screen waits for THAT one's answer instead of stacking a second
 * dialog. A page that fires three saves at once gets one prompt; with a
 * window confirmation all three retries pass, and a request that needed
 * something else (its own action) is refused again on retry and gets its own
 * round then — the shared answer spent none of its rounds (`"shared"`). A
 * cancel answers every sharer with its own original 403.
 */
export function shareCeremony(run: Ceremony): Ceremony {
  let inflight: Promise<CeremonyAnswer> | null = null;
  return (refusal, note) => {
    if (inflight) return inflight.then((a) => (a === false ? false : "shared"));
    const p = run(refusal, note).finally(() => {
      if (inflight === p) inflight = null;
    });
    inflight = p;
    return p;
  };
}

/** What one press of "Confirm with passkey" came to. */
export type ConfirmOutcome =
  | { kind: "confirmed" }
  /** The browser side failed (prompt dismissed, timed out, no passkey API):
   * the challenge was never sent, so trying again over it is real. */
  | { kind: "retry"; error: string }
  /** The server refused the confirmation: the challenge is spent, so the
   * dialog closes with `{ reissue: error }` for a fresh one. */
  | { kind: "reissue"; error: string };

/**
 * The dialog's confirm press, minus the DOM: get an assertion over the
 * challenge, then post it. Which of the two failed decides whether the same
 * challenge can be tried again.
 */
export async function confirmOnce(
  challenge: StepUpChallenge,
  io: {
    assert: (publicKey: Record<string, unknown>) => Promise<Record<string, unknown>>;
    confirm: (challengeId: string, credential: Record<string, unknown>) => Promise<unknown>;
    browserError: (e: unknown) => string;
  },
): Promise<ConfirmOutcome> {
  let credential: Record<string, unknown>;
  try {
    credential = await io.assert(challenge.public_key);
  } catch (e) {
    return { kind: "retry", error: io.browserError(e) };
  }
  try {
    await io.confirm(challenge.id, credential);
  } catch (e) {
    return {
      kind: "reissue",
      error: e instanceof Error && e.message ? e.message : "The confirmation was refused.",
    };
  }
  return { kind: "confirmed" };
}

// --- the dialog's state ---------------------------------------------------
//
// A module-level signal so the one dialog mounted in Layout serves every
// request in the app, and so the API client (which has no component tree) can
// open it. Reading it renders the dialog; `settle` closes it with an answer.

export type StepUpPrompt = {
  refusal: StepUpRequired;
  /** Why the previous confirmation for this request was refused, if it was. */
  note?: string;
  /** Close the dialog with the ceremony's answer (`CeremonyAnswer`). */
  settle: (answer: CeremonyAnswer) => void;
};

const [stepUpPrompt, setStepUpPrompt] = createSignal<StepUpPrompt | null>(null);
export { stepUpPrompt };

/** Open the dialog for `refusal`; resolves with the human's answer. The
 * dialog does the passkey ceremony itself — it needs the click (browsers
 * demand a user gesture for `navigator.credentials.get`). */
export function askStepUp(refusal: StepUpRequired, note?: string): Promise<CeremonyAnswer> {
  return new Promise((resolve) => {
    let done = false;
    setStepUpPrompt({
      refusal,
      note,
      settle: (answer) => {
        if (done) return;
        done = true;
        setStepUpPrompt(null);
        resolve(answer);
      },
    });
  });
}

/** The heading for a refusal's dialog. */
export function stepUpTitle(r: StepUpRequired): string {
  if (r.mode === "enroll") return "Enroll a passkey first";
  if (r.mode === "action") return "Confirm this action";
  return "Confirm it's you";
}

/** One line on what confirming buys, so "confirm" never reads as a blank
 * cheque. */
export function stepUpScope(r: StepUpRequired): string {
  if (r.mode === "window")
    return "Confirming opens a 5-minute window for changes like this one in this browser session.";
  if (r.mode === "action") return "Confirming authorises exactly this change, once.";
  return "Every change like this one needs a passkey confirmation, and you have no passkey yet.";
}

/** A server error body in words: the `message` of a JSON error
 * (`{code, message}`), else the text itself. */
export function errorMessage(text: string, fallback: string): string {
  const t = text.trim();
  if (!t) return fallback;
  try {
    const j = JSON.parse(t) as { message?: unknown };
    if (j && typeof j.message === "string" && j.message) return j.message;
  } catch {
    /* plain text */
  }
  return t;
}
