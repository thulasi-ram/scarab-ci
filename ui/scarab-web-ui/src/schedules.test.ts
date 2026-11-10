import { describe, it, expect } from "vitest";
import { fireOutcome, isScheduled, scheduledRunsHref } from "./schedules";

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


describe("isScheduled", () => {
  it("matches only the cron trigger kind", () => {
    expect(isScheduled("cron")).toBe(true);
    expect(isScheduled("push")).toBe(false);
    expect(isScheduled(null)).toBe(false);
    expect(isScheduled(undefined)).toBe(false);
  });
});

describe("scheduledRunsHref", () => {
  it("opens the Runs tab on the scheduled filter and the pipeline's short name", () => {
    expect(scheduledRunsHref("acme", "scarab", ".scarab/nightly.yaml")).toBe(
      "/acme/scarab?scheduled=1&pipeline=nightly",
    );
  });
});
