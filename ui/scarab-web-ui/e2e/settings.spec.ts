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
  // Your account (3) + Organization settings (9) for the fixture's admin.
  expect(count).toBe(12);

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

test("an environment opens read-only and its Edit button opens the edit screen", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/environments");
  await page.getByRole("link", { name: "production", exact: true }).click();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/environments\/production$/);

  // The view: the record, and no way to change it from here.
  const view = page.locator("section.page");
  await expect(view.getByText("customer-facing, eu-west-1 + us-east-1")).toBeVisible();
  await expect(view.getByRole("textbox")).toHaveCount(0);
  await expect(view.getByRole("checkbox")).toHaveCount(0);
  await expect(view.getByRole("button", { name: /delete|withdraw|grant|save/i })).toHaveCount(0);

  await view.getByRole("link", { name: "Edit", exact: true }).click();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/environments\/production\/edit$/);

  // The edit screen: every change the view left out.
  const edit = page.locator("section.page");
  await expect(edit.getByRole("textbox", { name: "description", exact: true })).toHaveValue(
    "customer-facing, eu-west-1 + us-east-1",
  );
  await expect(edit.getByRole("textbox", { name: "approvers", exact: true })).toHaveValue("ops-lead");
  await expect(edit.getByRole("checkbox", { name: /every project in acme/ })).toBeChecked();
  // Nothing changed yet, so there is nothing to save.
  await expect(edit.getByRole("button", { name: "Save", exact: true })).toBeDisabled();
  await edit.getByRole("textbox", { name: "description", exact: true }).fill("customer-facing, eu-west-1");
  await expect(edit.getByRole("button", { name: "Save", exact: true })).toBeEnabled();

  // Delete asks for the exact name before it is allowed.
  await edit.getByRole("button", { name: "Delete environment" }).click();
  const confirm = edit.getByRole("textbox", { name: "Type production to confirm" });
  const go = edit.getByRole("button", { name: "Delete environment" });
  await expect(go).toBeDisabled();
  await confirm.fill("Production");
  await expect(go).toBeDisabled();
  await confirm.fill("production");
  await expect(go).toBeEnabled();

  // Cancel goes back to the view without saving.
  await edit.getByRole("link", { name: "Cancel" }).click();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/environments\/production$/);
  expect(errors).toEqual([]);
});

test("Appearance switches the palette and the mode, and the choice survives a reload", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/settings/appearance");
  await expect(page.getByRole("heading", { level: 1, name: "Appearance", exact: true })).toBeVisible();
  const html = page.locator("html");
  await expect(html).toHaveAttribute("data-palette", "emerald");

  await page.getByRole("radio", { name: /Mono/ }).click();
  await expect(html).toHaveAttribute("data-palette", "mono");
  await expect(page.getByRole("radio", { name: /Mono/ })).toBeChecked();

  await page.getByRole("radio", { name: /Dark/ }).click();
  await expect(html).toHaveAttribute("data-theme", "dark");

  await page.reload();
  await expect(html).toHaveAttribute("data-palette", "mono");
  await expect(html).toHaveAttribute("data-theme", "dark");

  await page.getByRole("radio", { name: /Emerald/ }).click();
  await expect(html).toHaveAttribute("data-palette", "emerald");
  expect(errors).toEqual([]);
});
