// The web UI cites no decision record (an ADR number) in its code, its copy
// or its mock data: a reader of the UI does not have them. That includes the
// generated API client (api/schema.ts): it carries the server's API docs, so the
// fix for a reference there is in the server's doc comments. A field doc in the
// policy editor comes from the server's policy schema, which the scarab-policy
// tests hold to the same rule.
import { readdirSync, readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

const SRC = new URL("./", import.meta.url);
const REF = /ADR-\d{4}/;

const files = readdirSync(SRC, { recursive: true })
  .map((f) => f.replaceAll("\\", "/"))
  .filter((f) => /\.(ts|tsx|json|css|html)$/.test(f));

describe("the UI source", () => {
  it("is read", () => {
    expect(files).toContain("mock-policy-checkpoints.json");
    expect(files).toContain("components/ServicePane.tsx");
    expect(files).toContain("api/schema.ts");
  });

  it("cites no decision record", () => {
    const cited = files.flatMap((f) =>
      readFileSync(new URL(f, SRC), "utf8")
        .split("\n")
        .flatMap((line, i) => (REF.test(line) ? [`${f}:${i + 1}: ${line.trim()}`] : [])),
    );
    expect(cited).toEqual([]);
  });
});
