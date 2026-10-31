import { describe, it, expect } from "vitest";
import {
  callerInput,
  changeText,
  envName,
  errorChange,
  errorFix,
  errorWhy,
  passedChecks,
  refusalSummary,
  type CompileError,
} from "./compile-errors";

const own: CompileError = {
  kind: "command_value",
  step: "tag",
  command_index: 3,
  read: "${{ inputs.version }}",
  message: "step `tag`: `command[3]` reads `${{ inputs.version }}`, …",
};
const library: CompileError = {
  kind: "command_value",
  step: "ship",
  library: "acme/deploy-helm",
  library_step: "helm",
  command_index: 4,
  read: "${{ inputs.release }}",
  input: "release",
  bound: "${{ inputs.service }}",
  message: "library `acme/deploy-helm`, step `helm`: …",
};

describe("compile errors", () => {
  it("names the env var and the caller's parameter", () => {
    expect(envName("${{ inputs.version }}")).toBe("VERSION");
    expect(envName("${{ outputs.build.image-ref }}")).toBe("IMAGE_REF");
    expect(envName("${{ 1 }}")).toBe("VALUE");
    expect(callerInput("${{ inputs.service }}")).toBe("service");
    expect(callerInput("x-${{ inputs.service }}")).toBeNull();
  });

  it("fixes a Step in the Pipeline in the Step, and a library Step only on the caller's side", () => {
    expect(errorFix(own)).toBe('Pass it through env: and use "$VERSION"');
    expect(errorFix(library)).toBe("Make service a choice, or bind a literal release");
    expect(errorFix({ ...library, bound: "${{ outputs.build.tag }}" })).toBe(
      "Bind a literal release, or a choice or number input",
    );
    // "Ask the publisher" is only ever in the Why.
    expect(errorFix(library)).not.toMatch(/publisher/);
    expect(errorWhy(library)).toMatch(/publisher/);
    expect(errorFix({ kind: "other", message: "x" })).toBeNull();
    expect(errorWhy({ kind: "other", message: "x" })).toBeNull();
    expect(errorFix({ ...own, kind: "script_template" })).toMatch(/in the script/);
  });

  it("suggests a change and copies only its added lines", () => {
    expect(changeText(errorChange(own))).toBe(
      '    env:\n      - [VERSION, "${{ inputs.version }}"]\n    command: [sh, -c, \'… "$VERSION" …\']',
    );
    expect(changeText(errorChange(library))).toBe("    - { name: service, type: choice, options: [ … ] }");
    expect(errorChange({ kind: "other", message: "m" })).toEqual([]);
  });

  it("lists what still passed and sums up a refusal", () => {
    expect(passedChecks([own])).toEqual(["Parses as a Pipeline", "Every lib: and plugin: target it invokes is installed here"]);
    expect(passedChecks([{ kind: "other", message: "pipeline: yaml parse error: x" }])).toEqual([
      "Every lib: and plugin: target it invokes is installed here",
    ]);
    expect(refusalSummary([own, library], "m")).toEqual({
      what: "tag command[3] reads ${{ inputs.version }}",
      fix: 'Pass it through env: and use "$VERSION"',
      more: 1,
    });
    expect(refusalSummary([library], "m").what).toBe(
      "ship → lib:acme/deploy-helm helm command[4] reads ${{ inputs.service }}",
    );
    expect(refusalSummary([], "the message")).toEqual({ what: "the message", fix: null, more: 0 });
    expect(refusalSummary([{ kind: "other", message: "yanked: r" }], "m").what).toBe("yanked: r");
  });
});
