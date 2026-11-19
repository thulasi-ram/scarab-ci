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

/** What the pane does about a log stream the server ended:
 * - `done`   — leave it closed: the try had finished, so the log is whole.
 * - `wait`   — the try still read as running. The run's status is polled, so
 *              it may only lag the try's end: wait one more poll.
 * - `reopen` — open the stream again. Either the try still runs after that
 *              wait (the Pod had not started, or the connection dropped), or
 *              it finished with nothing in the buffer, so one more read costs
 *              nothing on screen.
 * A try that finishes ends its stream while the polled outcome may still say
 * `running`; reopening then read the whole log a second time. */
export type StreamEnd = "done" | "wait" | "reopen";

export function streamEndStep(opts: {
  /** The attempt's outcome now, or undefined when its step is not in view. */
  outcome: string | undefined;
  /** The wait of one poll after the end is over. */
  graceOver: boolean;
  /** The buffer holds output for this stream. */
  hadOutput: boolean;
}): StreamEnd {
  if (opts.outcome === "running") return opts.graceOver ? "reopen" : "wait";
  if (!opts.graceOver || opts.outcome === undefined) return "done";
  return opts.hadOutput ? "done" : "reopen";
}

/** The text to show while a reopened stream replays a log from the start.
 * The old buffer stays until the replay has as much, so the pane does not
 * shrink and grow back and keeps its scroll. When the replay ends short (the
 * stream also ends on a dropped connection), the longer text stays. */
export function replayBuffer(opts: {
  /** The text on screen. */
  shown: string;
  /** The replay so far. */
  replay: string;
  /** The replaying stream has ended. */
  ended: boolean;
}): { text: string; caughtUp: boolean } {
  if (opts.replay.length >= opts.shown.length) return { text: opts.replay, caughtUp: true };
  return { text: opts.shown, caughtUp: opts.ended };
}
