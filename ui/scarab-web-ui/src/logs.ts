// Pure log-line classification (extracted from components/StepPane.tsx): a
// coarse per-line level that drives the log body's row accents.

export type Level = "err" | "warn" | "ok" | "cmd" | "";

/** Classify one log line: a `$ `-prefixed command, an error/panic, a warning,
 * a success-ish line, or plain output. */
export function levelOf(line: string): Level {
  if (/^\s*\$ /.test(line)) return "cmd";
  if (/\b(error|panic|fatal)\b/i.test(line) || /^error(\[|:)/i.test(line)) return "err";
  if (/\bwarn(ing)?\b/i.test(line)) return "warn";
  if (/\b(finished|passed|ok|success(ful)?)\b/i.test(line)) return "ok";
  return "";
}
