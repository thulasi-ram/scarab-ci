import { describe, expect, it } from "vitest";
import {
  MAX_MATCH_BYTES,
  byteCount,
  byteLabel,
  canCheck,
  refusalText,
  resultHeading,
  searchedText,
  usageLine,
  whereText,
  type SecretMatch,
  type SecretValueMatch,
} from "./secret-match";

const secret = (over: Partial<SecretValueMatch>): SecretValueMatch => ({
  kind: "secret",
  key: "DEPLOY_TOKEN",
  scope: "org",
  overridden_in: [],
  in_use: true,
  ...over,
});

describe("whereText", () => {
  it("names each scope the way the key list does", () => {
    expect(whereText(secret({ scope: "org" }))).toBe("org default");
    expect(whereText(secret({ scope: "org_environment", environment: "production" }))).toBe(
      "production (org)",
    );
    expect(whereText(secret({ scope: "repo", repo: "web" }))).toBe("web default");
    expect(whereText(secret({ scope: "repo_environment", repo: "web", environment: "staging" }))).toBe(
      "web · staging",
    );
  });

  it("names a connection by its kind, with its address when it has one", () => {
    const forge: SecretMatch = {
      kind: "connection",
      connection: "forge",
      id: "forgejo-1",
      name: "codeberg",
      base_url: "https://codeberg.org",
    };
    expect(whereText(forge)).toBe("forge connection · https://codeberg.org");
    expect(whereText({ kind: "connection", connection: "plugin", id: "p", name: "argocd" })).toBe(
      "plugin connection",
    );
  });

  it("names a plugin connection's slot, so two connections of one installation differ", () => {
    const plugin = (slot: string): SecretMatch => ({
      kind: "connection",
      connection: "plugin",
      id: "0000000001",
      name: "argocd",
      slot,
      base_url: "https://argo.acme.test",
    });
    expect(whereText(plugin("staging"))).toBe("plugin connection staging · https://argo.acme.test");
    expect(whereText(plugin("production"))).not.toBe(whereText(plugin("staging")));
  });
});

describe("usageLine", () => {
  it("says who gets a value in use, and who sets their own in front of it", () => {
    expect(usageLine(secret({ scope: "org" }))).toEqual({
      text: "Used by Runs of every repository.",
      warn: false,
    });
    expect(
      usageLine(
        secret({ scope: "org_environment", environment: "production", overridden_in: ["web", "api · production"] }),
      ),
    ).toEqual({
      text: "Used by Runs in production, except web and api · production, which set their own.",
      warn: false,
    });
    expect(usageLine(secret({ scope: "repo_environment", repo: "web", environment: "staging" })).text).toBe(
      "Used by Runs of web in staging.",
    );
  });

  it("warns when no Run gets the value, and says it still works where it was issued", () => {
    const line = usageLine(
      secret({ scope: "org_environment", environment: "production", in_use: false, overridden_in: ["web", "api"] }),
    );
    expect(line.warn).toBe(true);
    expect(line.text).toBe(
      "No Run gets this value today; every place that would get it sets its own: web and api. It still works where it was issued.",
    );
    // No repository and no Environment: nothing overrides it, and still nothing gets it.
    expect(usageLine(secret({ in_use: false })).text).toBe(
      "No Run gets this value today. It still works where it was issued.",
    );
  });

  it("never warns on a connection", () => {
    expect(usageLine({ kind: "connection", connection: "forge", id: "f", name: "f" }).warn).toBe(false);
  });
});

describe("byteCount", () => {
  it("counts UTF-8 bytes, not characters", () => {
    expect(byteCount("abc")).toBe(3);
    expect(byteCount("é")).toBe(2);
    expect(byteCount("🔑")).toBe(4);
    expect(byteLabel("x".repeat(118))).toBe("118 / 4096 B");
  });

  it("offers Check only for text that is not blank and fits the limit", () => {
    expect(canCheck("")).toBe(false);
    expect(canCheck(" \n\t\r\n")).toBe(false);
    expect(canCheck("tok")).toBe(true);
    expect(canCheck("x".repeat(MAX_MATCH_BYTES))).toBe(true);
    expect(canCheck("x".repeat(MAX_MATCH_BYTES + 1))).toBe(false);
    // 2048 two-byte characters fit; one more ASCII byte does not.
    expect(canCheck("é".repeat(2048))).toBe(true);
    expect(canCheck(`${"é".repeat(2048)}x`)).toBe(false);
  });
});

describe("the answer's wording", () => {
  const searched = { secrets: 9, repositories: 1, connections: 2 };
  it("counts the matches and what was compared", () => {
    expect(resultHeading({ matches: [], searched })).toBe("No current value matches");
    expect(resultHeading({ matches: [secret({})], searched })).toBe("1 current value matches");
    expect(resultHeading({ matches: [secret({}), secret({ key: "B" })], searched })).toBe(
      "2 current values match",
    );
    expect(searchedText({ matches: [], searched })).toBe(
      "Compared with 9 secret values across 1 repository and 2 connections.",
    );
  });
});

describe("refusalText", () => {
  it("gives the time of the next check on a rate limit, and the day when it is not today", () => {
    const now = new Date(2026, 10, 25, 13, 40).getTime();
    const limit = "You have run as many checks as the org allows each person for now.";
    const later = new Date(2026, 10, 25, 14, 5).getTime();
    expect(refusalText({ status: 429, code: "rate_limited", retryAt: later }, now)).toBe(
      `${limit} The next check is allowed at 14:05.`,
    );
    // The daily limit can move the next check to tomorrow.
    const tomorrow = new Date(2026, 10, 26, 9, 30).getTime();
    expect(refusalText({ status: 429, code: "rate_limited", retryAt: tomorrow }, now)).toBe(
      `${limit} The next check is allowed tomorrow at 09:30.`,
    );
    expect(refusalText({ status: 429, code: "rate_limited" }, now)).toBe(limit);
  });

  it("says the org has the check turned off, and where to turn it on", () => {
    expect(refusalText({ status: 403, code: "leak_checks_off", org: "acme" })).toBe(
      "Leak checks are turned off for acme. An org Owner can turn them on in Settings › Security.",
    );
  });

  it("says when the text is too long", () => {
    expect(refusalText({ status: 413 })).toBe(
      "The text is longer than 4096 bytes. Paste only the part you found.",
    );
  });

  it("says only an org Admin may check, and tells a cancelled step-up apart", () => {
    expect(refusalText({ status: 403, message: "forbidden" })).toBe(
      "Only an org Admin can check a value against the org's secrets.",
    );
    expect(refusalText({ status: 403, code: "step_up_required" })).toBe(
      "The check needs a passkey confirmation, and none was given.",
    );
  });
});
