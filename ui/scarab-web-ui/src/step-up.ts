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
 * Run the confirmation for one refusal: `true` once the server has accepted a
 * StepUpConfirmation (retry the request), `false` if the human cancelled or it
 * cannot be done here (enroll mode, no challenge) — the caller then gets the
 * original 403.
 */
export type Ceremony = (refusal: StepUpRequired) => Promise<boolean>;

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
  for (let round = 0; round < MAX_STEP_UP_ROUNDS; round++) {
    const refusal = await readStepUp(current);
    if (!refusal || refusal.code !== "step_up_required") return current;
    if (!(await ceremony(refusal))) return current;
    current = await retry();
  }
  return current;
}

/**
 * One ceremony at a time, shared: a refusal that arrives while a ceremony is
 * already on screen waits for THAT one's answer instead of stacking a second
 * dialog. A page that fires three saves at once gets one prompt; with a
 * window confirmation all three retries pass, and a request that needed
 * something else (its own action) is refused again on retry and gets its own
 * round then. A cancel answers every sharer with its own original 403.
 */
export function shareCeremony(run: Ceremony): Ceremony {
  let inflight: Promise<boolean> | null = null;
  return (refusal) => {
    if (inflight) return inflight;
    const p = run(refusal).finally(() => {
      if (inflight === p) inflight = null;
    });
    inflight = p;
    return p;
  };
}

// --- the dialog's state ---------------------------------------------------
//
// A module-level signal so the one dialog mounted in Layout serves every
// request in the app, and so the API client (which has no component tree) can
// open it. Reading it renders the dialog; `settle` closes it with an answer.

export type StepUpPrompt = {
  refusal: StepUpRequired;
  /** Close the dialog: `true` once the server accepted the confirmation. */
  settle: (confirmed: boolean) => void;
};

const [stepUpPrompt, setStepUpPrompt] = createSignal<StepUpPrompt | null>(null);
export { stepUpPrompt };

/** Open the dialog for `refusal`; resolves with the human's answer. The
 * dialog does the passkey ceremony itself — it needs the click (browsers
 * demand a user gesture for `navigator.credentials.get`). */
export function askStepUp(refusal: StepUpRequired): Promise<boolean> {
  return new Promise((resolve) => {
    let done = false;
    setStepUpPrompt({
      refusal,
      settle: (confirmed) => {
        if (done) return;
        done = true;
        setStepUpPrompt(null);
        resolve(confirmed);
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
