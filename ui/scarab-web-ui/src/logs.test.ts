// Log-line level classification (extracted from StepPane) — drives row accents.
import { describe, it, expect } from "vitest";
import { levelOf, logBodyState, streamEndStep } from "./logs";

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
  const st = (o: Partial<Parameters<typeof logBodyState>[0]>) =>
    logBodyState({ text: undefined, ended: false, outcome: undefined, loaded: true, ...o });

  it("any non-empty line is output", () => {
    expect(st({ text: "hello\n", outcome: "running" })).toBe("output");
    expect(st({ text: "hello\n", outcome: "failed", loaded: false, error: "boom" })).toBe("output");
  });

  // A log whose only row is the server's note ("the rest of this log was not
  // stored") shows the note, not "No log output was captured".
  it("a note from the server is a row to show, with no output", () => {
    expect(st({ markers: 1, ended: true, outcome: "succeeded" })).toBe("output");
    expect(st({ text: "\n", markers: 1, outcome: "failed", storing: true })).toBe("output");
    expect(st({ markers: 0, ended: true, outcome: "succeeded" })).toBe("none");
  });

  // Regression: while a finished try's log was still on its way the pane
  // said "No log output was captured for this try."
  it("is loading until the server answers, for finished and running tries", () => {
    expect(st({ outcome: "succeeded", loaded: false })).toBe("loading");
    expect(st({ outcome: "failed", loaded: false })).toBe("loading");
    expect(st({ outcome: "running", loaded: false })).toBe("loading");
    expect(st({ outcome: undefined, loaded: false })).toBe("loading");
    // A stream that closed before it opened is not an answer: the error
    // follows.
    expect(st({ outcome: "failed", loaded: false, ended: true })).toBe("loading");
  });

  it("waits once the stream answered and the try is running", () => {
    expect(st({ outcome: "running" })).toBe("waiting");
    expect(st({ outcome: undefined })).toBe("waiting");
  });

  it("says nothing was captured once the server answered with no output", () => {
    expect(st({ ended: true, outcome: undefined })).toBe("none");
    expect(st({ text: "\n", ended: true, outcome: "failed" })).toBe("none");
    expect(st({ outcome: "failed" })).toBe("none");
    expect(st({ outcome: "succeeded" })).toBe("none");
  });

  // Regression: a stream the server ended while the try still ran said "no
  // log output was captured" for a try that went on to print.
  it("keeps waiting while the try runs, even after its stream ended", () => {
    expect(st({ ended: true, outcome: "running" })).toBe("waiting");
  });

  // Regression: for ~90 s after a step ended the pane said "No log output was
  // captured for this try" while the server was still storing its log.
  it("says the log is still being stored while the server says so", () => {
    expect(st({ outcome: "succeeded", storing: true })).toBe("storing");
    // A stream that dropped while storing is opened again: still storing.
    expect(st({ ended: true, outcome: "failed", storing: true })).toBe("storing");
    expect(st({ text: "hello\n", outcome: "succeeded", storing: true })).toBe("output");
    expect(st({ ended: true, outcome: "succeeded", storing: false })).toBe("none");
  });

  it("says the log could not be read while there is an error, and not after it clears", () => {
    expect(st({ outcome: "failed", loaded: false, error: "the server answered 500" })).toBe("error");
    expect(st({ outcome: "running", error: "log storage read failed: timeout" })).toBe("error");
    expect(st({ outcome: "succeeded", ended: true, error: "x" })).toBe("error");
    // A later event clears the error: the state follows what came.
    expect(st({ outcome: "succeeded", error: null })).toBe("none");
    expect(st({ outcome: "succeeded", error: null, text: "late\n" })).toBe("output");
  });
});

// Regression: a try that finished ended its stream while the polled outcome
// still said running, so the pane reopened it and read the whole log again.
describe("streamEndStep", () => {
  it("leaves the stream closed when the try finished", () => {
    expect(streamEndStep({ outcome: "succeeded", graceOver: false, hadOutput: true })).toBe("done");
    expect(streamEndStep({ outcome: "failed", graceOver: false, hadOutput: false })).toBe("done");
  });

  it("waits one poll when the try still reads as running", () => {
    expect(streamEndStep({ outcome: "running", graceOver: false, hadOutput: true })).toBe("wait");
  });

  it("leaves it closed when the try turned out finished after the wait", () => {
    expect(streamEndStep({ outcome: "succeeded", graceOver: true, hadOutput: true })).toBe("done");
  });

  it("reopens when the try still runs after the wait", () => {
    expect(streamEndStep({ outcome: "running", graceOver: true, hadOutput: true })).toBe("reopen");
    expect(streamEndStep({ outcome: "running", graceOver: true, hadOutput: false })).toBe("reopen");
  });

  it("reads a finished try once more when nothing arrived", () => {
    expect(streamEndStep({ outcome: "failed", graceOver: true, hadOutput: false })).toBe("reopen");
  });

  it("leaves a step that is out of view alone", () => {
    expect(streamEndStep({ outcome: undefined, graceOver: true, hadOutput: false })).toBe("done");
  });
});
