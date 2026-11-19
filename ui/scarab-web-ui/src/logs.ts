// Pure log-line classification (extracted from components/StepPane.tsx): a
// coarse per-line level that drives the log body's row accents.

export type Level = "err" | "warn" | "ok" | "cmd" | "";

/** Classify one log line: a `$ `-prefixed command, an error/panic, a warning,
 * a success-ish line, or plain output. */
export function levelOf(line: string): Level {
  if (/^\s*\$ /.test(line)) return "cmd";
  // `panic(ked)?`: a real Rust panic prints "thread '…' panicked at …" —
  // `\bpanic\b` alone missed it (bug found by the no-DOM test tier).
  if (/\b(error|panic(ked)?|fatal)\b/i.test(line) || /^error(\[|:)/i.test(line)) return "err";
  if (/\bwarn(ing)?\b/i.test(line)) return "warn";
  if (/\b(finished|passed|ok|success(ful)?)\b/i.test(line)) return "ok";
  return "";
}

/** What an attempt's log body shows when it has no lines yet:
 * - `output`  — there is output; render the lines.
 * - `waiting` — nothing yet, but more may come: the try is still running
 *               (even when its stream ended: the pane opens it again), or
 *               the stream is open and the outcome is not known yet.
 * - `none`    — nothing came and nothing will: the try has finished, or the
 *               stream ended and the outcome is not known. The pane then says
 *               no output was captured and, for a failed try, why it failed.
 * A try with no output used to show "waiting for output…" forever, so its
 * failure reason never appeared. */
export type LogBodyState = "output" | "waiting" | "none";

export function logBodyState(opts: {
  /** The buffered text for this (step, attempt, container), if any arrived. */
  text: string | undefined;
  /** The log stream closed (the server ends it when nothing more will come). */
  ended: boolean;
  /** The attempt's outcome (`running`, `succeeded`, `failed`, …). */
  outcome: string | undefined;
}): LogBodyState {
  if ((opts.text ?? "").split("\n").some((l) => l.length > 0)) return "output";
  if (opts.outcome === "running") return "waiting";
  if (opts.ended) return "none";
  return opts.outcome === undefined ? "waiting" : "none";
}
