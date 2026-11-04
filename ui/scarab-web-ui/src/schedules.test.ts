import { describe, it, expect } from "vitest";
import { fireOutcome } from "./schedules";

describe("fireOutcome", () => {
  it("reads a refused or missed slot as a failure, as the refused list does", () => {
    expect(fireOutcome("refused").tone).toBe("failed");
    expect(fireOutcome("missed").tone).toBe("failed");
    expect(fireOutcome("run").tone).toBe("");
  });

  it("shows an outcome it does not know as the server sent it", () => {
    expect(fireOutcome("deferred")).toEqual({ label: "deferred", tone: "" });
  });
});

