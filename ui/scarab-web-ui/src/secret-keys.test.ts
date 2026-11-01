import { describe, it, expect } from "vitest";
import type { SecretMatrix } from "./api/client";
import { secretKeyNote, secretKeyRows } from "./secret-keys";

// acme/web: the org holds GHCR_TOKEN and SENTRY_DSN, the repo holds
// DATABASE_URL and SENTRY_DSN (overriding the org), and SLACK_WEBHOOK_URL is set
// only in production.
const matrix: SecretMatrix = {
  columns: ["", "production", "qa"],
  environments: ["production", "qa"],
  keys: [
    {
      key: "SENTRY_DSN",
      status: { "": "set", production: "inherited", qa: "inherited" },
      inherited_from: { production: "repo", qa: "repo" },
      overrides: { "": "org" },
    },
    {
      key: "GHCR_TOKEN",
      status: { "": "inherited", production: "inherited", qa: "inherited" },
      inherited_from: { "": "org", production: "org", qa: "org" },
      overrides: {},
    },
    {
      key: "SLACK_WEBHOOK_URL",
      status: { "": "unset", production: "set", qa: "silenced" },
      inherited_from: {},
      overrides: {},
    },
    {
      key: "DATABASE_URL",
      status: { "": "set", production: "set", qa: "set" },
      inherited_from: {},
      overrides: { production: "repo", qa: "repo" },
    },
    {
      key: "OLD_MARKER",
      status: { "": "silenced", production: "unset", qa: "unset" },
      inherited_from: {},
      overrides: {},
    },
  ],
};

describe("secretKeyRows", () => {
  it("lists the repo's keys first, then the org's, then environment-only keys", () => {
    const rows = secretKeyRows(["SENTRY_DSN", "DATABASE_URL"], matrix);
    expect(rows.map((r) => `${r.kind}:${r.name}`)).toEqual([
      "own:DATABASE_URL",
      "own:SENTRY_DSN",
      "org:GHCR_TOKEN",
      "env:SLACK_WEBHOOK_URL",
    ]);
  });

  it("marks a repo key that hides an org key, and only that one", () => {
    const rows = secretKeyRows(["SENTRY_DSN", "DATABASE_URL"], matrix);
    const own = Object.fromEntries(
      rows.flatMap((r) => (r.kind === "own" ? [[r.name, r.overridesOrg]] : [])),
    );
    // DATABASE_URL is overridden BY environments; it overrides nothing itself.
    expect(own).toEqual({ SENTRY_DSN: true, DATABASE_URL: false });
  });

  it("does not repeat an org key the repo overrides as inherited", () => {
    const rows = secretKeyRows(["SENTRY_DSN"], matrix);
    expect(rows.filter((r) => r.name === "SENTRY_DSN")).toHaveLength(1);
  });

  it("names the environments an environment-only key lives in", () => {
    const env = secretKeyRows([], matrix).find((r) => r.name === "SLACK_WEBHOOK_URL");
    expect(env).toEqual({ name: "SLACK_WEBHOOK_URL", kind: "env", environments: ["production"] });
  });

  it("falls back to the repo's own keys when the matrix is missing", () => {
    expect(secretKeyRows(["B", "A"], null)).toEqual([
      { name: "A", kind: "own", overridesOrg: false },
      { name: "B", kind: "own", overridesOrg: false },
    ]);
  });
});

describe("secretKeyNote", () => {
  it("says where a key comes from", () => {
    expect(secretKeyNote({ name: "X", kind: "org" })).toBe("from org");
    expect(secretKeyNote({ name: "X", kind: "env", environments: ["production", "qa"] })).toBe(
      "only in production, qa",
    );
    expect(secretKeyNote({ name: "X", kind: "own", overridesOrg: true })).toBe("overrides org");
    expect(secretKeyNote({ name: "X", kind: "own", overridesOrg: false })).toBe("");
  });
});
