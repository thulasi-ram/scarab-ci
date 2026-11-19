// Log-line level classification (extracted from StepPane) — drives row accents.
import { describe, it, expect } from "vitest";
import { levelOf, logBodyState } from "./logs";

describe("levelOf", () => {
  it("classifies commands, errors, warnings and success lines", () => {
    expect(levelOf("$ cargo test --workspace")).toBe("cmd");
    expect(levelOf("  $ indented command")).toBe("cmd");
    expect(levelOf("error[E0308]: mismatched types")).toBe("err");
    expect(levelOf("thread 'main' panicked at src/lib.rs")).toBe("err");
    expect(levelOf("warning: unused variable `x`")).toBe("warn");
    expect(levelOf("    Finished test [unoptimized] target(s) in 48.21s")).toBe("ok");
    expect(levelOf("test executor::retry::backoff ... ok")).toBe("ok");
  });

  it("plain output has no level", () => {
    expect(levelOf("   Compiling scarab-core v0.4.0")).toBe("");
    expect(levelOf("")).toBe("");
  });

  it("error outranks a success word on the same line", () => {
    expect(levelOf("error: test failed, ok count 3")).toBe("err");
  });
});

// Regression: a try with no log output showed "waiting for output…" forever,
// so the failure reason under it never rendered.
describe("logBodyState", () => {
  it("any non-empty line is output", () => {
    expect(logBodyState({ text: "hello\n", ended: false, outcome: "running" })).toBe("output");
  });

  it("waits while the stream is open and the try is running", () => {
    expect(logBodyState({ text: undefined, ended: false, outcome: "running" })).toBe("waiting");
    expect(logBodyState({ text: undefined, ended: false, outcome: undefined })).toBe("waiting");
  });

  it("says nothing was captured once the stream ended", () => {
    expect(logBodyState({ text: undefined, ended: true, outcome: undefined })).toBe("none");
    expect(logBodyState({ text: "\n", ended: true, outcome: "failed" })).toBe("none");
  });

  // Regression: a stream the server ended while the try still ran said "no
  // log output was captured" for a try that went on to print.
  it("keeps waiting while the try runs, even after its stream ended", () => {
    expect(logBodyState({ text: undefined, ended: true, outcome: "running" })).toBe("waiting");
  });

  it("says nothing was captured for a finished try, even before the stream closes", () => {
    expect(logBodyState({ text: undefined, ended: false, outcome: "failed" })).toBe("none");
    expect(logBodyState({ text: undefined, ended: false, outcome: "succeeded" })).toBe("none");
  });
});
