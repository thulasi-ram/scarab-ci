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
  // Your account (3) + Organization settings (9) + Server administration (1)
  // for the fixture's admin, who is a global admin.
  expect(count).toBe(13);

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

  // It sits in the settings frame, with its section marked in the sidebar.
  const nav = page.getByRole("navigation", { name: "Settings" });
  await expect(nav).toBeVisible();
  await expect(nav.getByRole("link", { name: "Environments", exact: true })).toHaveAttribute(
    "aria-current",
    "page",
  );
  await expect(nav.locator('[aria-current="page"]')).toHaveCount(1);
  await expect(page.getByRole("heading", { level: 1 })).toHaveText(/Environments\s*production/);

  await view.getByRole("link", { name: "Edit", exact: true }).click();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/environments\/production\/edit$/);

  // The edit screen: every change the view left out, in the same frame.
  await expect(nav).toBeVisible();
  await expect(nav.getByRole("link", { name: "Environments", exact: true })).toHaveAttribute(
    "aria-current",
    "page",
  );
  await expect(nav.locator('[aria-current="page"]')).toHaveCount(1);
  await expect(page.getByRole("heading", { level: 1 })).toHaveText(/Environments\s*production\s*Edit/);
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

test("the library review sits in the settings frame, with Libraries marked", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/libraries/review");
  const nav = page.getByRole("navigation", { name: "Settings" });
  await expect(nav.getByRole("link", { name: "Libraries", exact: true })).toHaveAttribute(
    "aria-current",
    "page",
  );
  await expect(page.locator("section.page section.page")).toHaveCount(0);
  expect(errors).toEqual([]);
});

test("switching org on a section shows the other org's settings, not the first's", async ({ page }) => {
  const errors = collectErrors(page);
  // The mock makes the caller an admin of both orgs only when asked.
  await page.goto("/orgs/acme/settings/secrets?admin_orgs=acme,globex");
  await expect(page.getByText("Org secrets · acme", { exact: true })).toBeVisible();

  await page.getByRole("combobox", { name: "Organization" }).selectOption("globex");
  await expect(page).toHaveURL(/\/orgs\/globex\/settings\/secrets$/);
  await expect(page.getByText("Org secrets · globex", { exact: true })).toBeVisible();
  await expect(page.getByText("Org secrets · acme", { exact: true })).toHaveCount(0);
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

test("Connections lists the org's connections and, for a global admin, the unassigned ones apart", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/connections");
  const page_ = page.locator("section.page");
  await expect(page_.getByText("forgejo-7f3a91c2", { exact: true })).toBeVisible();
  const unassigned = page_.locator(".panel", { hasText: "Unassigned connections" });
  await expect(unassigned.getByText("github-install-90211", { exact: true })).toBeVisible();

  // Add repository offers only repositories owned by the org.
  await page_.getByRole("button", { name: "Add repository" }).click();
  await expect(page.getByText("acme/terraform-modules")).toBeVisible();
  await expect(page.getByText("globex/shared-ci")).toHaveCount(0);
  await page.locator(".modal-scrim").click({ position: { x: 5, y: 5 } });

  // Removing a connection asks for its id typed exactly.
  const row = page_.locator("li.conn", { hasText: "forgejo-7f3a91c2" });
  await row.getByRole("button", { name: "Remove", exact: true }).first().click();
  const go = row.getByRole("button", { name: "Remove connection" });
  await expect(go).toBeDisabled();
  await row.getByRole("textbox", { name: "Type forgejo-7f3a91c2 to confirm" }).fill("forgejo-7f3a91c2");
  await expect(go).toBeEnabled();
  expect(errors).toEqual([]);
});

test("a global admin connects a forge for a new org and lands on its connections", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/settings/server/connect-forge");
  await expect(page.getByRole("heading", { level: 1, name: "Connect a forge", exact: true })).toBeVisible();
  const add = page.getByRole("button", { name: "Add connection" });
  await expect(add).toBeDisabled();
  await page.locator("#conn-org").fill("globex");
  await page.locator("#conn-url").fill("https://git.globex.dev");
  await page.locator("#conn-token").fill("x".repeat(24));
  await add.click();
  await expect(page).toHaveURL(/\/orgs\/globex\/settings\/connections$/);
  // The page renders globex's connections, not the "You do not administer"
  // panel: the new org reached `admin_orgs` on the next `/v1/me`.
  const page_ = page.locator("section.page");
  await expect(page_.getByText("forgejo-5be0d4a1", { exact: true })).toBeVisible();
  await expect(page_.getByText("https://git.globex.dev", { exact: true })).toBeVisible();
  await expect(page_.getByText(/You do not administer/)).toHaveCount(0);
  expect(errors).toEqual([]);
});

test("a global admin assigns an unassigned connection to an org", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/connections");
  const page_ = page.locator("section.page");
  const unassigned = page_.locator(".panel", { hasText: "Unassigned connections" });
  const row = unassigned.locator("li.conn", { hasText: "github-install-90211" });
  // It binds nothing on a re-sync, so it offers none.
  await expect(row.getByRole("button", { name: /Re-sync/ })).toHaveCount(0);
  await row.getByRole("button", { name: "Assign to org" }).click();
  const assign = row.getByRole("button", { name: "Assign", exact: true });
  const field = row.getByRole("textbox", { name: "Organization to assign" });
  await field.fill("a/b");
  await expect(assign).toBeDisabled();
  await field.fill("umbrella");
  await expect(assign).toBeEnabled();
  await assign.click();
  // Assigned, it is no longer unassigned, so the panel goes.
  await expect(unassigned).toHaveCount(0);
  expect(errors).toEqual([]);
});
