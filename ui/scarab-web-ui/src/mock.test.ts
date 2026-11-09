// The mock's copy of the server's baseline + overlay merge, checked against
// the merge cases the server's own test reads
// (crates/scarab-server/tests/effective_protection_cases.rs). A change to the
// server's merge updates that file, and this suite then fails until the mock
// follows.
import { describe, expect, it } from "vitest";
import type { components } from "./api/schema";
import fixture from "../../../crates/scarab-server/tests/fixtures/effective_protection_cases.json";
import { effectiveOf } from "./mock";

type RulesDto = components["schemas"]["EnvironmentProtectionRulesDto"];
type OverlayDto = components["schemas"]["EnvironmentProjectOverlayDto"];

interface MergeCase {
  name: string;
  baseline: RulesDto;
  overlay: OverlayDto | null;
  effective: components["schemas"]["EffectiveProtectionDto"];
}

const cases = fixture.cases as MergeCase[];

describe("mock effectiveOf", () => {
  it("reads a non-empty fixture", () => {
    expect(cases.length).toBeGreaterThan(0);
  });

  it.each(cases.map((c) => [c.name, c] as const))("%s", (_name, c) => {
    expect(effectiveOf(c.baseline, c.overlay ?? undefined)).toEqual(c.effective);
  });
});
