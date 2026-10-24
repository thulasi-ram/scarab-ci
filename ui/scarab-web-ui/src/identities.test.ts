import { describe, it, expect } from "vitest";
import {
  addToList,
  canUnlink,
  linkNotice,
  linkable,
  loginChoices,
  memberRows,
  providerName,
} from "./identities";

const github = { id: "default", name: "GitHub", default: true };
const okta = { id: "okta", name: "Okta", default: false };

describe("loginChoices", () => {
  it("keeps the single provider-agnostic button for one provider or none", () => {
    expect(loginChoices([])).toEqual([
      { href: "/v1/auth/login", label: "Sign in", primary: true },
    ]);
    expect(loginChoices([github])).toEqual([
      { href: "/v1/auth/login?provider=default", label: "Sign in", primary: true },
    ]);
  });

  it("offers one button per provider, the default first and primary", () => {
    expect(loginChoices([okta, github])).toEqual([
      { href: "/v1/auth/login?provider=default", label: "Sign in with GitHub", primary: true },
      { href: "/v1/auth/login?provider=okta", label: "Sign in with Okta", primary: false },
    ]);
  });

  it("carries the page that needed the sign-in back as `next` (ADR-0073)", () => {
    // A step-up confirmation link opened signed-out must land on itself after
    // the login, not on the dashboard with the link lost.
    expect(loginChoices([github], "/security/confirm/abc")[0]!.href).toBe(
      "/v1/auth/login?provider=default&next=%2Fsecurity%2Fconfirm%2Fabc",
    );
    expect(loginChoices([], "/")[0]!.href).toBe("/v1/auth/login");
    // Never an absolute or protocol-relative target: the server would 400 it.
    expect(loginChoices([], "//evil.example/x")[0]!.href).toBe("/v1/auth/login");
  });
});

describe("linking", () => {
  it("offers only providers with no account linked yet", () => {
    expect(linkable([github, okta], [{ provider: "okta", handle: "ada@x" }])).toEqual([github]);
    expect(linkable([github, okta], [])).toEqual([github, okta]);
  });

  it("never lets the last way in be unlinked", () => {
    expect(canUnlink([{ provider: "okta", handle: "a" }])).toBe(false);
    expect(
      canUnlink([
        { provider: "okta", handle: "a" },
        { provider: "default", handle: "b" },
      ]),
    ).toBe(true);
  });

  it("names a provider that is no longer configured by its id", () => {
    expect(providerName([github], "gitlab")).toBe("gitlab");
    expect(providerName([github], "default")).toBe("GitHub");
  });
});

describe("linkNotice", () => {
  it("reads the callback's outcome from the query string", () => {
    expect(linkNotice("", [github])).toBeNull();
    expect(linkNotice("?identity_linked=default", [github])).toEqual({
      kind: "ok",
      text: "Linked your GitHub account. You can now sign in with it.",
    });
    const merged = linkNotice("?identity_linked=default&merged=ada-gh", [github]);
    expect(merged?.kind).toBe("ok");
    expect(merged?.text).toContain("signed in before as ada-gh");
  });

  it("an error wins and carries the server's own words", () => {
    const n = linkNotice(
      "?identity_linked=default&identity_link_error=that%20account%20belongs%20to%20%60x%60",
      [github],
    );
    expect(n).toEqual({ kind: "error", text: "that account belongs to `x`" });
  });
});

describe("memberRows", () => {
  it("folds bindings into one row per person, org role apart from projects", () => {
    const rows = memberRows([
      { subject: "github:zed", role: "viewer", project: "web", display_name: "Zed" },
      { subject: "alice", role: "admin", project: "", display_name: "Alice" },
      { subject: "alice", role: "member", project: "web", display_name: "Alice" },
      { subject: "alice", role: "viewer", project: "api", display_name: "Alice" },
      { subject: "carol", role: "admin", project: "web", claimed: false },
    ]);
    expect(rows.map((r) => r.subject)).toEqual(["alice", "carol", "github:zed"]);
    expect(rows[0]).toEqual({
      subject: "alice",
      display: "Alice",
      pending: false,
      orgRole: "admin",
      projects: [
        { project: "api", role: "viewer" },
        { project: "web", role: "member" },
      ],
    });
    expect(rows[1].pending).toBe(true);
    expect(rows[1].display).toBe("carol");
  });
});

describe("addToList", () => {
  it("appends a picked id once, normalizing separators", () => {
    expect(addToList("", "github:alice")).toBe("github:alice");
    expect(addToList("a ,b", "github:alice")).toBe("a, b, github:alice");
    expect(addToList("github:alice", "github:alice")).toBe("github:alice");
  });
});
