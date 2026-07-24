// Log-line level classification (extracted from StepPane) — drives row accents.
import { describe, it, expect } from "vitest";
import { levelOf } from "./logs";

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
