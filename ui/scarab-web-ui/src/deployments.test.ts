import { describe, it, expect } from "vitest";
import type { BoardRow, Deployment } from "./api/client";
import {
  deploymentTags,
  targetLabel,
  requesterHref,
  inFlight,
  groupBoard,
  madeLiveBy,
  pickOrg,
  safeHref,
} from "./deployments";

const dep = (over: Partial<Deployment> = {}): Deployment => ({
  id: 1,
  target: { environment: "production", source: "argocd", id: "orders-api" },
  version: "v1",
  status: "live",
  requesters: [],
  reports: [],
  created_at: 0,
  was_ever_live: true,
  unexplained: false,
  unconfirmed: false,
  ...over,
});

const keys = (d: Deployment) => deploymentTags(d).map((t) => t.key);

describe("deploymentTags", () => {
  it("is empty for an ordinary requested-and-live Deployment", () => {
    expect(keys(dep())).toEqual([]);
  });

  it("shows the server's display terms, never re-derived from requesters", () => {
    // The server owns Unexplained/Unconfirmed; an empty requester list alone
    // must not invent a tag the server did not set.
    expect(keys(dep({ requesters: [], unexplained: false }))).toEqual([]);
    expect(keys(dep({ unexplained: true }))).toEqual(["unexplained"]);
    expect(keys(dep({ status: "requested", unconfirmed: true }))).toEqual(["unconfirmed"]);
  });

  it("tells a superseded Deployment that ran apart from one that never took", () => {
    // The status alone says `superseded` for both; the story differs.
    expect(keys(dep({ status: "superseded", was_ever_live: true }))).toEqual(["was-live"]);
    expect(keys(dep({ status: "superseded", was_ever_live: false }))).toEqual(["never-live"]);
  });

  it("says nothing about past liveness unless superseded", () => {
    expect(keys(dep({ status: "failed", was_ever_live: false }))).toEqual([]);
  });

  it("marks the operator-attention terms as warnings", () => {
    const tones = deploymentTags(dep({ unexplained: true, status: "superseded" })).map((t) => t.tone);
    expect(tones).toEqual(["warn", "muted"]);
  });
});

describe("targetLabel", () => {
  it("joins the opaque source and id", () => {
    expect(targetLabel({ source: "argocd", id: "orders-api" })).toBe("argocd/orders-api");
  });
});

describe("madeLiveBy", () => {
  const r = (run: string, claim: boolean) => ({ run, step: "deploy", effective_at: 0, project: "web", claim });

  it("leaves out claims, which did not make the change", () => {
    // An Unexplained hotfix a later run named: nobody made it live.
    expect(madeLiveBy(dep({ unexplained: true, requesters: [r("late", true)] }))).toEqual([]);
    expect(madeLiveBy(dep({ requesters: [r("early", false), r("late", true)] })).map((x) => x.run)).toEqual([
      "early",
    ]);
  });
});

describe("safeHref", () => {
  it("passes http and https URLs through", () => {
    expect(safeHref("https://argo.example/app")).toBe("https://argo.example/app");
    expect(safeHref("HTTP://argo.example/app")).toBe("HTTP://argo.example/app");
  });

  it("refuses script-bearing and unparseable links", () => {
    expect(safeHref("javascript:alert(1)")).toBeNull();
    expect(safeHref(" JavaScript:alert(1)")).toBeNull();
    expect(safeHref("data:text/html,<script>alert(1)</script>")).toBeNull();
    expect(safeHref("argo.example/no-scheme")).toBeNull();
    expect(safeHref(null)).toBeNull();
    expect(safeHref(undefined)).toBeNull();
  });
});

describe("requesterHref", () => {
  it("links to the run under its project", () => {
    expect(requesterHref("acme", { run: "0190f8", project: "orders-api" })).toBe(
      "/acme/orders-api/runs/0190f8",
    );
  });

  it("is null when the project is unknown, rather than guessing a repo", () => {
    expect(requesterHref("acme", { run: "0190f8", project: null })).toBeNull();
    expect(requesterHref("acme", { run: "0190f8" })).toBeNull();
  });
});

describe("inFlight", () => {
  it("is null when the newest Deployment is the live one", () => {
    const live = dep({ id: 4 });
    expect(inFlight({ live, newest: live })).toBeNull();
  });

  it("is the newest when it differs from what is live", () => {
    const live = dep({ id: 4 });
    const newest = dep({ id: 7, status: "in_progress" });
    expect(inFlight({ live, newest })?.id).toBe(7);
  });

  it("is the newest when nothing has ever gone live", () => {
    expect(inFlight({ live: null, newest: dep({ id: 2, status: "requested" }) })?.id).toBe(2);
    expect(inFlight({ live: null, newest: null })).toBeNull();
  });
});

describe("groupBoard", () => {
  const row = (environment: string, id: string): BoardRow => ({
    target: { environment, source: "argocd", id },
  });

  it("groups by environment, sorting environments and targets", () => {
    const g = groupBoard([row("staging", "web"), row("production", "web"), row("production", "api")]);
    expect(g.map((x) => x.environment)).toEqual(["production", "staging"]);
    expect(g[0]!.rows.map((r) => r.target.id)).toEqual(["api", "web"]);
  });

  it("is empty for no rows", () => {
    expect(groupBoard([])).toEqual([]);
  });
});

describe("pickOrg", () => {
  it("prefers the administered org, then a visible project's org", () => {
    expect(pickOrg(["acme"], ["other"])).toBe("acme");
    expect(pickOrg([], ["other"])).toBe("other");
    expect(pickOrg(undefined, [])).toBeUndefined();
  });
});
