import { describe, it, expect } from "vitest";
import { nameError, toForm, toPutBody } from "./org-environments";

describe("toPutBody", () => {
  it("sends the description trimmed", () => {
    expect(toPutBody({ description: "  the one prod " })).toEqual({
      body: { description: "the one prod" },
    });
  });
});

describe("toForm", () => {
  it("round-trips a stored Environment through the form unchanged", () => {
    const stored = { name: "prod", description: "the one prod" };
    expect(toPutBody(toForm(stored)).body).toEqual({ description: stored.description });
  });

  it("starts a new Environment blank", () => {
    expect(toForm()).toEqual({ description: "" });
  });
});

describe("nameError", () => {
  it("admits any name a pipeline can deploy to and refuses only empty, dot segments and control characters", () => {
    for (const n of ["prod", "Prod-EU", "prod.eu-west-1", "staging_2", "Production EU", "prod/eu", " prod", "...", "x".repeat(500)]) {
      expect(nameError(n), n).toBeNull();
    }
    for (const n of ["", ".", "..", "prod\t", "prod\n", "prod\u0000", "prod\u007f"]) expect(nameError(n), JSON.stringify(n)).toBeTruthy();
  });
});
