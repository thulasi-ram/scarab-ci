import { describe, it, expect } from "vitest";
import type { SecretMatrix } from "./api/client";
import {
  envSummary,
  foldLabel,
  isOrgKey,
  keySummary,
  matrixFromNames,
  namesSetAt,
  scopeLines,
  sortedKeys,
  stateText,
} from "./secret-keys";

// acme/web with four environments: the org holds GHCR_TOKEN and SENTRY_DSN,
// the repo holds DATABASE_URL and SENTRY_DSN (overriding the org's),
// DATABASE_URL is overridden in production and staging, and SLACK_WEBHOOK_URL
// is set only in production, unset on purpose in qa.
const envs = ["production", "staging", "qa", "dev"];
const matrix: SecretMatrix = {
  columns: ["", ...envs],
  environments: envs,
  keys: [
    {
      key: "SENTRY_DSN",
      status: { "": "set", production: "inherited", staging: "inherited", qa: "inherited", dev: "inherited" },
      inherited_from: { production: "repo", staging: "repo", qa: "repo", dev: "repo" },
      overrides: { "": "org" },
    },
    {
      key: "GHCR_TOKEN",
      status: { "": "inherited", production: "inherited", staging: "inherited", qa: "inherited", dev: "inherited" },
      inherited_from: { "": "org", production: "org", staging: "org", qa: "org", dev: "org" },
      overrides: {},
    },
    {
      key: "SLACK_WEBHOOK_URL",
      status: { "": "unset", production: "set", staging: "unset", qa: "silenced", dev: "unset" },
      inherited_from: {},
      overrides: {},
    },
    {
      key: "DATABASE_URL",
      status: { "": "set", production: "set", staging: "set", qa: "inherited", dev: "inherited" },
      inherited_from: { qa: "repo", dev: "repo" },
      overrides: { production: "repo", staging: "repo" },
    },
  ],
};
const row = (key: string) => matrix.keys.find((r) => r.key === key)!;

describe("keys", () => {
  it("sorts the keys by name", () => {
    expect(sortedKeys(matrix).map((r) => r.key)).toEqual([
      "DATABASE_URL",
      "GHCR_TOKEN",
      "SENTRY_DSN",
      "SLACK_WEBHOOK_URL",
    ]);
  });

  it("tags only a key whose default comes from the org", () => {
    expect(matrix.keys.filter(isOrgKey).map((r) => r.key)).toEqual(["GHCR_TOKEN"]);
  });

  it("lists the names set at exactly one scope, for the overwrite guard", () => {
    expect(namesSetAt(matrix, "").sort()).toEqual(["DATABASE_URL", "SENTRY_DSN"]);
    expect(namesSetAt(matrix, "production").sort()).toEqual(["DATABASE_URL", "SLACK_WEBHOOK_URL"]);
  });

  it("builds a default-only matrix from the repo's names", () => {
    const m = matrixFromNames(["B", "A"]);
    expect(sortedKeys(m).map((r) => r.key)).toEqual(["A", "B"]);
    expect(keySummary(m, m.keys[0])).toBe("default set");
    expect(scopeLines(m, m.keys[0])).toEqual({ shown: [""], folded: [] });
  });
});

describe("keySummary", () => {
  it("says where each key is set", () => {
    expect(keySummary(matrix, row("GHCR_TOKEN"))).toBe("inherited everywhere");
    expect(keySummary(matrix, row("SENTRY_DSN"))).toBe("default set, overrides org");
    expect(keySummary(matrix, row("DATABASE_URL"))).toBe(
      "default set · overridden in production and staging",
    );
    expect(keySummary(matrix, row("SLACK_WEBHOOK_URL"))).toBe(
      "unset by default · set in production · unset on purpose in qa",
    );
  });

  it("counts instead of naming past two environments", () => {
    const all = {
      ...row("DATABASE_URL"),
      status: { "": "set", production: "set", staging: "set", qa: "set", dev: "inherited" },
    } as const;
    expect(keySummary(matrix, all)).toBe("default set · overridden in 3 of 4");
  });

  it("says an org key comes from the org when the repo has no environments", () => {
    const m: SecretMatrix = {
      columns: [""],
      environments: [],
      keys: [{ key: "X", status: { "": "inherited" }, inherited_from: { "": "org" }, overrides: {} }],
    };
    expect(keySummary(m, m.keys[0])).toBe("from org");
  });
});

describe("envSummary", () => {
  it("reads one environment's resolved state and its source", () => {
    expect(envSummary(row("DATABASE_URL"), "production")).toBe(
      "set here, overrides the Repository default",
    );
    expect(envSummary(row("DATABASE_URL"), "qa")).toBe("from the Repository default");
    expect(envSummary(row("GHCR_TOKEN"), "qa")).toBe("from the org");
    expect(envSummary(row("SLACK_WEBHOOK_URL"), "staging")).toBe("unset");
    expect(envSummary(row("SLACK_WEBHOOK_URL"), "qa")).toBe("unset on purpose");
  });
});

