// Forge web links: a file at a commit takes each forge's own URL form.
import { describe, expect, it } from "vitest";
import { forgeFileUrl } from "./forge";

describe("forgeFileUrl", () => {
  it("writes GitHub's and Forgejo's forms", () => {
    expect(forgeFileUrl("https://github.com/acme/ci", "3f9a1c2", ".scarab/ci.yaml")).toBe(
      "https://github.com/acme/ci/blob/3f9a1c2/.scarab/ci.yaml",
    );
    expect(forgeFileUrl("https://github.acme.io/acme/ci", "3f9a1c2", ".scarab/ci.yaml")).toBe(
      "https://github.acme.io/acme/ci/blob/3f9a1c2/.scarab/ci.yaml",
    );
    expect(forgeFileUrl("https://code.acme.io/acme/ci", "3f9a1c2", ".scarab/a b#c.yaml")).toBe(
      "https://code.acme.io/acme/ci/src/commit/3f9a1c2/.scarab/a%20b%23c.yaml",
    );
  });

  it("gives nothing without a repo, a commit or a path", () => {
    expect(forgeFileUrl(null, "3f9a1c2", "x")).toBeNull();
    expect(forgeFileUrl("https://github.com/a/b", null, "x")).toBeNull();
    expect(forgeFileUrl("https://github.com/a/b", "3f9a1c2", null)).toBeNull();
    expect(forgeFileUrl("not a url", "3f9a1c2", "x")).toBeNull();
  });
});
