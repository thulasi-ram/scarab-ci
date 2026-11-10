import { describe, it, expect } from "vitest";
import {
  confirmMerge,
  mergedNotice,
  pendingMergeId,
  addToList,
  canUnlink,
  linkNotice,
  linkable,
  loginChoices,
  memberRows,
  providerName,
  repoAccessRows,
  resyncDiff,
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
    // What the server's `safe_next` refuses (400) is dropped, not sent: the
    // sign-in must still work.
    expect(loginChoices([], `/${"a".repeat(600)}`)[0]!.href).toBe("/v1/auth/login");
    expect(loginChoices([], "/a\\b")[0]!.href).toBe("/v1/auth/login");
    expect(loginChoices([], "/a\nb")[0]!.href).toBe("/v1/auth/login");
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
  it("lists one row per person with their org role and the repositories they also hold a role in", () => {
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
      repos: ["api", "web"],
    });
    // Repository roles only: still a member, with no org role.
    expect(rows[1]).toEqual({
      subject: "carol",
      display: "carol",
      pending: true,
      orgRole: null,
      repos: ["web"],
    });
  });
});

describe("repoAccessRows", () => {
  const bindings = [
    { subject: "a.kim", role: "owner", project: "", display_name: "Avery", origin: "native" },
    { subject: "j.lee", role: "member", project: "", display_name: "Jordan", origin: "native" },
    { subject: "j.lee", role: "admin", project: "web", display_name: "Jordan", origin: "native" },
    { subject: "r.ok", role: "viewer", project: "web", claimed: false, origin: "import" },
    { subject: "s.p", role: "member", project: "api", display_name: "Sam", origin: "import" },
    { subject: "old", role: "viewer", project: "web" },
  ];

  it("splits the people bound to the repository from those its org role reaches", () => {
    const { direct, viaOrg } = repoAccessRows(bindings, "web");
    expect(direct).toEqual([
      { subject: "j.lee", display: "Jordan", pending: false, role: "admin", source: "scarab", orgRole: "member" },
      { subject: "old", display: "old", pending: false, role: "viewer", source: null, orgRole: null },
      { subject: "r.ok", display: "r.ok", pending: true, role: "viewer", source: "forge", orgRole: null },
    ]);
    // j.lee is bound here, so is not listed again under the org; s.p's role
    // is on another repository and reaches nothing here.
    expect(viaOrg).toEqual([{ subject: "a.kim", display: "Avery", pending: false, orgRole: "owner" }]);
  });
});

describe("resyncDiff", () => {
  it("reports the forge's rows added, changed and removed, never a role granted in Scarab", () => {
    const before = [
      { subject: "amy", role: "member", project: "web", origin: "native" },
      { subject: "bo", role: "viewer", project: "web", origin: "import" },
      { subject: "cy", role: "member", project: "web", origin: "import" },
      { subject: "di", role: "viewer", project: "api", origin: "import" },
    ];
    const after = [
      { subject: "amy", role: "member", project: "web", origin: "native" },
      { subject: "bo", role: "admin", project: "web", origin: "import" },
      { subject: "ed", role: "viewer", project: "web", origin: "import", display_name: "Ed" },
      { subject: "di", role: "viewer", project: "api", origin: "import" },
    ];
    expect(resyncDiff(before, after, "web")).toEqual({
      added: [{ subject: "ed", display: "Ed", role: "viewer" }],
      changed: [{ subject: "bo", display: "bo", role: "admin", was: "viewer" }],
      removed: [{ subject: "cy", display: "cy", role: "member" }],
    });
    expect(resyncDiff(before, before, "web")).toEqual({ added: [], changed: [], removed: [] });
  });
});

describe("addToList", () => {
  it("appends a picked id once, normalizing separators", () => {
    expect(addToList("", "github:alice")).toBe("github:alice");
    expect(addToList("a ,b", "github:alice")).toBe("a, b, github:alice");
    expect(addToList("github:alice", "github:alice")).toBe("github:alice");
  });
});

describe("the merge a link asks to confirm", () => {
  it("reads the merge id from the callback's redirect", () => {
    expect(pendingMergeId("")).toBeNull();
    expect(pendingMergeId("?identity_linked=default")).toBeNull();
    expect(pendingMergeId("?identity_merge=ch-1")).toBe("ch-1");
  });

  it("says the merged account's passkeys were deleted, not moved", () => {
    expect(mergedNotice("github:ada", 1).text).toContain("Its passkey was deleted");
    expect(mergedNotice("github:ada", 2).text).toContain("Its 2 passkeys were deleted");
    expect(mergedNotice("github:ada", 1).kind).toBe("ok");
  });
});

describe("confirming a merge", () => {
  const deps = (over: Partial<Parameters<typeof confirmMerge>[2]> = {}) => {
    const calls = { reload: 0 };
    return {
      calls,
      deps: {
        assert: async () => ({ id: "cred" }),
        confirm: async () => ({ merged: "github:ada", removed_passkeys: 1 }),
        reload: () => {
          calls.reload++;
        },
        webauthnError: () => "The passkey prompt was dismissed or timed out.",
        ...over,
      },
    };
  };

  it("a refused POST has spent the challenge, so the merge is loaded again", async () => {
    const { calls, deps: d } = deps({
      confirm: async () => {
        throw new Error("that passkey is not one of `github:ada`'s");
      },
    });
    const out = await confirmMerge("m1", {}, d);
    expect(out).toEqual({ error: "that passkey is not one of `github:ada`'s" });
    expect(calls.reload).toBe(1);
  });

  it("a dismissed prompt spends nothing: no reload, the same challenge can be tried again", async () => {
    const { calls, deps: d } = deps({
      assert: async () => {
        throw Object.assign(new Error("x"), { name: "NotAllowedError" });
      },
    });
    const out = await confirmMerge("m1", {}, d);
    expect(out).toEqual({ error: "The passkey prompt was dismissed or timed out." });
    expect(calls.reload).toBe(0);
  });

  it("a sign-in linked without a merge says so", async () => {
    const { deps: d } = deps({ confirm: async () => ({ merged: null, removed_passkeys: 0 }) });
    const out = await confirmMerge("m1", {}, d);
    expect("notice" in out && out.notice.text).toContain("nothing was merged");
  });
});
