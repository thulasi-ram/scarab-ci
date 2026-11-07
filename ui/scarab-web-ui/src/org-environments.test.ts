import { describe, it, expect } from "vitest";
import { decodeParam } from "./settings-nav";
import {
  approvalGateFacts,
  confirmsName,
  durationText,
  idleTimerWarning,
  requiredApprovalsError,
  toBaselineBody,
  copiedSummary,
  displayNameError,
  duplicateChecklist,
  environmentEditHref,
  environmentLabel,
  environmentHref,
  environmentsHref,
  nameBeside,
  nameError,
  planSave,
  runSave,
  DELETED_ELSEWHERE,
  shownName,
  toDuplicateBody,
  toEditForm,
  toPutBody,
  type DuplicateResultLike,
  type EditableEnvironmentLike,
  type EnvironmentWrite,
} from "./org-environments";

describe("toPutBody", () => {
  it("sends the display name and description trimmed", () => {
    expect(toPutBody({ displayName: " Production ", description: "  the one prod " })).toEqual({
      body: { display_name: "Production", description: "the one prod" },
    });
  });

  it("sends a blank display name as none, so the name shows", () => {
    expect(toPutBody({ displayName: "   ", description: "" }).body.display_name).toBeNull();
  });
});

describe("display name", () => {
  it("shows the display name with the name beside it, or the name alone", () => {
    const named = { name: "prod", display_name: "Production (EU)" };
    expect(shownName(named)).toBe("Production (EU)");
    expect(nameBeside(named)).toBe("prod");
    expect(environmentLabel(named)).toBe("Production (EU) · prod");
    for (const plain of [{ name: "prod" }, { name: "prod", display_name: null }, { name: "prod", display_name: " " }, { name: "prod", display_name: "prod" }]) {
      expect(shownName(plain)).toBe("prod");
      expect(nameBeside(plain)).toBeNull();
      expect(environmentLabel(plain)).toBe("prod");
    }
  });

  it("refuses control characters and more than 200 characters", () => {
    expect(displayNameError("")).toBeNull();
    expect(displayNameError("Production (EU)")).toBeNull();
    expect(displayNameError("x".repeat(200))).toBeNull();
    expect(displayNameError("x".repeat(201))).toBeTruthy();
    expect(displayNameError("bad\u0007")).toBeTruthy();
  });
});

