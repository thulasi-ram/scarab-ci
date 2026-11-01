// Settings shell (git-bug 734ffa1), in mock mode: every section in the sidebar
// opens at its own URL under its own heading, the old addresses redirect, and
// nothing errors on the way.
import { test, expect, type Page } from "@playwright/test";

function collectErrors(page: Page): string[] {
  const errors: string[] = [];
  page.on("pageerror", (err) => errors.push(`pageerror: ${err.message}`));
  page.on("console", (msg) => {
    if (msg.type() === "error") errors.push(`console.error: ${msg.text()}`);
  });
  return errors;
}

test("an admin lands on their org's settings and every section opens at its own URL", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/settings");
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/connections$/);

  const nav = page.getByRole("navigation", { name: "Settings" });
  const links = nav.getByRole("link");
  const count = await links.count();
  // Your account (2) + Organization settings (8) for the fixture's admin.
  expect(count).toBe(10);

  const seen = new Set<string>();
  for (let i = 0; i < count; i++) {
    const link = links.nth(i);
    const label = (await link.textContent())!.trim();
    const href = (await link.getAttribute("href"))!;
    await link.click();
    await expect(page).toHaveURL(new RegExp(`${href.replace(/[/]/g, "\\/")}$`));
    await expect(page.getByRole("heading", { level: 1, name: label, exact: true })).toBeVisible();
    await expect(link).toHaveAttribute("aria-current", "page");
    seen.add(href);
  }
  expect(seen.size).toBe(count);
  await expect(page.getByText(/There is no such settings section/)).toHaveCount(0);
  expect(errors).toEqual([]);
});

test("the old account addresses redirect and keep their query", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/account/security?enroll=1");
  await expect(page).toHaveURL(/\/settings\/security\?enroll=1$/);
  await expect(page.getByRole("heading", { level: 1, name: "Security", exact: true })).toBeVisible();

  await page.goto("/account?identity_linked=forgejo");
  await expect(page).toHaveURL(/\/settings\/profile\?identity_linked=forgejo$/);
  await expect(page.getByText(/Linked your Forgejo account/)).toBeVisible();
  expect(errors).toEqual([]);
});

test("an org the caller does not administer shows no org sections", async ({ page }) => {
  await page.goto("/orgs/globex/settings/members");
  await expect(page.getByText(/You do not administer/)).toBeVisible();
  const nav = page.getByRole("navigation", { name: "Settings" });
  await expect(nav.getByRole("link", { name: "Members" })).toHaveCount(0);
});
