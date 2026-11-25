import { describe, expect, it } from "vitest";
import { canEditLeakChecks, leakChecksOff, leakChecksOffText, limitsText, validateLimits } from "./leak-checks";

const bounds = { max_per_hour: 100, max_per_day: 1000 };

describe("limitsText", () => {
  it("names both limits", () => {
    expect(limitsText({ per_hour: 20, per_day: 40 })).toBe("20 checks per person per hour · 40 per day");
    expect(limitsText({ per_hour: 1, per_day: 5 })).toBe("1 check per person per hour · 5 per day");
  });

  it("says the check is off when either limit is 0", () => {
    expect(leakChecksOff({ per_hour: 0, per_day: 40 })).toBe(true);
    expect(leakChecksOff({ per_hour: 20, per_day: 0 })).toBe(true);
    expect(leakChecksOff({ per_hour: 20, per_day: 40 })).toBe(false);
    expect(limitsText({ per_hour: 20, per_day: 0 })).toBe("Turned off: no one in the org can run a check.");
  });
});

describe("leakChecksOffText", () => {
  it("names the org and where to turn the check on", () => {
    expect(leakChecksOffText("acme")).toBe(
      "Leak checks are turned off for acme. An org Owner can turn them on in Settings › Security.",
    );
  });
});

describe("canEditLeakChecks", () => {
  it("lets only an Owner change the limits", () => {
    expect(canEditLeakChecks({ caller_role: "owner" })).toBe(true);
    expect(canEditLeakChecks({ caller_role: "admin" })).toBe(false);
    expect(canEditLeakChecks(null)).toBe(false);
  });
});

describe("validateLimits", () => {
  it("takes whole numbers within the bounds, 0 and the maximum included", () => {
    expect(validateLimits(" 20 ", "40", bounds)).toEqual({ ok: true, value: { per_hour: 20, per_day: 40 } });
    expect(validateLimits("0", "0", bounds)).toEqual({ ok: true, value: { per_hour: 0, per_day: 0 } });
    expect(validateLimits("100", "1000", bounds)).toEqual({ ok: true, value: { per_hour: 100, per_day: 1000 } });
  });

  it("names each box that is out of bounds", () => {
    expect(validateLimits("101", "40", bounds)).toEqual({
      ok: false,
      hour: "Checks per hour must be a whole number from 0 to 100.",
    });
    expect(validateLimits("20", "1001", bounds)).toEqual({
      ok: false,
      day: "Checks per day must be a whole number from 0 to 1000.",
    });
    expect(validateLimits("-1", "2.5", bounds)).toEqual({
      ok: false,
      hour: "Checks per hour must be a whole number from 0 to 100.",
      day: "Checks per day must be a whole number from 0 to 1000.",
    });
    expect(validateLimits("", "40", bounds)).toMatchObject({ ok: false, hour: expect.any(String) });
  });
});
