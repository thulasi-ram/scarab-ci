// Run-detail walkthrough over the RICH mock fixture (src/mock.ts): one run
// carrying every multi-take surface — a RunRerunRequested boundary (two
// versions, superseded/shadowed attempts), a shared service + docked sidecar
// (ADR-0058), a pending manual gate, and an artifact of record. All selectors
// are role/text-based (glossary copy, ARIA roles) — no CSS-class coupling, no
// pixel snapshots.
import { test, expect, type Page } from "@playwright/test";

// The rich run's id in src/mock.ts.
const RICH_RUN_ID = "0190f8a2000071fb8c0099887766ccdd";

// Glossary (ADR-0056 amendment): the words "Take" and "Attempt" NEVER surface
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
  // build took two tries in this version — the ×N badge.
  await expect(page.getByRole("button", { name: /^build/ })).toContainText("×2");
  // The shared-service lane: label + the postgres peer node with its lifecycle.
  await expect(page.getByText("services", { exact: true })).toBeVisible();
  const svcNode = page.getByRole("button", { name: /postgres/ });
  await expect(svcNode).toBeVisible();
  await expect(svcNode).toContainText("ready");
  // test's docked sidecar chip (a role=button chip INSIDE the step node).
  const testNode = page.getByRole("button", { name: /^test/ });
  await expect(testNode.getByRole("button", { name: /redis/ })).toBeVisible();

  // ── (b) version dropdown: canonical rerun language, two versions ─────────
  await page.getByRole("button", { name: /latest/ }).click();
  const versionMenu = page.getByRole("menu", { name: "run versions" });
  await expect(versionMenu).toBeVisible();
  const latestRow = versionMenu.getByRole("menuitemradio", { name: /you reran build/ });
  const originalRow = versionMenu.getByRole("menuitemradio", { name: /original run/ });
  await expect(latestRow).toBeVisible();
  await expect(latestRow).toContainText("by a.kim");
  await expect(originalRow).toBeVisible();
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
  // The coordinate stamp's tries dropdown names the active try.
  const triesBtn = page.getByRole("button", { name: /try 2/ });
  await expect(triesBtn).toBeVisible();
  await expect(triesBtn).toContainText("succeeded");
  await triesBtn.click();
  const try1 = page.getByRole("menuitemradio", { name: /try 1/ });
  const try2 = page.getByRole("menuitemradio", { name: /try 2/ });
  await expect(try1).toContainText("you reran"); // the rerun target's fork try
  await expect(try1).toContainText("✗ failed");
  await expect(try2).toContainText("auto-retry"); // the engine's in-version retry
  await expect(try2).toContainText("✓ succeeded");
  // The glossary ban again, WITH the tries menu in the DOM.
  await expectGlossaryClean(page);
  await page.keyboard.press("Escape");

  // ── (f) artifacts section lists the artifact of record ───────────────────
  await expect(page.getByText("Artifacts", { exact: true })).toBeVisible();
  await expect(page.getByRole("link", { name: "scarab-dist.tar.gz" })).toBeVisible();
  await expect(page.getByText("build@a3")).toBeVisible();
  await expect(page.getByText("of record", { exact: true })).toBeVisible();
});
