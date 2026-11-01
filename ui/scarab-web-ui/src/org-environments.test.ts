import { describe, it, expect } from "vitest";
import {
  confirmsName,
  environmentEditHref,
  environmentHref,
  environmentsHref,
  nameError,
  nameFromParam,
  planSave,
  toEditForm,
  toForm,
  toPutBody,
  type EditableEnvironmentLike,
} from "./org-environments";

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

describe("environment pages", () => {
  it("put the name in one encoded segment under the Environments section", () => {
    expect(environmentsHref("acme")).toBe("/orgs/acme/settings/environments");
    expect(environmentHref("acme", "prod")).toBe("/orgs/acme/settings/environments/prod");
    expect(environmentEditHref("acme", "prod")).toBe("/orgs/acme/settings/environments/prod/edit");
    // A name with `/` or a space stays one segment: five for the view, six
    // for the edit page, whatever the name holds.
    const view = environmentHref("acme", "prod/eu west");
    expect(view).toBe("/orgs/acme/settings/environments/prod%2Feu%20west");
    expect(view.split("/").filter(Boolean)).toHaveLength(5);
    expect(environmentEditHref("acme", "edit").split("/").filter(Boolean)).toHaveLength(6);
  });

  it("read back the exact name from the route param", () => {
    for (const n of ["prod", "prod/eu west", "100%", "Prod-EU", "edit"]) {
      const seg = environmentHref("acme", n).split("/").pop()!;
      expect(nameFromParam(seg)).toBe(n);
    }
    // A malformed escape is taken as typed rather than throwing.
    expect(nameFromParam("bad%zz")).toBe("bad%zz");
  });
});

describe("confirmsName", () => {
  it("accepts only the exact name", () => {
    expect(confirmsName("prod", "prod")).toBe(true);
    expect(confirmsName("Prod", "prod")).toBe(false);
    expect(confirmsName("prod ", "prod")).toBe(false);
    expect(confirmsName("", "prod")).toBe(false);
    expect(confirmsName("", "")).toBe(false);
  });
});

describe("planSave", () => {
  const stored: EditableEnvironmentLike = {
    name: "prod",
    description: "the one prod",
    protection: {
      approvers: ["a.kim", "j.lee"],
      allowed_refs: ["refs/heads/main"],
      wait_timer_s: 0,
      require_reason: true,
    },
    eligibility: { all_projects: false, projects: ["payments", "team-a-*"] },
  };

  it("sends nothing when nothing changed", () => {
    expect(planSave(stored, toEditForm(stored))).toEqual({ writes: [] });
  });

  it("sends nothing when only spacing changed", () => {
    const form = toEditForm(stored);
    form.description = "  the one prod ";
    form.baseline.approvers = "a.kim,j.lee , ";
    form.patterns = " payments ,team-a-*";
    expect(planSave(stored, form)).toEqual({ writes: [] });
  });

  it("sends only the part that changed", () => {
    const form = toEditForm(stored);
    form.baseline.waitTimerS = "30";
    expect(planSave(stored, form)).toEqual({
      writes: [
        {
          part: "baseline",
          body: {
            approvers: ["a.kim", "j.lee"],
            allowed_refs: ["refs/heads/main"],
            wait_timer_s: 30,
            require_reason: true,
          },
        },
      ],
    });
  });

  it("sends every changed part in order: description, baseline, eligibility", () => {
    const form = toEditForm(stored);
    form.allProjects = true;
    form.baseline.requireReason = false;
    form.description = "prod, EU";
    expect(planSave(stored, form).writes?.map((w) => w.part)).toEqual([
      "description",
      "baseline",
      "eligibility",
    ]);
  });

  it("treats a changed pattern list as an eligibility change", () => {
    const form = toEditForm(stored);
    form.patterns = "payments";
    expect(planSave(stored, form).writes).toEqual([
      { part: "eligibility", body: { all_projects: false, projects: ["payments"] } },
    ]);
  });

  it("ignores the hidden pattern list while all projects stay eligible", () => {
    const open = { ...stored, eligibility: { all_projects: true, projects: [] } };
    const form = toEditForm(open);
    form.patterns = "payments";
    expect(planSave(open, form)).toEqual({ writes: [] });
  });

  it("sends nothing when the baseline does not parse", () => {
    const form = toEditForm(stored);
    form.description = "changed";
    form.baseline.waitTimerS = "1.5";
    const plan = planSave(stored, form);
    expect(plan.writes).toBeUndefined();
    expect(plan.error).toMatch(/wait timer/);
  });
});
