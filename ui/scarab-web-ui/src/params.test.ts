import { describe, expect, it } from "vitest";
import { reconcilePrefill, type ParamSpec } from "./params";

describe("reconcilePrefill", () => {
  const specs: ParamSpec[] = [
    { name: "version", type: "string", required: true },
    { name: "channel", type: "choice", required: true, options: ["stable", "beta"] },
    { name: "parallelism", type: "number", required: false, default: 4 },
    { name: "dry_run", type: "boolean", required: false, default: false },
  ];

  it("carries values that still pass, and starts the rest from their defaults", () => {
    const r = reconcilePrefill(specs, {
      version: "0.8.1",
      channel: "nightly", // an option since removed
      parallelism: "lots", // no longer a number
      dry_run: "yes", // not a boolean
      region: "eu", // no longer declared
    });
    expect(r.values).toEqual({ version: "0.8.1", channel: "", parallelism: "4", dry_run: false });
    expect(r.reset).toEqual(["channel", "parallelism", "dry_run"]);
    expect(r.dropped).toEqual(["region"]);
  });
});
