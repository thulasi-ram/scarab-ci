// Compile errors as the Run pipeline page and the Runs list show them
// (ADR-0076): one "what" line, one "fix" line, and a collapsed "Why". The
// server reads each error's fields back from the compiler's text
// (`CompileErrorDto`); these helpers only word them. Pure, so the page and
// its tests agree.

/** One compile error's fields — structurally the generated `CompileErrorDto`. */
export type CompileError = {
  kind: string;
  step?: string | null;
  library?: string | null;
  library_step?: string | null;
  command_index?: number | null;
  read?: string | null;
  input?: string | null;
  bound?: string | null;
  message: string;
};

/** The environment variable a fix names for a template: its last part, in
 *  capitals (`${{ inputs.version }}` → `VERSION`). */
export function envName(read: string | null | undefined): string {
  const inner = (read ?? "").replace(/^\$\{\{\s*/, "").replace(/\s*\}\}$/, "");
  const last = inner.split(".").pop() ?? "";
  const name = last.toUpperCase().replace(/[^A-Z0-9_]/g, "_");
  return /^[A-Z_]/.test(name) ? name : "VALUE";
}

/** The Run parameter a caller's binding reads, when it reads exactly one
 *  (`${{ inputs.service }}` → `service`). */
export function callerInput(bound: string | null | undefined): string | null {
  const m = (bound ?? "").match(/^\$\{\{\s*inputs\.([A-Za-z0-9_-]+)\s*\}\}$/);
  return m ? m[1]! : null;
}

/** Is this an error in a Step inlined from a library? */
export const isLibraryError = (e: CompileError) => !!e.library;

/** The fix, as one sentence the reader acts on; `null` when the message is
 *  the whole story. For a library Step it names only what the caller can
 *  change: the library is not editable from the Pipeline's repository. */
export function errorFix(e: CompileError): string | null {
  if (e.kind === "command_value") {
    if (isLibraryError(e)) {
      const input = callerInput(e.bound);
      const lit = e.input ? `bind a literal ${e.input}` : "bind a literal";
      return input ? `Make ${input} a choice, or ${lit}` : `${lit[0]!.toUpperCase()}${lit.slice(1)}, or a choice or number input`;
    }
    return `Pass it through env: and use "$${envName(e.read)}"`;
  }
  if (e.kind === "script_template") {
    return `Pass it through env: and read "$${envName(e.read)}" in the script`;
  }
  return null;
}

/** Why the fix is needed, in plain words. */
export function errorWhy(e: CompileError): string | null {
  if (e.kind === "command_value") {
    const base =
      "Scarab puts a value from outside the Pipeline into a command only when it is a listed choice, true or false, a non-negative number or a commit SHA. Any other string could change what the command does.";
    if (isLibraryError(e)) {
      return `${base} The library puts its ${e.input ?? "input"} input into ${e.library_step ?? "a Step"}'s command, so that input takes only a value from the Pipeline or a fixed type. You cannot change the library from this repository; its publisher can read ${e.input ?? "the input"} from env: in a new version.`;
    }
    return base;
  }
  if (e.kind === "script_template") {
    return "A value pasted into a script becomes part of the script: a value holding a quote and a command would run it. A value read from the environment stays a value.";
  }
  return null;
}

/** A suggested change for the fix, as diff lines (`+`, `-`, or a space). */
export function errorChange(e: CompileError): { sign: " " | "+" | "-"; text: string }[] {
  const name = envName(e.read);
  if ((e.kind === "command_value" && !isLibraryError(e)) || e.kind === "script_template") {
    return [
      { sign: " ", text: `  - id: ${e.step ?? "<step>"}` },
      { sign: "+", text: "    env:" },
      { sign: "+", text: `      - [${name}, "${e.read ?? ""}"]` },
      { sign: "-", text: `    command: [ … "${e.read ?? ""}" … ]` },
      { sign: "+", text: `    command: [sh, -c, '… "$${name}" …']` },
    ];
  }
  if (e.kind === "command_value") {
    const input = callerInput(e.bound);
    if (input) {
      return [
        { sign: " ", text: "interface:" },
        { sign: " ", text: "  inputs:" },
        { sign: "-", text: `    - { name: ${input}, type: string }` },
        { sign: "+", text: `    - { name: ${input}, type: choice, options: [ … ] }` },
      ];
    }
    return [
      { sign: " ", text: `  - id: ${e.step ?? "<step>"}` },
      { sign: "-", text: `    with: { ${e.input ?? "input"}: ${e.bound ?? ""} }` },
      { sign: "+", text: `    with: { ${e.input ?? "input"}: <a fixed value> }` },
    ];
  }
  return [];
}

/** The added lines of a suggested change: what "copy change" copies. */
export function changeText(lines: { sign: string; text: string }[]): string {
  return lines
    .filter((l) => l.sign === "+")
    .map((l) => l.text)
    .join("\n");
}

/** The checks a failed compile still passed, for the dimmed rows above the
 *  errors. */
export function passedChecks(errors: CompileError[]): string[] {
  const out: string[] = [];
  if (!errors.some((e) => /^(pipeline: )?yaml parse error/.test(e.message))) out.push("Parses as a Pipeline");
  if (!errors.some((e) => /is not installed in this org/.test(e.message))) {
    out.push("Every lib: and plugin: target it invokes is installed here");
  }
  return out;
}

/** A refused Run's compact line for the Runs list: the first error's what
 *  and fix, and how many more there are. */
export function refusalSummary(errors: CompileError[], message: string): { what: string; fix: string | null; more: number } {
  const first = errors[0];
  if (!first) return { what: message, fix: null, more: 0 };
  const what =
    first.kind === "other"
      ? first.message
      : first.library
        ? `${first.step ?? ""} → lib:${first.library} ${first.library_step ?? ""} command[${first.command_index ?? "?"}] reads ${first.bound ?? first.read ?? ""}`
        : `${first.step ?? ""} command[${first.command_index ?? "?"}] reads ${first.read ?? ""}`;
  return { what, fix: errorFix(first), more: errors.length - 1 };
}