describe("scopeLines", () => {
  it("shows the default and the scopes set here, and folds the inherited ones", () => {
    expect(scopeLines(matrix, row("DATABASE_URL"))).toEqual({
      shown: ["", "production", "staging"],
      folded: ["qa", "dev"],
    });
  });

  it("keeps unset-on-purpose scopes out of the fold", () => {
    expect(scopeLines(matrix, row("SLACK_WEBHOOK_URL"))).toEqual({
      shown: ["", "production", "qa"],
      folded: ["staging", "dev"],
    });
  });

  it("keeps the picked environment out of the fold", () => {
    expect(scopeLines(matrix, row("SENTRY_DSN"), "qa")).toEqual({
      shown: ["", "qa"],
      folded: ["production", "staging", "dev"],
    });
  });

  it("does not fold a single line", () => {
    const one = {
      ...row("DATABASE_URL"),
      status: { "": "set", production: "set", staging: "set", qa: "set", dev: "inherited" },
    } as const;
    expect(scopeLines(matrix, one)).toEqual({ shown: ["", ...envs], folded: [] });
  });

  it("names what the fold holds", () => {
    expect(foldLabel(row("DATABASE_URL"), ["qa", "dev"])).toBe("2 more inherited");
    expect(foldLabel(row("SLACK_WEBHOOK_URL"), ["staging", "dev"])).toBe("2 more unset");
    const mixed = {
      ...row("SLACK_WEBHOOK_URL"),
      status: { ...row("SLACK_WEBHOOK_URL").status, dev: "inherited" },
    } as const;
    expect(foldLabel(mixed, ["staging", "dev"])).toBe("2 more inherited or unset");
  });
});

describe("stateText", () => {
  it("says what a scope line holds and where an inherited value comes from", () => {
    expect(stateText(row("SENTRY_DSN"), "")).toBe("set here, overrides the org value");
    expect(stateText(row("SENTRY_DSN"), "qa")).toBe("inherited (Repository default)");
    expect(stateText(row("GHCR_TOKEN"), "")).toBe("inherited (org)");
    expect(stateText(row("SLACK_WEBHOOK_URL"), "dev")).toBe("unset");
    expect(stateText(row("SLACK_WEBHOOK_URL"), "qa")).toBe("unset on purpose");
  });
});

// The org's value for an Environment, as a repository sees it: AWS_DEPLOY_ROLE
// is set by the org for production and staging, and acme/web sets its own for
// production.
const orgEnvRow: SecretMatrix["keys"][number] = {
  key: "AWS_DEPLOY_ROLE",
  status: { "": "unset", production: "set", staging: "inherited", qa: "unset", dev: "unset" },
  inherited_from: { staging: "org_environment" },
  overrides: { production: "org_environment" },
};

describe("the org's value for an Environment, on a repository's list", () => {
  const m: SecretMatrix = { ...matrix, keys: [orgEnvRow] };
  it("tags the key as the org's and says where it comes from", () => {
    expect(isOrgKey(orgEnvRow)).toBe(true);
    expect(keySummary(m, orgEnvRow)).toBe(
      "unset by default · from the org in staging · overridden in production",
    );
    expect(stateText(orgEnvRow, "staging")).toBe("inherited (org value for staging)");
    expect(stateText(orgEnvRow, "production")).toBe(
      "set here, overrides the org value for production",
    );
    expect(envSummary(orgEnvRow, "staging")).toBe("from the org value for staging");
    expect(envSummary(orgEnvRow, "production")).toBe(
      "set here, overrides the org value for production",
    );
  });
});

describe("a value the repository sets in front of the org's", () => {
  // The org sets AWS_DEPLOY_ROLE for production only, and acme/web sets its
  // own for production: no Run of web gets the org's value.
  const hidden: SecretMatrix["keys"][number] = {
    key: "AWS_DEPLOY_ROLE",
    status: { "": "unset", production: "set", staging: "unset", qa: "unset", dev: "unset" },
    inherited_from: {},
    overrides: { production: "org_environment" },
  };
  const m: SecretMatrix = { ...matrix, keys: [hidden] };
  it("is not tagged as the org's, and reads as overridden", () => {
    expect(isOrgKey(hidden)).toBe(false);
    expect(keySummary(m, hidden)).toBe("unset by default · overridden in production");
  });
});

describe("the org's own list", () => {
  const orgEnvs = ["production", "staging", "qa"];
  const orgMatrix: SecretMatrix = {
    columns: ["", ...orgEnvs],
    environments: orgEnvs,
    keys: [
      {
        key: "AWS_DEPLOY_ROLE",
        status: { "": "unset", production: "set", staging: "set", qa: "unset" },
        inherited_from: {},
        overrides: {},
        repos_with_own: { production: ["web"] },
      },
      {
        key: "GHCR_TOKEN",
        status: { "": "set", production: "inherited", staging: "inherited", qa: "inherited" },
        inherited_from: { production: "org", staging: "org", qa: "org" },
        overrides: {},
        // Each Environment falls to the Org default, so the repository
        // defaults stand in front of it there too.
        repos_with_own: {
          "": ["api", "web"],
          production: ["api", "web"],
          staging: ["api", "web"],
          qa: ["api", "web"],
        },
      },
    ],
  };
  const orgRow = (key: string) => orgMatrix.keys.find((r) => r.key === key)!;
  it("names the Org default and says who sets their own", () => {
    expect(keySummary(orgMatrix, orgRow("AWS_DEPLOY_ROLE"), true)).toBe(
      "unset by default · set in production and staging · web sets its own for production",
    );
    expect(keySummary(orgMatrix, orgRow("GHCR_TOKEN"), true)).toBe(
      "default set · api and web set their own",
    );
    expect(stateText(orgRow("AWS_DEPLOY_ROLE"), "production", true)).toBe(
      "set here · web sets its own",
    );
    expect(stateText(orgRow("GHCR_TOKEN"), "qa", true)).toBe(
      "inherited (Org default) · api and web set their own",
    );
    expect(envSummary(orgRow("GHCR_TOKEN"), "staging", true)).toBe(
      "from the Org default · api and web set their own",
    );
    expect(stateText(orgRow("AWS_DEPLOY_ROLE"), "qa", true)).toBe("unset");
  });
});
