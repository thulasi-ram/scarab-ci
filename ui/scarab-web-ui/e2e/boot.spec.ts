// Boot smoke: the app loads at `/` and renders real content, with ZERO page
// errors. This is the guard for the escape class the vitest tier can't see —
// a change that breaks the bundle at boot (e.g. top-level await under Vite's
// default target built fine but produced a blank page in the browser).
import { test, expect } from "@playwright/test";

test("app boots at / and renders the dashboard with no console errors", async ({ page }) => {
  const errors: string[] = [];
  page.on("pageerror", (err) => errors.push(`pageerror: ${err.message}`));
  page.on("console", (msg) => {
    if (msg.type() === "error") errors.push(`console.error: ${msg.text()}`);
  });

  await page.goto("/");

  // The dashboard heading — the app actually mounted.
  await expect(page.getByRole("heading", { name: "Dashboard" })).toBeVisible();

  // The action inbox rendered from the fixture's two suspended runs.
  await expect(page.getByText(/waiting on you/)).toBeVisible();

  // The repo list renders real fixture content (cards + rows both count —
  // `.first()` because a repo can appear in "most active" AND the full list).
  await expect(page.getByRole("link", { name: /orders-api/ }).first()).toBeVisible();
  await expect(page.getByRole("link", { name: /edge-gateway/ }).first()).toBeVisible();
  await expect(page.getByText(/repos · 7/)).toBeVisible();

  // No degraded-mode copy leaked through.
  await expect(page.getByText(/Could not load/)).toHaveCount(0);

  // The whole page rendered without a single page error / console error —
  // fails the test on the boot-breakage escape class.
  expect(errors).toEqual([]);
});
