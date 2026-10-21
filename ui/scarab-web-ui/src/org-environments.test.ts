import { describe, it, expect } from "vitest";
import {
  DEFAULT_DEADLINE_MINUTES,
  MAX_DEADLINE_MINUTES,
  deadlineLabel,
  envPills,
  nameError,
  toForm,
  toPutBody,
  type OrgEnvironmentForm,
} from "./org-environments";

const MIN = 60_000;

const form = (o: Partial<OrgEnvironmentForm> = {}): OrgEnvironmentForm => ({
  description: "",
  deadlineMinutes: 5,
  sources: [],
  ...o,
});

describe("toPutBody", () => {
  it("sends every field, minutes as milliseconds, free text trimmed", () => {
    // A full replace: an omitted field would be reset server-side, so the
    // body must always carry the deadline and the source list.
    expect(
      toPutBody(
        form({
          description: "  the one prod ",
          deadlineMinutes: 5,
          // The vendor grouping is a routing key matched exactly, so it is sent
          // as typed (whitespace around it is an error, tested below); only the
          // free-text fields are trimmed.
          sources: [{ installation: " argocd-prod ", vendor_grouping: "prod" }],
        }),
      ),
    ).toEqual({
      body: {
        description: "the one prod",
        deployment_deadline_ms: 5 * MIN,
        deployment_sources: [{ installation: "argocd-prod", vendor_grouping: "prod" }],
      },
    });
    expect(toPutBody(form()).body).toEqual({
      description: "",
      deployment_deadline_ms: 5 * MIN,
      deployment_sources: [],
    });
  });

  it("holds the deadline to 1 minute .. 7 days, inclusive", () => {
    expect(toPutBody(form({ deadlineMinutes: 1 })).body?.deployment_deadline_ms).toBe(MIN);
    expect(toPutBody(form({ deadlineMinutes: MAX_DEADLINE_MINUTES })).body?.deployment_deadline_ms).toBe(
      MAX_DEADLINE_MINUTES * MIN,
    );
    for (const m of [0, 0.5, -1, MAX_DEADLINE_MINUTES + 1, NaN, Infinity]) {
      expect(toPutBody(form({ deadlineMinutes: m })).error, String(m)).toBeTruthy();
    }
  });

  it("drops an untouched blank row but refuses a half-filled one", () => {
    expect(
      toPutBody(form({ sources: [{ installation: " ", vendor_grouping: "" }] })).body?.deployment_sources,
    ).toEqual([]);
    expect(toPutBody(form({ sources: [{ installation: "argocd", vendor_grouping: "" }] })).error).toMatch(
      /vendor grouping/,
    );
    expect(toPutBody(form({ sources: [{ installation: "", vendor_grouping: "prod" }] })).error).toMatch(
      /installation/,
    );
  });

  it("refuses a source listed twice, even when the installation differs only by whitespace", () => {
    expect(
      toPutBody(
        form({
          sources: [
            { installation: "argocd", vendor_grouping: "prod" },
            { installation: "argocd ", vendor_grouping: "prod" },
          ],
        }),
      ).error,
    ).toMatch(/twice/);
    // Two groupings of one installation are two sources.
    expect(
      toPutBody(
        form({
          sources: [
            { installation: "argocd", vendor_grouping: "prod" },
            { installation: "argocd", vendor_grouping: "prod-eu" },
          ],
        }),
      ).error,
    ).toBeUndefined();
  });

  it("sends a vendor grouping exactly as typed, refusing an edge space instead of trimming it", () => {
    for (const vendor_grouping of [" prod", "prod ", "\tprod"]) {
      expect(
        toPutBody(form({ sources: [{ installation: "argocd", vendor_grouping }] })).error,
        JSON.stringify(vendor_grouping),
      ).toMatch(/space/);
    }
    expect(
      toPutBody(form({ sources: [{ installation: "argocd", vendor_grouping: "Team A / Prod" }] })).body
        ?.deployment_sources,
    ).toEqual([{ installation: "argocd", vendor_grouping: "Team A / Prod" }]);
  });

  it("refuses an installation name that is not a DNS-1123 label", () => {
    for (const installation of ["ArgoCD", "-argo", "argo-", "argo_cd", "a".repeat(64)]) {
      expect(
        toPutBody(form({ sources: [{ installation, vendor_grouping: "prod" }] })).error,
        installation,
      ).toBeTruthy();
    }
    expect(
      toPutBody(form({ sources: [{ installation: "a".repeat(63), vendor_grouping: "prod" }] })).error,
    ).toBeUndefined();
  });
});

describe("toForm", () => {
  it("round-trips a stored Environment through the form unchanged", () => {
    const stored = {
      name: "prod",
      description: "the one prod",
      deployment_deadline_ms: 90_500, // set through the API, not a whole minute
      deployment_sources: [{ installation: "argocd", vendor_grouping: "prod" }],
    };
    expect(toPutBody(toForm(stored)).body).toEqual({
      description: stored.description,
      deployment_deadline_ms: stored.deployment_deadline_ms,
      deployment_sources: stored.deployment_sources,
    });
  });

  it("starts a new Environment on the server's default deadline", () => {
    expect(toForm().deadlineMinutes).toBe(DEFAULT_DEADLINE_MINUTES);
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

describe("deadlineLabel", () => {
  it("reads in the largest units that fit", () => {
    expect(deadlineLabel(5 * MIN)).toBe("5 min");
    expect(deadlineLabel(60 * MIN)).toBe("1 h");
    expect(deadlineLabel(150 * MIN)).toBe("2 h 30 min");
    expect(deadlineLabel(MAX_DEADLINE_MINUTES * MIN)).toBe("7 d");
  });
});

describe("envPills", () => {
  const orgEnvs = [
    { name: "staging", deployment_deadline_ms: 30 * MIN },
    { name: "production", deployment_deadline_ms: 5 * MIN },
  ];

  it("is the sorted union of registered names, the record's names and the picked one", () => {
    expect(envPills(orgEnvs, ["production", "legacy"], "qa").map((p) => p.name)).toEqual([
      "legacy",
      "production",
      "qa",
      "staging",
    ]);
  });

  it("marks only the names with no org Environment as unregistered", () => {
    const pills = envPills(orgEnvs, ["production", "legacy"]);
    const byName = Object.fromEntries(pills.map((p) => [p.name, p]));
    expect(byName.production!.registered).toBe(true);
    expect(byName.production!.title).toMatch(/5 min/);
    // Registered but nothing deployed yet: still a pill, still pickable.
    expect(byName.staging!.registered).toBe(true);
    expect(byName.legacy!.registered).toBe(false);
    expect(byName.legacy!.title).toMatch(/unregistered · default deadline/);
  });

  it("marks nothing when the org's Environments could not be read", () => {
    // 403 or not wired: not knowing is not the same as "unregistered".
    const pills = envPills(null, ["production", "legacy"]);
    expect(pills.map((p) => p.registered)).toEqual([null, null]);
    expect(pills.map((p) => p.name)).toEqual(["legacy", "production"]);
  });
});