describe("duplicate", () => {
  it("sends the new name, and the display name only when one is typed", () => {
    expect(toDuplicateBody("prod-eu", "", ["prod"])).toEqual({ body: { name: "prod-eu" } });
    expect(toDuplicateBody("prod-eu", " Production (EU) ", ["prod"])).toEqual({
      body: { name: "prod-eu", display_name: "Production (EU)" },
    });
  });

  it("refuses a taken or invalid name before sending", () => {
    expect(toDuplicateBody("prod", "", ["prod"]).error).toMatch(/already registered/);
    expect(toDuplicateBody("", "", []).error).toBeTruthy();
    expect(toDuplicateBody("..", "", []).error).toBeTruthy();
    expect(toDuplicateBody("x", "bad\u0007", []).error).toBeTruthy();
  });

  const result: DuplicateResultLike = {
    environment: { name: "prod-eu" },
    copied: {
      overlays: ["web"],
      privilege_grants: ["api"],
      secrets: [
        { project: "api", keys: ["DB_PASSWORD"] },
        { project: "web", keys: ["API_KEY", "DB_PASSWORD"] },
      ],
    },
    not_copied: {
      policies: [
        { id: "p1", name: "no-fridays", project: null, environments: ["prod"] },
        { id: "p2", name: "two-approvals", project: "web", environments: ["prod"] },
      ],
      projects_with_runs: ["checkout", "web"],
      old_oidc_subject: "scarab:org/acme/repo/*/env/prod/ref/*",
      new_oidc_subject: "scarab:org/acme/repo/*/env/prod-eu/ref/*",
      notes: ["Run and approval history and the Cache stay with prod, which is not changed and stays until an admin deletes it."],
    },
  };

  it("summarises what was copied, never a value", () => {
    expect(copiedSummary(result)).toEqual([
      "baseline rules and the projects that may use it",
      "1 project overlay",
      "1 privilege grant",
      "3 secrets with their values (api, web)",
    ]);
  });

  it("lists what was not copied: pipelines, policies, trust policies, then notes", () => {
    const list = duplicateChecklist("prod", result);
    expect(list.map((i) => i.title)).toEqual([
      "Change the pipelines",
      "Review the policies",
      "Update cloud trust policies",
      "Note",
    ]);
    expect(list[0]!.items).toEqual(["checkout", "web"]);
    expect(list[1]!.items).toEqual(["no-fridays", "web / two-approvals"]);
    expect(list[2]!.detail).toContain("scarab:org/acme/repo/*/env/prod-eu/ref/*");
  });

  it("leaves out empty items but always asks about cloud trust policies", () => {
    const bare = {
      ...result,
      not_copied: { ...result.not_copied, policies: [], projects_with_runs: [], notes: [] },
    };
    expect(duplicateChecklist("prod", bare).map((i) => i.title)).toEqual(["Update cloud trust policies"]);
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
      { part: "details", body: { display_name: null, description: "the other prod" } },
    ]);
  });

  it("sends the display name with the description it keeps", () => {
    const form = toEditForm(stored);
    form.displayName = "Production";
    expect(planSave(stored, form).writes).toEqual([
      { part: "details", body: { display_name: "Production", description: "the one prod" } },
    ]);
    const named = { ...stored, display_name: "Production" };
    expect(planSave(named, toEditForm(named))).toEqual({ writes: [] });
    const cleared = toEditForm(named);
    cleared.displayName = " ";
    expect(planSave(named, cleared).writes).toEqual([
      { part: "details", body: { display_name: null, description: "the one prod" } },
    ]);
  });

  it("sends nothing when the display name has a control character", () => {
    const form = toEditForm(stored);
    form.displayName = "bad\u0007";
    expect(planSave(stored, form).error).toMatch(/control characters/);
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
            missing_gate_mode: "add",
            added_gate_expiry_s: null,
            required_approvals: null,
            max_approval_age_s: 86400,
          },
        },
      ],
    });
  });

  it("sends every changed part in order: details, baseline, eligibility", () => {
    const form = toEditForm(stored);
    form.allProjects = true;
    form.baseline.requireReason = false;
    form.description = "prod, EU";
    expect(planSave(stored, form).writes?.map((w) => w.part)).toEqual([
      "details",
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

describe("planSave: approval gate settings", () => {
  const stored: EditableEnvironmentLike = {
    name: "prod",
    description: "",
    protection: {
      approvers: ["a.kim", "j.lee"],
      allowed_refs: [],
      wait_timer_s: 0,
      require_reason: false,
      approval_gate: {
        missing_gate_mode: "add",
        added_gate_expiry_s: 3600,
        required_approvals: 1,
        max_approval_age_s: 86400,
      },
    },
    eligibility: { all_projects: true, projects: [] },
  };

  it("reads the stored settings into the form and sends nothing unchanged", () => {
    const form = toEditForm(stored);
    expect(form.gate).toEqual({
      missingGateMode: "add",
      addedGateExpiryS: "3600",
      requiredApprovals: "1",
      maxApprovalAgeS: "86400",
    });
    expect(planSave(stored, form)).toEqual({ writes: [] });
  });

  it("treats a baseline with no settings as the defaults", () => {
    const bare = { ...stored, protection: { ...stored.protection, approval_gate: null } };
    const form = toEditForm(bare);
    expect(form.gate).toEqual({
      missingGateMode: "add",
      addedGateExpiryS: "",
      requiredApprovals: "",
      maxApprovalAgeS: "86400",
    });
    expect(planSave(bare, form)).toEqual({ writes: [] });
  });

  it("sends the whole baseline, every setting included, when one setting changes", () => {
    const form = toEditForm(stored);
    form.gate.missingGateMode = "refuse";
    form.gate.requiredApprovals = "";
    expect(planSave(stored, form).writes).toEqual([
      {
        part: "baseline",
        body: {
          approvers: ["a.kim", "j.lee"],
          allowed_refs: [],
          wait_timer_s: 0,
          require_reason: false,
          missing_gate_mode: "refuse",
          added_gate_expiry_s: 3600,
          required_approvals: null,
          max_approval_age_s: 86400,
        },
      },
    ]);
  });

  it("refuses a required count above the number of approvers", () => {
    const form = toEditForm(stored);
    form.gate.requiredApprovals = "3";
    expect(planSave(stored, form).error).toMatch(/from 1 to 2/);
  });

  it("refuses a required count with no approvers", () => {
    const form = toEditForm(stored);
    form.baseline.approvers = "";
    expect(planSave(stored, form).error).toMatch(/Add approvers first/);
  });

  it("does not resend a timer the form cannot edit, and sends 0 with the baseline", () => {
    // An Environment saved before the timer needed approvers.
    const timerOnly: EditableEnvironmentLike = {
      ...stored,
      protection: { approvers: [], allowed_refs: [], wait_timer_s: 300, require_reason: false },
    };
    const form = toEditForm(timerOnly);
    expect(planSave(timerOnly, form)).toEqual({ writes: [] });
    form.baseline.requireReason = true;
    const w = planSave(timerOnly, form).writes?.[0];
    expect(w?.part).toBe("baseline");
    expect(w?.body).toMatchObject({ wait_timer_s: 0, require_reason: true });
  });
});

describe("toBaselineBody", () => {
  const rules = { approvers: "a.kim", allowedRefs: "", waitTimerS: "60", requireReason: false };
  const gate = { missingGateMode: "add" as const, addedGateExpiryS: "", requiredApprovals: "", maxApprovalAgeS: "" };

  it("takes blank as the default: no expiry, every approver, 24 hours", () => {
    expect(toBaselineBody(rules, gate).body).toMatchObject({
      wait_timer_s: 60,
      added_gate_expiry_s: null,
      required_approvals: null,
      max_approval_age_s: 86400,
    });
  });

  it("refuses an expiry or maximum age that is not a whole number above 0", () => {
    expect(toBaselineBody(rules, { ...gate, addedGateExpiryS: "0" }).error).toMatch(/expiry/);
    expect(toBaselineBody(rules, { ...gate, maxApprovalAgeS: "1.5" }).error).toMatch(/maximum approval age/);
  });
});

describe("requiredApprovalsError", () => {
  it("accepts blank and 1 to the number of approvers", () => {
    expect(requiredApprovalsError("", 0)).toBeNull();
    expect(requiredApprovalsError("2", 2)).toBeNull();
    expect(requiredApprovalsError("0", 2)).toMatch(/from 1 to 2/);
    expect(requiredApprovalsError("1", 0)).toMatch(/Add approvers first/);
  });
});

describe("idleTimerWarning", () => {
  it("warns only for a timer with no approvers", () => {
    expect(idleTimerWarning({ approvers: [], wait_timer_s: 300 })).toMatch(
      /5 minutes wait timer does nothing until this environment has approvers/,
    );
    expect(idleTimerWarning({ approvers: ["a.kim"], wait_timer_s: 300 })).toBeNull();
    expect(idleTimerWarning({ approvers: [], wait_timer_s: 0 })).toBeNull();
  });
});

describe("durationText", () => {
  it("names at most two units", () => {
    expect(durationText(86400)).toBe("24 hours");
    expect(durationText(5400)).toBe("1 hour 30 minutes");
    expect(durationText(45)).toBe("45 seconds");
    expect(durationText(90061)).toBe("25 hours 1 minute");
  });
});

describe("approvalGateFacts", () => {
  it("says every approver, or n of m", () => {
    const facts = (g: Parameters<typeof approvalGateFacts>[0], n: number) =>
      Object.fromEntries(approvalGateFacts(g, n).map((f) => [f.label, f.value]));
    expect(facts(null, 2)).toEqual({
      "missing gate": "add an approval gate to the run",
      "approvals needed": "every approver",
      "added gate expiry": "none: waits for a decision",
      "max approval age": "24 hours",
    });
    expect(
      facts({ missing_gate_mode: "refuse", required_approvals: 1, added_gate_expiry_s: 7200, max_approval_age_s: 3600 }, 3),
    ).toEqual({
      "missing gate": "refuse the run",
      "approvals needed": "1 of 3",
      "added gate expiry": "2 hours",
      "max approval age": "1 hour",
    });
  });
});

describe("runSave", () => {
  const writes: EnvironmentWrite[] = [
    { part: "details", body: { display_name: null, description: "prod, EU" } },
    { part: "eligibility", body: { all_projects: true, projects: [] } },
  ];

  // Another admin deleted the Environment after this page loaded. The
  // details write is a create-or-replace, so sending it would bring the
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
    expect(sent).toEqual(["details", "eligibility"]);
  });

  it("stops at the first failed write and names what saved", async () => {
    const problem = await runSave(writes, {
      stillThere: async () => true,
      send: async (w) => {
        if (w.part === "eligibility") throw new Error("pattern list is empty");
      },
    });
    expect(problem).toBe(
      "Could not save the list of projects that may use it: pattern list is empty. The display name and description saved.",
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
