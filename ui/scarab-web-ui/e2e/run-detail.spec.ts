// Run-detail walkthrough over the RICH mock fixture (src/mock.ts): one run
// carrying every multi-take surface — a RunRerunRequested boundary (two
// versions, superseded/shadowed attempts), a shared service + docked sidecar,
// a pending manual gate, and an artifact of record. All selectors
// are role/text-based (glossary copy, ARIA roles) — no CSS-class coupling, no
// pixel snapshots.
import { test, expect, type Page } from "@playwright/test";

// The rich run's id in src/mock.ts.
const RICH_RUN_ID = "0190f8a2000071fb8c0099887766ccdd";

// Glossary: the words "Take" and "Attempt" NEVER surface
// in user-facing copy — rows say "original run" / "you reran X", tries say
// "try N". (Lowercase "attempt started" in the raw activity log is event
// language, not the version/try lens — the ban is on the capitalized terms.)
async function expectGlossaryClean(page: Page) {
  const text = await page.locator("body").innerText();
  expect(text).not.toMatch(/\bTake\b/);
  expect(text).not.toMatch(/\bAttempt\b/);
}

test("run-detail walkthrough: DAG, versions, activity, gate, tries, artifacts", async ({
  page,
}) => {
  await page.goto(`/acme/scarab/runs/${RICH_RUN_ID}`);

  // The run mounted: breadcrumb carries the human `#N` handle, provenance the
  // Headline, and the status badge reads suspended (held by the gate).
  await expect(page.getByRole("heading", { name: /#147/ })).toBeVisible();
  await expect(page.getByText("Wire shared postgres into the integration suite")).toBeVisible();
  await expect(page.getByText("suspended", { exact: true })).toBeVisible();

  // ── (a) DAG: step nodes + services lane + docked sidecar chip ────────────
  for (const step of ["clone", "build", "test", "approve"]) {
    await expect(page.getByRole("button", { name: new RegExp(`^${step}`) })).toBeVisible();
  }
  // build took two tries in this version; the second passed.
  await expect(page.getByRole("button", { name: /^build/ })).toContainText("passed on try 2");
  // The shared-service lane: label + the postgres peer node with its lifecycle.
  await expect(page.getByText("Services", { exact: true })).toBeVisible();
  const svcNode = page.getByRole("button", { name: /postgres/ });
  await expect(svcNode).toBeVisible();
  await expect(svcNode).toContainText("ready");
  // test's docked sidecar chip (a role=button chip INSIDE the step node).
  const testNode = page.getByRole("button", { name: /^test/ });
  await expect(testNode.getByRole("button", { name: /redis/ })).toBeVisible();

  // The run opens on the gate waiting for a decision, and the URL names it by
  // its index in the run's steps, never by name.
  await expect(page.locator(".sp-name")).toHaveText("approve");
  await expect(page).toHaveURL(/[?&]step=3(&|$)/);

  // ── (b) version selector: canonical rerun language, two versions ─────────
  await page.getByRole("button", { name: /^Version 2 of 2 · latest/ }).click();
  const versionMenu = page.getByRole("menu", { name: "run versions" });
  await expect(versionMenu).toBeVisible();
  const latestRow = versionMenu.getByRole("menuitemradio", { name: /you reran from build/ });
  const originalRow = versionMenu.getByRole("menuitemradio", { name: /Version 1/ });
  await expect(latestRow).toBeVisible();
  await expect(latestRow).toContainText("by a.kim");
  await expect(originalRow).toBeVisible();
  // The first version exists because of its trigger.
  await expect(originalRow).toContainText("PR #131");
  // Superseded evidence: the original run's outcome chips carry the ⊘ bucket
  // (test@a1 was cut short by the rerun).
  await expect(originalRow).toContainText("⊘");
  // The glossary ban, checked WITH the version rows in the DOM.
  await expectGlossaryClean(page);
  await page.keyboard.press("Escape");
  await expect(versionMenu).toHaveCount(0);

  // ── (c) activity rail is newest-first ────────────────────────────────────
  await expect(page.getByText("Activity", { exact: true })).toBeVisible();
  // The newest event (approve's pending → waiting gate transition) renders
  // ABOVE the older attempt events; both strings exist only in the rail.
  const bodyText = await page.locator("body").innerText();
  const newestIdx = bodyText.indexOf("pending → waiting");
  const olderIdx = bodyText.indexOf("attempt started");
  expect(newestIdx).toBeGreaterThan(-1);
  expect(olderIdx).toBeGreaterThan(-1);
  expect(newestIdx).toBeLessThan(olderIdx);
  // The rerun boundary is witnessed in the rail, in glossary language.
  await expect(page.getByText(/build reran by a\.kim — new version/)).toBeVisible();

  // ── (d) the gate step surfaces its pending-approval affordance ───────────
  await expect(page.getByRole("button", { name: /^approve/ })).toContainText("gate · manual");

  // ── (e) the failed→retried step's tries, with per-try outcomes ───────────
  await page.getByRole("button", { name: /^build/ }).click();
  await expect(page).toHaveURL(/[?&]step=1(&|$)/);
  // The step header's try control names the selected try, and the header
  // says what it did.
  const triesBtn = page.getByRole("button", { name: /^Try 2 of 2/ });
  await expect(triesBtn).toBeVisible();
  await expect(page.locator(".sp-status")).toContainText("✓ Passed in");
  // It passed after an earlier failure in this version.
  await expect(page.locator(".sp-box")).toContainText("Passed on try 2 (automatic retry)");
  await expect(page.getByRole("button", { name: /^Logs try 2/ })).toBeVisible();
  await triesBtn.click();
  const try1 = page.getByRole("menuitemradio", { name: /Try 1/ });
  const try2 = page.getByRole("menuitemradio", { name: /Try 2/ });
  await expect(try1).toContainText("rerun by a.kim"); // the rerun target's fork try
  await expect(try1).toContainText("Failed: step error");
  await expect(try2).toContainText("automatic retry"); // the engine's in-version retry
  await expect(try2).toContainText("Passed");
  // The glossary ban again, WITH the tries menu in the DOM.
  await expectGlossaryClean(page);
  // Picking the older try scopes the pane to it, keeps it in the URL, and
  // disables Debug pod (it reproduces the latest try).
  await try1.click();
  await expect(page.getByRole("button", { name: /^Try 1 of 2/ })).toBeVisible();
  await expect(page.locator(".sp-box")).toContainText("Try 1 failed: step error");
  await expect(page.locator(".sp-box")).toContainText("Not the latest try: try 2 passed");
  await expect(page.getByRole("button", { name: /Debug pod/ })).toBeDisabled();
  await expect(page).toHaveURL(/[?&]try=1(&|$)/);
  // The same URL opens on the same step and try.
  await page.reload();
  await expect(page.getByRole("button", { name: /^Try 1 of 2/ })).toBeVisible();
  // An index past the end is dropped, not an error: the default applies.
  await page.goto(`/acme/scarab/runs/${RICH_RUN_ID}?step=1&try=9`);
  await expect(page.getByRole("button", { name: /^Try 2 of 2/ })).toBeVisible();

  // ── (f) artifacts section lists the artifact of record ───────────────────
  // The step header counts build's files.
  await expect(page.locator(".sp-files")).toHaveText("1 artifact");
  await expect(page.getByText("Artifacts", { exact: true })).toBeVisible();
  await expect(page.getByRole("link", { name: "scarab-dist.tar.gz" })).toBeVisible();
  // The pill names the try the way the tries menu counts it: build's a3 is
  // its second try in the latest version.
  await expect(page.getByText("build · try 2", { exact: true })).toBeVisible();
  await expect(page.getByText("of record", { exact: true })).toBeVisible();
});

// The ENVIRONMENT GATE run in src/mock.ts: a deploy into production with a
// plain QA gate and the approval gate production added.
const ENV_GATE_RUN_ID = "0190f8a2000071fb8c00e7a9b0c1d2e3";

test("an environment approval gate says so, names who added it, and its wait timer", async ({ page }) => {
  await page.goto(`/acme/scarab/runs/${ENV_GATE_RUN_ID}`);
  await expect(page.getByRole("heading", { name: /#152/ })).toBeVisible();

  // The DAG: the added gate is an approval gate, added by production.
  const added = page.getByRole("button", { name: /^environment-approval/ });
  await expect(added).toContainText("gate · approval");
  await expect(added).toContainText("added by Production");
  await expect(page.getByRole("button", { name: /^qa/ })).toContainText("gate · manual");

  // The gate cards: the approval gate names the environment; the plain one
  // says nothing of its approvers.
  const bar = page.locator(".gate-bar");
  const approvalRow = bar.locator(".gate-row", { hasText: "environment-approval" });
  const qaRow = bar.locator(".gate-row", { hasText: /^\s*qa/ });
  await expect(approvalRow).toContainText("Environment approval gate · added by Production (production)");
  await expect(approvalRow).toContainText("waiting for the environment's approvers");
  await expect(qaRow).toContainText("waiting for a decision");
  await expect(qaRow).not.toContainText(/approver|Environment/);

  // Approving records the approval and says when the wait timer releases it.
  await approvalRow.getByRole("button", { name: "Approve" }).click();
  await expect(approvalRow).toContainText(/approval recorded · the gate releases at .+, when the wait timer runs out/);
  await expect(approvalRow).not.toContainText(/fail/);
  await expect(approvalRow.locator(".gate-clock")).toContainText(/releases at .+ · in \d+:\d\d/);
  await expect(approvalRow).toContainText("approvals in · waiting for the wait timer");

  // The step pane names the gate too.
  await added.click();
  await expect(page.locator(".gate-env-line")).toContainText("Environment approval gate · added by Production (production)");
});

// A Run a Policy refused a Step of (src/mock-policies.ts).
const POLICY_RUN_ID = "0190f8a2000071fb8c00b01c7e0a5e11";

test("the Run page lists the Run's policy decisions with the input each rule saw", async ({ page }) => {
  await page.goto(`/acme/orders-api/runs/${POLICY_RUN_ID}`);
  const decisions = page.getByRole("list", { name: "Policy decisions" });
  // The strip opens by itself when a decision denied.
  await expect(decisions).toBeVisible();
  await expect(decisions.getByRole("listitem").filter({ hasText: "run.create" })).toContainText(
    "would deny (audit)",
  );
  const launch = decisions.locator(":scope > li").filter({ hasText: "deploy · try 1" });
  await expect(launch).toContainText("denied");
  await expect(launch).toContainText("no-latest-images");
  await expect(launch).toContainText("Pin the image by tag or digest, not :latest.");
  await launch.getByText("Input the rules saw").click();
  await expect(launch).toContainText("ghcr.io/acme/deployer:latest");
});

test("a Run no Policy applied to shows no Policies strip", async ({ page }) => {
  await page.goto(`/acme/scarab/runs/${RICH_RUN_ID}`);
  await expect(page.getByText("Wire shared postgres into the integration suite")).toBeVisible();
  await expect(page.getByRole("list", { name: "Policy decisions" })).toHaveCount(0);
});

// The MATRIX run in src/mock.ts: a plan over cluster × phase where two cells
// failed, one of them on both of its tries.
const MATRIX_RUN_ID = "0190f8a2000071fb8c00f00d4a7b1c33";

test("a failed matrix run names where it failed and opens on the first failure", async ({ page }) => {
  await page.goto(`/acme/scarab/runs/${MATRIX_RUN_ID}`);
  await expect(page.getByRole("heading", { name: /#33/ })).toBeVisible();

  // The summary line names the root failures, each a link to its step.
  const summary = page.getByLabel("Run summary");
  await expect(summary).toContainText("✕ 2 of 7 steps failed:");
  await expect(summary.getByRole("button", { name: "plan · treebo-production · aws-post-cluster" })).toBeVisible();

  // The graph heads the matrix with its step and keys; each cell shows only
  // its values, and the cell that failed twice says so.
  await expect(page.getByText("plan · matrix · cluster · phase", { exact: false }).first()).toBeVisible();
  const twice = page.getByRole("button", { name: /^treebo-production · aws-cluster/ });
  await expect(twice).toContainText("failed 2 of 2 tries");

  // The page opens on the first root failure, with its failure box: the
  // failure class, the executor's detail, and the wait for a node.
  await expect(page.locator(".sp-name")).toHaveText("plan · treebo-production · aws-cluster");
  await expect(page.locator(".sp-status")).toContainText("✕ Failed after 2m06s: step error");
  const box = page.locator(".sp-box");
  await expect(box).toContainText("Try 2 failed: step error");
  await expect(box).toContainText("Error acquiring the state lock");
  await expect(box).toContainText("unschedulable for about 1m before it started");
  await expect(page.getByRole("button", { name: /^Retry step/ })).toBeVisible();
  await expect(page.getByRole("button", { name: /^Rerun from here/ })).toBeVisible();

  // A summary link selects that step.
  await summary.getByRole("button", { name: "plan · treebo-production · aws-post-cluster" }).click();
  await expect(page.locator(".sp-name")).toHaveText("plan · treebo-production · aws-post-cluster");
  await expect(page).toHaveURL(/[?&]step=4(&|$)/);

  // The List view: dependency order, a Failed filter.
  await page.getByRole("button", { name: /^List/ }).click();
  await page.getByRole("button", { name: /^Failed 2/ }).click();
  await expect(page.locator(".slist-row")).toHaveCount(2);
  await page.locator(".slist-row", { hasText: "treebo-production · aws-cluster" }).click();
  await expect(page.locator(".sp-name")).toHaveText("plan · treebo-production · aws-cluster");
});

test("the run header's ⋯ menu copies the run ID and keeps snapshots", async ({ page, context }) => {
  await context.grantPermissions(["clipboard-read", "clipboard-write"]);
  await page.goto(`/acme/scarab/runs/${MATRIX_RUN_ID}`);
  const more = page.getByRole("button", { name: "More run actions" });
  await more.focus();
  await page.keyboard.press("Enter");
  const menu = page.getByRole("menu", { name: "More run actions" });
  await expect(menu.getByRole("menuitem", { name: /Keep workspace snapshots/ })).toBeVisible();
  await menu.getByRole("menuitem", { name: /Copy run ID/ }).click();
  await expect(menu.getByRole("menuitem", { name: /Copied/ })).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(menu).toHaveCount(0);
});
