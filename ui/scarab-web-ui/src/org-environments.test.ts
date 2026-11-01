import { describe, it, expect } from "vitest";
import { decodeParam } from "./settings-nav";
import {
  confirmsName,
  environmentEditHref,
  environmentHref,
  environmentsHref,
  nameError,
  planSave,
  runSave,
  DELETED_ELSEWHERE,
  toEditForm,
  toPutBody,
  type EditableEnvironmentLike,
  type EnvironmentWrite,
} from "./org-environments";

describe("toPutBody", () => {
  it("sends the description trimmed", () => {
    expect(toPutBody({ description: "  the one prod " })).toEqual({
      body: { description: "the one prod" },
    });
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
      expect(decodeParam(seg)).toBe(n);
    }
    // A malformed escape is taken as typed rather than throwing.
    expect(decodeParam("bad%zz")).toBe("bad%zz");
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

  it("sends nothing when the stored description has edge spaces and the form is untouched", () => {
    const spaced = { ...stored, description: "  the one prod \n" };
    expect(planSave(spaced, toEditForm(spaced))).toEqual({ writes: [] });
  });

  it("sends the trimmed description when it changed beyond its edge spaces", () => {
    const spaced = { ...stored, description: "  the one prod " };
    const form = toEditForm(spaced);
    form.description = "the other prod";
    expect(planSave(spaced, form).writes).toEqual([
      { part: "description", body: { description: "the other prod" } },
    ]);
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

describe("runSave", () => {
  const writes: EnvironmentWrite[] = [
    { part: "description", body: { description: "prod, EU" } },
    { part: "eligibility", body: { all_projects: true, projects: [] } },
  ];

  // Another admin deleted the Environment after this page loaded. The
  // description write is a create-or-replace, so sending it would bring the
  // Environment back with the default baseline and undo the delete.
  it("sends nothing and says so when the environment was deleted since the page loaded", async () => {
    const sent: EnvironmentWrite[] = [];
    const problem = await runSave(writes, {
      stillThere: async () => false,
      send: async (w) => {
        sent.push(w);
      },
    });
    expect(problem).toBe(DELETED_ELSEWHERE);
    expect(sent).toEqual([]);
  });

  it("sends every write in order when the environment is still there", async () => {
    const sent: string[] = [];
    const problem = await runSave(writes, {
      stillThere: async () => true,
      send: async (w) => {
        sent.push(w.part);
      },
    });
    expect(problem).toBeNull();
    expect(sent).toEqual(["description", "eligibility"]);
  });

  it("stops at the first failed write and names what saved", async () => {
    const problem = await runSave(writes, {
      stillThere: async () => true,
      send: async (w) => {
        if (w.part === "eligibility") throw new Error("pattern list is empty");
      },
    });
    expect(problem).toBe(
      "Could not save the list of projects that may deploy: pattern list is empty. The description saved.",
    );
  });

  it("sends nothing when the check itself fails", async () => {
    const sent: EnvironmentWrite[] = [];
    const problem = await runSave(writes, {
      stillThere: async () => {
        throw new Error("network down");
      },
      send: async (w) => {
        sent.push(w);
      },
    });
    expect(problem).toBe("Could not check the environment before saving: network down.");
    expect(sent).toEqual([]);
  });
});
