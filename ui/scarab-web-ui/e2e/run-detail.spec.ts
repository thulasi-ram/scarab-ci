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

  // ── (b) the version ribbon: canonical rerun language, two versions ───────
  const versions = page.getByRole("group", { name: "Versions" });
  const latestDot = versions.getByRole("button", { name: /^v2 latest/ });
  const originalDot = versions.getByRole("button", { name: /^v1/ });
  await expect(latestDot).toHaveAttribute("aria-pressed", "true");
  await expect(latestDot).toHaveAttribute("title", /you reran from build/);
  await expect(latestDot).toHaveAttribute("title", /by a\.kim/);
  // The first version exists because of its trigger, and a step of it was
  // cut short by the rerun.
  await expect(originalDot).toHaveAttribute("title", /PR #131/);
  await expect(originalDot).toHaveAttribute("title", /superseded/);
  await expectGlossaryClean(page);
  // Picking version 1 frames the page as read-only, with the way back; the
  // graph's strip says what it did differently from the latest.
  await originalDot.click();
  await expect(page.locator(".vframe.older")).toBeVisible();
  await expect(page.locator(".vr-note")).toContainText("Version 1 of 2");
  await expect(page.locator(".dag-changes")).toContainText("in the latest (v2)");
  // The approval bar is hidden while version 1 is in view but keeps its
  // space, so nothing below it moves.
  await expect(page.locator(".gate-bar.reserved")).toBeHidden();
  const tabsTop = await page.locator(".run-tabs").evaluate((el) => el.getBoundingClientRect().top);
  await page.getByRole("button", { name: "Show latest" }).click();
  await expect(page.locator(".vframe.older")).toHaveCount(0);
  // The frame stays, faint, on the latest version, and so does the strip.
  await expect(page.locator(".vframe.framed")).toBeVisible();
  await expect(page.locator(".dag-changes")).toContainText("Latest version");
  await expect(page.locator(".gate-bar").getByRole("button", { name: "Approve" })).toBeVisible();
  expect(await page.locator(".run-tabs").evaluate((el) => el.getBoundingClientRect().top)).toBe(tabsTop);

  // ── (c) the Activity tab is newest-first ─────────────────────────────────
  await page.getByRole("tab", { name: /^Activity/ }).click();
  await expect(page).toHaveURL(/[?&]tab=activity(&|$)/);
  // The newest step event (test's pass on its one try of this version, merged
  // with its move to passed) renders ABOVE the older try events; both strings
  // exist only in the rail.
  const bodyText = await page.locator("body").innerText();
  const newestIdx = bodyText.search(/passed in \S+ on try 1/);
  const olderIdx = bodyText.indexOf("try 1 started");
  expect(newestIdx).toBeGreaterThan(-1);
  expect(olderIdx).toBeGreaterThan(-1);
  expect(newestIdx).toBeLessThan(olderIdx);
  // The rerun boundary is witnessed in the rail, in glossary language.
  await expect(page.getByText(/build reran by a\.kim — new version/)).toBeVisible();
  // The trigger reads as a sentence.
  await expect(page.getByText(/^Triggered by PR #131/)).toBeVisible();
  await page.getByRole("tab", { name: /^Pipeline/ }).click();

  // ── (d) the gate step surfaces its pending-approval affordance ───────────
  await expect(page.getByRole("button", { name: /^approve/ })).toContainText("gate · manual");

  // ── (e) the failed→retried step's tries, with per-try outcomes ───────────
  await page.getByRole("button", { name: /^build/ }).click();
  await expect(page).toHaveURL(/[?&]step=1(&|$)/);
  // With two tries the header carries a strip of numbered tries, the
  // selected one filled; the status line says what that try did and nothing
  // more for a passing try. No "try 2" on the content tabs.
  await expect(page.locator(".sp-status")).toContainText("✓ Passed in");
  await expect(page.locator(".sp-reason")).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Logs", exact: true })).toBeVisible();
  const tries = page.getByRole("tablist", { name: "Tries of this step" });
  const try1 = tries.getByRole("tab", { name: /Try 1 ·/ });
  const try2 = tries.getByRole("tab", { name: /Try 2 ·/ });
  await expect(try1).toContainText("rerun by a.kim"); // the rerun target's fork try
  await expect(try1).toContainText("Try 1 · failed: step error");
  await expect(try2).toContainText("automatic retry"); // the engine's in-version retry
  await expect(try2).toContainText("Try 2 · passed");
  await expect(try2).toContainText("of record");
  await expect(try2).toHaveAttribute("aria-selected", "true");
  // The glossary ban again, WITH the try tabs in the DOM.
  await expectGlossaryClean(page);
  // Picking the older try scopes the pane to it, keeps it in the URL, and
  // disables Debug pod (it reproduces the latest try).
  await try1.click();
  await expect(try1).toHaveAttribute("aria-selected", "true");
  await expect(page.locator(".sp-status")).toContainText("Failed");
  await expect(page.locator(".sp-line")).toContainText("Not the latest try: try 2 passed");
  await expect(page.getByRole("button", { name: /Debug pod/ })).toBeDisabled();
  await expect(page).toHaveURL(/[?&]try=1(&|$)/);
  // The same URL opens on the same step and try.
  await page.reload();
  await expect(try1).toHaveAttribute("aria-selected", "true");
  // An index past the end is dropped, not an error: the default applies.
  await page.goto(`/acme/scarab/runs/${RICH_RUN_ID}?step=1&try=9`);
  await expect(try2).toHaveAttribute("aria-selected", "true");

  // ── (f) the Artifacts tab lists the artifact of record ───────────────────
  // The step header counts build's files; the link opens the Artifacts tab
  // on build's files.
  await page.locator(".sp-head").getByRole("button", { name: "1 artifact", exact: true }).click();
  await expect(page.getByRole("tab", { name: /^Artifacts 1/ })).toHaveAttribute("aria-selected", "true");
  await expect(page).toHaveURL(/[?&]tab=artifacts(&|$)/);
  await expect(page.getByText("Showing artifacts from build")).toBeVisible();
  // The filter is in the URL by the step's index, and a reload keeps it.
  await expect(page).toHaveURL(/[?&]from=1(&|$)/);
  await page.reload();
  await expect(page.getByText("Showing artifacts from build")).toBeVisible();
  await expect(page.getByRole("link", { name: "scarab-dist.tar.gz" })).toBeVisible();
  // The row names the try the way the try tabs count it: build's a3 is its
  // second try in the latest version.
  const row = page.locator(".art-row", { hasText: "scarab-dist.tar.gz" });
  await expect(row.getByText("build · try 2", { exact: true })).toBeVisible();
  await expect(row.getByText("of record", { exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Show all" }).click();
  await expect(page.getByText("Showing artifacts from build")).toHaveCount(0);
  await expect(page).not.toHaveURL(/[?&]from=/);
  // Back on the Pipeline tab, the tab leaves the URL.
  await page.getByRole("tab", { name: /^Pipeline/ }).click();
  await expect(page).not.toHaveURL(/[?&]tab=/);
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

  // The page opens on the first root failure. One line under the step name
  // gives the failure class, the executor's detail and the wait for a node.
  await expect(page.locator(".sp-name")).toHaveText("plan · treebo-production · aws-cluster");
  const line = page.locator(".sp-line");
  await expect(line.locator(".sp-status")).toHaveText("✕ Failed after 2m06s: step error");
  await expect(line.locator(".sp-reason")).toContainText("Error acquiring the state lock");
  await expect(line).toContainText("waited about 1m for a node");
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

// The MATRIX run's treebo-staging × aws-cluster cell printed a large plan
// (5.5 MiB, 130,004 lines). The pane asks for the last 2 MiB and says so; it
// gets them from the start of the stored chunk that holds the cut.
test("a large log shows its last 2 MiB, Load full log reads it all, Download names the try", async ({ page }) => {
  await page.goto(`/acme/scarab/runs/${MATRIX_RUN_ID}?step=5`);
  await expect(page.locator(".sp-name")).toHaveText("plan · treebo-staging · aws-cluster");

  const notice = page.getByRole("status").filter({ hasText: "Showing the last" });
  await expect(notice).toHaveText(/Showing the last 2\.\d MiB of 5\.5 MiB( so far)?\./);
  const download = notice.getByRole("link", { name: "Download" });
  await expect(download).toHaveAttribute("href", /\/steps\/[^/]+\/logs\/raw\?attempt=a1$/);
  // The log tools offer the download for every try, cut or not.
  await expect(page.locator(".steplogs-tools").getByRole("link", { name: "Download" })).toBeVisible();
  // The file is the whole log.
  const href = await download.getAttribute("href");
  const raw = await page.evaluate(async (u) => (await fetch(u!)).text(), href);
  expect(raw.startsWith("$ tofu plan")).toBe(true);
  expect(raw.endsWith("Plan: 13000 to add, 0 to change, 0 to destroy.\n")).toBe(true);

  // The plan's last line, numbered from the first line the pane received.
  const last = page.locator(".lgrow", { hasText: "Plan: 13000 to add" });
  await expect(last).toBeVisible();
  await expect(last.locator(".lgln")).not.toHaveText("130004");

  // The whole log fits in 8 MiB, so the pane can read all of it.
  await notice.getByRole("button", { name: "Load full log" }).click();
  await expect(notice).toHaveCount(0);
  await expect(last.locator(".lgln")).toHaveText("130004");
});

// The rich run's test step: its log source restarted part way through, and
// the server's note about it is a row of its own between two output lines.
test("the server's note about a log is a row of its own, not an output line", async ({ page }) => {
  await page.goto(`/acme/scarab/runs/${RICH_RUN_ID}?step=2`);
  await expect(page.locator(".sp-name")).toHaveText("test");
  const note = page.locator(".lgbody").getByRole("note");
  await expect(note).toHaveCount(1);
  await expect(note).toHaveText(
    "[scarab] the log source restarted; lines before this point may be missing or repeat",
  );
  // It has no line number, and the lines around it keep theirs: the note did
  // not split or join them.
  await expect(note.locator(".lgln")).toHaveText("");
  const before = note.locator("xpath=preceding-sibling::div[1]");
  const after = note.locator("xpath=following-sibling::div[1]");
  await expect(before.locator(".lgtx")).toHaveText("running 214 tests");
  await expect(before.locator(".lgln")).toHaveText("7");
  await expect(after.locator(".lgtx")).toHaveText("test executor::retry::backoff_is_exponential ... ok");
  await expect(after.locator(".lgln")).toHaveText("8");
  // The raw log writes the note as a `[scarab]` line in the same place.
  const href = await page.locator(".steplogs-tools").getByRole("link", { name: "Download" }).getAttribute("href");
  const raw = await page.evaluate(async (u) => (await fetch(u!)).text(), href);
  expect(raw).toContain(
    "running 214 tests\n[scarab] the log source restarted; lines before this point may be missing or repeat\ntest executor",
  );
});

// The rich run's shared postgres service logged about 2.5 MiB (30,005 lines).
// Its pane shows the last 2 MiB, can read the whole log, and downloads it.
test("a large service log offers Load full log and Download", async ({ page }) => {
  await page.goto(`/acme/scarab/runs/${RICH_RUN_ID}`);
  await page.getByRole("button", { name: /postgres/ }).click();
  const pane = page.locator(".servicepane");
  const notice = pane.getByRole("status").filter({ hasText: "Showing the last" });
  await expect(notice).toHaveText(/Showing the last [\d.]+ MiB of 2\.\d MiB\./);
  const download = notice.getByRole("link", { name: "Download" });
  await expect(download).toHaveAttribute("href", /\/services\/postgres\/logs\/raw$/);
  // The readout offers the download whether the log is cut or not.
  await expect(pane.locator(".svc-readout").getByRole("link", { name: "Download" })).toBeVisible();
  // The file is the whole log.
  const href = await download.getAttribute("href");
  const raw = await page.evaluate(async (u) => (await fetch(u!)).text(), href);
  expect(raw.startsWith("PostgreSQL init process complete")).toBe(true);
  expect(raw.endsWith("LOG:  checkpoint complete: wrote 412 buffers\n")).toBe(true);
  const last = pane.locator(".lgrow", { hasText: "checkpoint complete" });
  await expect(last).toBeVisible();
  await expect(last.locator(".lgln")).not.toHaveText("30005");

  await notice.getByRole("button", { name: "Load full log" }).click();
  await expect(notice).toHaveCount(0);
  await expect(last.locator(".lgln")).toHaveText("30005");
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

// The RETRIES run in src/mock.ts: live, with two matrix cells waiting for a
// node, cleared Pod conditions on earlier tries, a step with three tries, one
// with six, and artifacts that earlier tries also wrote.
const RETRIES_RUN_ID = "0190f8a2000071fb8c00a11c3e5d7f34";

test("run tabs: the Activity filter hides cleared Pod conditions, Artifacts shows earlier copies", async ({
  page,
}) => {
  await page.goto(`/acme/scarab/runs/${RETRIES_RUN_ID}`);
  await expect(page.getByRole("heading", { name: /#34/ })).toBeVisible();
  // The summary names the cells waiting for a node; the run opens on one.
  await expect(page.getByLabel("Run summary")).toContainText("◷ 2 waiting for a node");
  await expect(page.locator(".sp-name")).toContainText("deploy-preview");

  const runTabs = page.getByRole("tablist", { name: "Run" });
  await expect(runTabs.getByRole("tab", { name: /^Pipeline 5 steps/ })).toHaveAttribute("aria-selected", "true");
  // A live run's Activity tab carries a dot while a condition is active, not
  // a count.
  await expect(runTabs.getByRole("tab", { name: /^Activity/ }).locator(".run-tab-dot")).toBeVisible();

  // ── Activity: Run and steps by default ───────────────────────────────────
  await runTabs.getByRole("tab", { name: /^Activity/ }).click();
  const filter = page.getByRole("group", { name: "Activity filter" });
  await expect(filter.getByRole("button", { name: /^Run and steps/ })).toHaveAttribute("aria-pressed", "true");
  // The active conditions show, highlighted; the cleared ones are hidden and
  // counted (three tries waited, each an onset and a close).
  await expect(page.locator(".tl-active")).toHaveCount(2);
  await expect(page.locator(".tl-cleared")).toHaveCount(0);
  await expect(page.getByText("6 cleared Pod condition events hidden")).toBeVisible();
  await expect(page.getByText(/^Triggered by PR #162/)).toBeVisible();
  // Show switches to the Infrastructure filter, where they appear.
  await page.locator(".rt-hidden").getByRole("button", { name: "Show" }).click();
  await expect(filter.getByRole("button", { name: /^Infrastructure 8/ })).toHaveAttribute("aria-pressed", "true");
  await expect(page.locator(".tl-cleared")).toHaveCount(6);
  await filter.getByRole("button", { name: /^Everything/ }).click();
  await expect(page.getByText("cleared Pod condition events hidden")).toHaveCount(0);

  // The step pane stayed mounted under the other tab; Pipeline shows it again.
  await runTabs.getByRole("tab", { name: /^Pipeline/ }).click();
  await expect(page.locator(".sp-name")).toContainText("deploy-preview");

  // ── Artifacts: of record by default, earlier copies on request ───────────
  await page.getByLabel("Run summary").getByRole("button", { name: "4 artifacts" }).click();
  await expect(runTabs.getByRole("tab", { name: /^Artifacts 4/ })).toHaveAttribute("aria-selected", "true");
  const copies = page.getByRole("group", { name: "Artifact copies" });
  await expect(copies.getByRole("button", { name: /^Of record 4/ })).toHaveAttribute("aria-pressed", "true");
  await expect(page.locator(".art-row")).toHaveCount(4);
  // The trace only a failed try wrote still shows, marked as such.
  await expect(page.locator(".art-row", { hasText: "trace.zip" })).toContainText("from a failed try");
  await expect(page.locator(".art-row", { hasText: "junit.xml" })).toContainText("unit · try 3");
  await copies.getByRole("button", { name: /^Include earlier copies 7/ }).click();
  await expect(page.locator(".art-row")).toHaveCount(7);
  await expect(page.locator(".art-row", { hasText: "e2e · try 1" })).toContainText("from a failed try");
});

test("try strip: every try oldest first in the header, and tries nested in the List view", async ({ page }) => {
  // e2e is step 2: six tries, the sixth passed.
  await page.goto(`/acme/scarab/runs/${RETRIES_RUN_ID}?step=2`);
  const tries = page.getByRole("tablist", { name: "Tries of this step" });
  // Every try is a numbered button, oldest first; the newest is selected.
  await expect(tries.getByRole("tab")).toHaveCount(6);
  await expect(tries.getByRole("tab").first()).toContainText("Try 1 · failed: step error");
  await expect(tries.getByRole("tab").last()).toContainText("Try 6 · passed");
  await expect(tries.getByRole("tab").last()).toContainText("of record");
  await expect(tries.getByRole("tab").last()).toHaveAttribute("aria-selected", "true");
  await tries.getByRole("tab", { name: /Try 1 ·/ }).click();
  await expect(tries.getByRole("tab", { name: /Try 1 ·/ })).toHaveAttribute("aria-selected", "true");
  await expect(page).toHaveURL(/[?&]try=1(&|$)/);
  await expect(page.locator(".sp-line")).toContainText("Not the latest try: try 6 passed");

  // unit (step 1): three tries; the third was a retry by a.kim.
  await page.goto(`/acme/scarab/runs/${RETRIES_RUN_ID}?step=1`);
  const unitTries = page.getByRole("tablist", { name: "Tries of this step" });
  await expect(unitTries.getByRole("tab")).toHaveCount(3);
  await expect(unitTries.getByRole("tab").last()).toContainText("retried by a.kim");

  // The List view nests each step's tries; a try row opens that try.
  await page.getByRole("button", { name: /^List/ }).click();
  const nested = page.locator(".slist-try", { hasText: "Try 2 · failed: timed out" }).first();
  await nested.click();
  await expect(unitTries.getByRole("tab", { name: /Try 2 ·/ })).toHaveAttribute("aria-selected", "true");
  await expect(page).toHaveURL(/[?&]try=2(&|$)/);

  // A narrow screen keeps the same strip.
  await page.setViewportSize({ width: 600, height: 900 });
  await expect(page.getByRole("tablist", { name: "Tries of this step" }).getByRole("tab")).toHaveCount(3);
});

// A step with one try has no try control: there is nothing to pick.
test("a step with one try shows no try strip", async ({ page }) => {
  await page.goto(`/acme/scarab/runs/${RETRIES_RUN_ID}?step=0`);
  await expect(page.locator(".sp-status")).toContainText("Passed");
  await expect(page.getByRole("tablist", { name: "Tries of this step" })).toHaveCount(0);
});

// The page polls a live run every 1.2s and its clock ticks every second.
// Regression: each poll re-created the List rows and the try tabs (focus
// lost), and closed the Workspace tab's open file.
test("a live run's polls keep focus, the open file and the step tab", async ({ page }) => {
  await page.goto(`/acme/scarab/runs/${RETRIES_RUN_ID}?step=1`);
  const tries = page.getByRole("tablist", { name: "Tries of this step" });
  const try2 = tries.getByRole("tab", { name: /Try 2 ·/ });
  await try2.focus();
  await page.waitForTimeout(2500);
  await expect(try2).toBeFocused();

  await page.getByRole("button", { name: /^List/ }).click();
  const row = page.locator(".slist-row", { hasText: "unit" });
  await row.focus();
  await page.waitForTimeout(2500);
  await expect(row).toBeFocused();

  await page.getByRole("button", { name: "Workspace", exact: true }).click();
  await expect(page).toHaveURL(/[?&]ptab=workspace(&|$)/);
  await page.getByRole("button", { name: /summary\.txt/ }).click();
  await expect(page.locator(".fileview")).toContainText("12 passed, 0 failed");
  await page.waitForTimeout(2600);
  await expect(page.locator(".fileview")).toContainText("12 passed, 0 failed");
  await expect(page).toHaveURL(/[?&]ptab=workspace(&|$)/);
});

// Make the `/events` reads fail: the first `times` of them, or every one
// when `times` is null. The mock replaces window.fetch when the app boots, so
// wrap whatever it installs.
async function failEventReads(page: Page, times: number | null) {
  await page.addInitScript((limit) => {
    const orig = window.fetch;
    let inner: typeof fetch = orig;
    let failed = 0;
    const wrapper = ((input: RequestInfo | URL, init?: RequestInit) => {
      const url = typeof input === "string" ? input : input instanceof URL ? input.href : input.url;
      const events = /\/v1\/runs\/[^/]+\/events$/.test(new URL(url, location.origin).pathname);
      if (events && (limit === null || failed < limit)) {
        failed += 1;
        return Promise.resolve(new Response("unavailable", { status: 503 }));
      }
      return inner(input, init);
    }) as typeof fetch;
    Object.defineProperty(window, "fetch", {
      configurable: true,
      get: () => (inner === orig ? orig : wrapper),
      set: (v: typeof fetch) => {
        inner = v;
      },
    });
  }, times);
}

// Slow or break the mock's step log streams (see `__scarabMockLogs` in
// src/mock.ts) before the app boots.
async function mockStepLogs(page: Page, knobs: { delayMs?: number; failStreams?: number }) {
  await page.addInitScript((k) => {
    (window as unknown as { __scarabMockLogs: object }).__scarabMockLogs = { match: "/steps/", ...k };
  }, knobs);
}

// Regression: while a finished step's log was on its way, the pane said "No
// log output was captured for this try."
test("a finished step's log says it is loading until it arrives", async ({ page }) => {
  await mockStepLogs(page, { delayMs: 3000 });
  // Record whether the empty-log line ever shows, however briefly.
  await page.addInitScript(() => {
    const w = window as unknown as { sawNoOutput?: boolean };
    new MutationObserver(() => {
      if (document.body?.textContent?.includes("No log output was captured")) w.sawNoOutput = true;
    }).observe(document, { subtree: true, childList: true, characterData: true });
  });
  await page.goto(`/acme/scarab/runs/${RICH_RUN_ID}?step=1`);
  await expect(page.locator(".sp-name")).toHaveText("build");
  await expect(page.getByRole("status").filter({ hasText: "Loading the log…" })).toBeVisible();
  await expect(page.locator(".lgrow", { hasText: "cargo test --workspace" })).toBeVisible();
  await expect(page.getByText("Loading the log…")).toHaveCount(0);
  expect(await page.evaluate(() => (window as unknown as { sawNoOutput?: boolean }).sawNoOutput)).toBeFalsy();
});

test("a log stream that fails says why and Retry loads the log", async ({ page }) => {
  await mockStepLogs(page, { failStreams: 1 });
  await page.goto(`/acme/scarab/runs/${RICH_RUN_ID}?step=1`);
  await expect(page.locator(".sp-name")).toHaveText("build");
  const alert = page.getByRole("alert").filter({ hasText: "Could not load the log" });
  await expect(alert).toHaveText(/Could not load the log: the server answered 503\. Retry/);
  await expect(page.getByText("No log output was captured")).toHaveCount(0);
  await alert.getByRole("button", { name: "Retry" }).click();
  await expect(page.locator(".lgrow", { hasText: "cargo test --workspace" })).toBeVisible();
  await expect(alert).toHaveCount(0);
});

// Regression: when the first read of the events failed, the page applied the
// URL's selection to a run with no versions and dropped `v` and `try`.
test("a failed first read of the events keeps the URL's version and try", async ({ page }) => {
  await failEventReads(page, 1);
  await page.goto(`/acme/scarab/runs/${RICH_RUN_ID}?step=1&v=1&try=1`);
  await expect(page.locator(".vr-note")).toContainText("Version 1 of 2");
  // build has one try in version 1: the status says what it did, and there
  // is no try strip.
  await expect(page.locator(".sp-status")).toContainText("Passed");
  await expect(page.getByRole("tablist", { name: "Tries of this step" })).toHaveCount(0);
  await expect(page).toHaveURL(/[?&]v=1(&|$)/);
  await expect(page).toHaveURL(/[?&]try=1(&|$)/);
});

// Regression: when every read of the events failed, the page never selected
// a step and never wrote the URL. After a few failed reads it goes on
// without them: the step applies, and the version and try (which only the
// events can confirm) leave the URL.
test("events that never load still select the URL's step and write the URL", async ({ page }) => {
  await failEventReads(page, null);
  await page.goto(`/acme/scarab/runs/${RICH_RUN_ID}?step=1&v=1&try=1`);
  await expect(page.locator(".sp-name")).toHaveText("build");
  await expect(page).not.toHaveURL(/[?&]v=/);
  await expect(page).not.toHaveURL(/[?&]try=/);
  await expect(page).toHaveURL(/[?&]step=1(&|$)/);
});

test("Cancel run from the keyboard: the confirm opens on Keep running", async ({ page }) => {
  await page.goto(`/acme/scarab/runs/${RETRIES_RUN_ID}`);
  const trigger = page.locator(".rh-actions").getByRole("button", { name: "Cancel run" });
  await trigger.focus();
  await page.keyboard.press("Enter");
  const dialog = page.getByRole("dialog", { name: "confirm cancel run" });
  await expect(dialog.getByRole("button", { name: "Keep running" })).toBeFocused();
  // Escape closes it and gives focus back to the button.
  await page.keyboard.press("Escape");
  await expect(dialog).toHaveCount(0);
  await expect(trigger).toBeFocused();

  await page.keyboard.press("Enter");
  await expect(dialog.getByRole("button", { name: "Keep running" })).toBeFocused();
  await page.keyboard.press("Tab");
  await expect(dialog.getByRole("button", { name: "Cancel run" })).toBeFocused();
  await page.keyboard.press("Enter");
  await expect(dialog).toHaveCount(0);
  await expect(page.getByText("cancelled", { exact: true })).toBeVisible();
  await expect(page.locator(".rh-actions").getByRole("button", { name: /New run/ })).toBeVisible();
});
