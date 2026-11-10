// Settings shell (git-bug 734ffa1), in mock mode: account and org settings
// are separate pages, every section in a page's sidebar opens at its own URL
// under its own heading, the user menu reaches each page, a global admin makes
// a new org from the org dropdown, the old addresses redirect, and nothing
// errors on the way.
import { test, expect, type Locator, type Page } from "@playwright/test";

function collectErrors(page: Page): string[] {
  const errors: string[] = [];
  page.on("pageerror", (err) => errors.push(`pageerror: ${err.message}`));
  page.on("console", (msg) => {
    if (msg.type() === "error") errors.push(`console.error: ${msg.text()}`);
  });
  return errors;
}

/** The options of an open dropdown (the app's one dropdown, not a native
 *  select): open `combobox` first. */
function listOptions(page: Page) {
  return page.getByRole("listbox").getByRole("option");
}

/** Open the dropdown `combobox` and pick the option named `name`. */
async function choose(page: Page, combobox: Locator, name: string) {
  await combobox.click();
  await listOptions(page).getByText(name, { exact: true }).click();
}

test("an admin lands on their org's settings", async ({ page }) => {
  await page.goto("/settings");
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/connections$/);
});

// Account settings and an org's settings are separate pages: each sidebar
// lists its own sections and none of the other's.
for (const [start, heading, count] of [
  ["/settings/profile", "Account settings", 4],
  ["/orgs/acme/settings/connections", "Organization settings", 9],
] as const) {
  test(`${heading} lists only its own sections, each at its own URL`, async ({ page }) => {
    const errors = collectErrors(page);
    await page.goto(start);
    const nav = page.getByRole("navigation", { name: "Settings" });
    await expect(nav.getByText(heading, { exact: true })).toBeVisible();
    const links = nav.getByRole("link");
    await expect(links).toHaveCount(count);

    const seen = new Set<string>();
    for (let i = 0; i < count; i++) {
      const link = links.nth(i);
      const label = (await link.textContent())!.trim();
      const href = (await link.getAttribute("href"))!;
      expect(href.startsWith(start.slice(0, start.lastIndexOf("/") + 1)), href).toBe(true);
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
}

test("the user menu links each settings page, the org one to the org in view", async ({ page }) => {
  const errors = collectErrors(page);
  // In an org's context (here its settings; a repository page counts too),
  // Organization settings opens that org.
  await page.goto("/orgs/globex/settings/members");
  await page.locator("button.avatar").click();
  const menu = page.getByRole("menu");
  await expect(menu.getByRole("menuitem", { name: "Account settings" })).toHaveAttribute(
    "href",
    "/settings/profile",
  );
  // Two settings pages, so two entries.
  await expect(menu.getByRole("menuitem", { name: /settings$/ })).toHaveCount(2);
  await menu.getByRole("menuitem", { name: "Organization settings" }).click();
  await expect(page).toHaveURL(/\/orgs\/globex\/settings\/connections$/);
  await expect(page.getByRole("combobox", { name: "Organization" })).toHaveText("globex");

  // Outside an org, it opens the first org the caller administers.
  await page.locator("button.avatar").click();
  await page.getByRole("menu").getByRole("menuitem", { name: "Account settings" }).click();
  await expect(page).toHaveURL(/\/settings\/profile$/);
  await page.locator("button.avatar").click();
  await expect(
    page.getByRole("menu").getByRole("menuitem", { name: "Organization settings" }),
  ).toHaveAttribute("href", "/orgs/acme/settings/connections");
  expect(errors).toEqual([]);
});

test("the org dropdown is there with one org, and on a narrow screen sits above the section list", async ({ page }) => {
  await page.goto("/orgs/acme/settings/members?admin_orgs=acme");
  const pick = page.getByRole("combobox", { name: "Organization" });
  await expect(pick).toBeVisible();
  // The mock caller is a global admin, so New organization… ends the list,
  // with the two connections that wait for an org.
  await pick.click();
  await expect(listOptions(page)).toHaveText(["acme", "New organization… (2 waiting)"]);
  await page.keyboard.press("Escape");
  await expect(page.getByRole("listbox")).toHaveCount(0);

  await page.setViewportSize({ width: 600, height: 900 });
  const section = page.getByRole("combobox", { name: "Settings section" });
  await expect(pick).toBeVisible();
  await expect(section).toBeVisible();
  const [a, b] = [await pick.boundingBox(), await section.boundingBox()];
  expect(a!.y).toBeLessThan(b!.y);
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

// The event log has no section of its own: it sits under the step-up
// settings in Security, and its old address keeps its filters.
test("the old Event log address opens Security with its filters, and the refusals preset filters the log", async ({
  page,
}) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/events?kind=passkey.enrolled");
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/security\?kind=passkey\.enrolled$/);
  await expect(page.getByRole("heading", { level: 1, name: "Security", exact: true })).toBeVisible();
  const nav = page.getByRole("navigation", { name: "Settings" });
  await expect(nav.getByRole("link", { name: "Security", exact: true })).toHaveAttribute("aria-current", "page");
  await expect(nav.getByRole("link", { name: "Event log" })).toHaveCount(0);
  // The step-up settings sit above the log.
  await expect(page.getByText(/Step-up is enforced/)).toBeVisible();
  const rows = page.locator(".sec-table tbody tr");
  await expect(rows).toHaveCount(1);
  await expect(rows.first()).toContainText("Passkey enrolled");

  // One click: the refusals of the last 24 hours, through the URL. The mock
  // holds two; its older refusals are four days old.
  await page.getByRole("button", { name: "Refusals, last 24h" }).click();
  await expect(page).toHaveURL(/[?&]kind=step_up\.refused(&|$)/);
  await expect(page).toHaveURL(/[?&]from=/);
  await expect(rows).toHaveCount(2);
  for (const r of await rows.all()) await expect(r).toContainText("Refused: step-up required");
  await expect(page.getByRole("button", { name: "Clear filters" })).toBeVisible();
  expect(errors).toEqual([]);
});

test("an owner resets a member's passkeys from a dialog: pick, type the name, see what was removed", async ({
  page,
}) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/security");
  const panel = page.locator(".panel", { hasText: "Security · step-up" });
  // No member list inline: one button in the header.
  await expect(panel.getByRole("radio")).toHaveCount(0);
  await expect(panel.getByRole("link", { name: "enroll a passkey" })).toHaveAttribute(
    "href",
    "/settings/security?enroll=1",
  );

  // An admin without a passkey opens the dialog with that admin picked.
  await panel.getByRole("list", { name: "Admins without a passkey" }).getByRole("button", { name: "j.lee" }).click();
  let dialog = page.getByRole("dialog");
  await expect(dialog.getByRole("radio", { name: /j\.lee/ })).toBeChecked();
  await expect(dialog.getByText("no passkey", { exact: true })).toHaveCount(1);
  await dialog.getByRole("button", { name: "Cancel" }).click();

  await panel.getByRole("button", { name: "Reset passkeys" }).click();
  dialog = page.getByRole("dialog");
  const next = dialog.getByRole("button", { name: "Continue" });
  await expect(next).toBeDisabled();
  await dialog.getByRole("radio", { name: /m\.diaz/ }).check();
  await next.click();

  // The typed confirm; Cancel goes back to the member list.
  dialog = page.getByRole("dialog");
  await expect(dialog).toContainText("Reset every passkey m.diaz holds?");
  await dialog.getByRole("button", { name: "Cancel" }).click();
  await expect(page.getByRole("dialog").getByRole("radio", { name: /m\.diaz/ })).toBeChecked();
  await page.getByRole("dialog").getByRole("button", { name: "Continue" }).click();
  dialog = page.getByRole("dialog");
  const go = dialog.getByRole("button", { name: "Reset passkeys" });
  await expect(go).toBeDisabled();
  await dialog.getByRole("textbox", { name: "Type m.diaz to confirm" }).fill("m.diaz");
  await go.click();
  await expect(page.getByRole("dialog")).toHaveCount(0);
  await expect(panel.locator(".notice-ok")).toHaveText("Reset m.diaz: 2 passkeys removed, 1 confirmation revoked.");
  // An admin left with no passkey joins the list.
  await expect(panel.getByRole("list", { name: "Admins without a passkey" })).toContainText("m.diaz");
  expect(errors).toEqual([]);
});

test("an org the caller does not administer shows no org sections", async ({ page }) => {
  await page.goto("/orgs/initech/settings/members");
  await expect(page.getByText(/You do not administer/)).toBeVisible();
  const nav = page.getByRole("navigation", { name: "Settings" });
  await expect(nav.getByRole("link", { name: "Members" })).toHaveCount(0);
});

test("an environment is managed on one page, and its old addresses redirect there", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/environments");
  // The list shows the display name, with the name pipelines use beside it,
  // and New environment sits in the section header.
  const list = page.getByRole("list", { name: "Environments" });
  await expect(page.locator(".oenv-name", { hasText: "production" })).toBeVisible();
  await expect(page.locator(".panel-h").getByRole("button", { name: "New environment" })).toBeVisible();
  await list.getByRole("listitem").filter({ hasText: "Production" }).getByRole("link", { name: "Manage" }).click();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/environments\/production\/manage$/);

  // It sits in the settings frame, with its section marked in the sidebar.
  const nav = page.getByRole("navigation", { name: "Settings" });
  await expect(nav.getByRole("link", { name: "Environments", exact: true })).toHaveAttribute(
    "aria-current",
    "page",
  );
  await expect(nav.locator('[aria-current="page"]')).toHaveCount(1);
  await expect(page.getByRole("heading", { level: 1 })).toHaveText(/Environments\s*Production$/);
  await expect(page.locator(".page-sub")).toContainText("production");

  // The details are the form, filled with what is stored.
  const manage = page.locator("section.page");
  await expect(manage.getByRole("textbox", { name: "description", exact: true })).toHaveValue(
    "customer-facing, eu-west-1 + us-east-1",
  );
  await expect(manage.getByRole("textbox", { name: "Display name", exact: true })).toHaveValue("Production");
  await expect(manage.getByRole("textbox", { name: "Approvers", exact: true })).toHaveValue("ops-lead");
  await expect(manage.getByRole("checkbox", { name: /every project in acme/i })).toBeChecked();
  await expect(manage.getByRole("heading", { name: /Projects that may use Production/ })).toBeVisible();
  await expect(manage.getByRole("list", { name: "Project overlays and grants" })).toBeVisible();
  await expect(manage.getByRole("button", { name: "Duplicate" })).toBeVisible();
  // Nothing changed yet, so there is nothing to save or discard.
  const save = manage.getByRole("button", { name: "Save", exact: true });
  const discard = manage.getByRole("button", { name: "Discard changes" });
  await expect(save).toBeDisabled();
  await expect(discard).toBeDisabled();
  await manage.getByRole("textbox", { name: "description", exact: true }).fill("customer-facing, eu-west-1");
  await expect(save).toBeEnabled();
  await discard.click();
  await expect(manage.getByRole("textbox", { name: "description", exact: true })).toHaveValue(
    "customer-facing, eu-west-1 + us-east-1",
  );
  await expect(save).toBeDisabled();

  // Delete asks, in a dialog, for the exact name before it is allowed.
  await manage.getByRole("button", { name: "Delete environment" }).click();
  const dialog = page.getByRole("dialog");
  const confirm = dialog.getByRole("textbox", { name: "Type production to confirm" });
  const go = dialog.getByRole("button", { name: "Delete environment" });
  await expect(go).toBeDisabled();
  await confirm.fill("Production");
  await expect(go).toBeDisabled();
  await confirm.fill("production");
  await expect(go).toBeEnabled();
  await page.keyboard.press("Escape");
  await expect(dialog).toHaveCount(0);

  // The former read-only page and edit screen land here, query kept.
  await page.goto("/orgs/acme/settings/environments/production?from=link");
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/environments\/production\/manage\?from=link$/);
  await page.goto("/orgs/acme/settings/environments/production/edit");
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/environments\/production\/manage$/);
  await expect(page.getByRole("heading", { level: 1 })).toHaveText(/Environments\s*Production$/);
  expect(errors).toEqual([]);
});

test("New environment opens its dialog from the section header and lands on the new one's page", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/environments");
  await page.getByRole("button", { name: "New environment" }).click();
  const dialog = page.getByRole("dialog");
  await expect(dialog.getByRole("heading", { name: "New environment" })).toBeVisible();
  await dialog.getByRole("textbox", { name: "Name", exact: true }).fill("canary");
  await dialog.getByRole("button", { name: "Create environment" }).click();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/environments\/canary\/manage$/);
  await expect(page.getByRole("heading", { level: 1 })).toHaveText(/Environments\s*canary$/);
  expect(errors).toEqual([]);
});

test("an environment's approval gate settings save on its page", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/environments/production/manage");
  const manage = page.locator("section.page");
  const mode = manage.getByRole("combobox", { name: "When a pipeline has no approval gate" });
  await expect(mode).toHaveText("Add an approval gate");
  await expect(manage.getByRole("textbox", { name: "Added gate expiry" })).toHaveValue("86400");
  await expect(manage.getByRole("textbox", { name: "Maximum approval age" })).toHaveValue("86400");
  // One approver: a count above it cannot be saved.
  const required = manage.getByRole("textbox", { name: "Required approvals" });
  await required.fill("2");
  await expect(manage.getByRole("alert")).toContainText("With one approver, the only count is 1.");
  await expect(manage.getByRole("button", { name: "Save", exact: true })).toBeDisabled();
  await required.fill("1");
  await choose(page, mode, "Refuse the run");
  await manage.getByRole("button", { name: "Save", exact: true }).click();

  // It stays on the page, the form now holding what is stored.
  await expect(page.getByText("Production is saved.")).toBeVisible();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/environments\/production\/manage$/);
  await expect(mode).toHaveText("Refuse the run");
  await expect(required).toHaveValue("1");
  await expect(manage.getByRole("button", { name: "Save", exact: true })).toBeDisabled();
  expect(errors).toEqual([]);
});

test("a wait timer with no approvers is disabled and its environment warns", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/environments/staging/manage");
  const manage = page.locator("section.page");
  await expect(manage.getByText(/The 10 minutes wait timer does nothing until this environment has approvers/)).toBeVisible();
  const wait = manage.getByRole("textbox", { name: "Wait timer" });
  await expect(wait).toBeDisabled();
  await expect(manage.getByText(/Needs approvers: the timer waits on the approval gate/)).toBeVisible();
  await expect(manage.getByRole("textbox", { name: "Required approvals" })).toBeDisabled();
  // An approver turns both on.
  await manage.getByRole("textbox", { name: "Approvers", exact: true }).fill("ops-lead");
  await expect(wait).toBeEnabled();
  await expect(manage.getByRole("textbox", { name: "Required approvals" })).toBeEnabled();
  expect(errors).toEqual([]);
});

test("a repository's Environments tab is one table whose rows expand into the rules a deploy needs", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/acme/scarab?tab=environments");
  const table = page.locator("table.envt");
  const production = table.getByRole("row", { name: /Production/ });
  await expect(production).toContainText("+1 approver");
  await expect(production).toContainText("5 min");
  await expect(table.getByRole("row", { name: /qa/ })).toContainText("no rules");
  await expect(table.getByRole("row", { name: /Staging/ })).toContainText("not eligible");

  // Expanding: the sentence, the two layers side by side, and the actions.
  await expect(production).toHaveAttribute("aria-expanded", "false");
  // The name cell, not the row's centre: the row also holds the Runs link.
  await production.locator(".envt-name").click();
  await expect(production).toHaveAttribute("aria-expanded", "true");
  await expect(
    table.getByText("A deploy from main needs 2 approvals and a reason, then waits 5 minutes."),
  ).toBeVisible();
  const layers = table.locator("table.envt-layers");
  await expect(layers.getByRole("row", { name: /Approvers/ })).toContainText("ops-lead");
  await expect(layers.getByRole("row", { name: /Approvers/ })).toContainText("j.lee");
  await expect(layers.getByRole("row", { name: /Privileges/ })).toContainText("1 privileged image");
  await expect(table.getByRole("button", { name: "Edit this repo's rules" })).toBeVisible();

  // The keyboard folds it again.
  await production.press("Enter");
  await expect(production).toHaveAttribute("aria-expanded", "false");
  await expect(layers).toHaveCount(0);

  // A withdrawn environment says so, and keeps its overlay until removed.
  await table.getByRole("row", { name: /Staging/ }).locator(".envt-name").click();
  await expect(table.getByText("An org admin withdrew this repo. Its overlay stays until removed.")).toBeVisible();
  await expect(table.getByRole("button", { name: "Remove" })).toBeVisible();

  // The Runs link opens the Runs tab filtered by that environment.
  await production.getByRole("link", { name: "Runs in Production" }).click();
  await expect(page).toHaveURL(/\/acme\/scarab\?env=production$/);
  expect(errors).toEqual([]);
});

test("the Policies page lists CEL policies with their 7-day counts, and each opens its own page", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/policies");
  await expect(page.getByRole("heading", { level: 1, name: "Policies", exact: true })).toBeVisible();
  // Paste YAML and New policy sit in the section header.
  const header = page.locator(".panel-h", { hasText: "CEL policies" });
  await expect(header.getByRole("button", { name: "Paste YAML" })).toBeVisible();
  await expect(header.getByRole("link", { name: "New policy" })).toBeVisible();

  // CEL policies: live ones only, with what they apply to and their counts.
  const cel = page.getByRole("list", { name: "CEL policies" });
  await expect(cel.getByRole("listitem")).toHaveCount(3);
  const forks = cel.getByRole("listitem").filter({ hasText: "no-fork-deploys" });
  await expect(forks).toContainText("all projects");
  await expect(forks).toContainText("denied 1 in 7 days");
  const release = cel.getByRole("listitem").filter({ hasText: "release-from-main" });
  await expect(release).toContainText("audit");
  await expect(release).toContainText("orders-*, inventory-*, except inventory-legacy");
  // The decision 9 days old is outside the window.
  await expect(release).toContainText("would deny 2 in 7 days");
  await expect(release.getByRole("button", { name: "Enforce" })).toBeVisible();
  await expect(forks.getByRole("button", { name: "Enforce" })).toHaveCount(0);
  await expect(cel).not.toContainText("old-rule");

  // The org-wide decision log and the typed settings are not on this page:
  // each is on the page of what it is about.
  await expect(page.getByRole("list", { name: "Recent decisions" })).toHaveCount(0);
  await expect(page.getByText("Typed settings")).toHaveCount(0);
  await expect(cel).not.toContainText("approved-libraries");

  await forks.getByRole("link", { name: "Manage" }).click();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/policies\/pol-1$/);
  await expect(page.getByRole("heading", { level: 1 })).toHaveText(/Policies\s*no-fork-deploys$/);
  await expect(page.getByRole("navigation", { name: "Settings" }).locator('[aria-current="page"]')).toHaveText(
    "Policies",
  );
  expect(errors).toEqual([]);
});

test("a policy's Decisions tab lists its own outcome in each decision it matched", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/policies/pol-2");
  await page.getByRole("button", { name: "Decisions", exact: true }).click();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/policies\/pol-2\?tab=decisions$/);
  const list = page.getByRole("list", { name: "Decisions", exact: true });
  // release-from-main is in audit mode: it would have denied three times,
  // and two decisions it did not match are left out.
  await expect(list.getByRole("listitem")).toHaveCount(3);
  await expect(list.getByRole("listitem").first()).toContainText("would deny (audit)");
  await expect(list.getByRole("listitem").first()).toContainText("Deploy from main.");
  await expect(page.getByText(/2 decisions its match did not select are left out/)).toBeVisible();
  // Another policy's denial is not this one's.
  await expect(list).not.toContainText("A pull request from a fork may not deploy to production.");
  await list.getByRole("listitem").first().getByRole("link", { name: "run" }).click();
  await expect(page).toHaveURL(/\/acme\/orders-api\/runs\//);
  expect(errors).toEqual([]);
});

test("a library list's own page sends you to where it is edited", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/policies/pol-3");
  await expect(page.getByText("This is a list with a form of its own, edited under Libraries.")).toBeVisible();
  await expect(page.getByRole("textbox", { name: "Condition" })).toHaveCount(0);
  await page.locator("section.page").getByRole("link", { name: "Libraries", exact: true }).last().click();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/libraries#library-policies$/);
  await expect(page.getByRole("list", { name: "Library policies" })).toContainText("approved-libraries");
  expect(errors).toEqual([]);
});

test("enforcing an audit-mode policy from the list says what it would have denied", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/policies");
  const release = page.getByRole("list", { name: "CEL policies" }).getByRole("listitem").filter({ hasText: "release-from-main" });
  await release.getByRole("button", { name: "Enforce" }).click();
  const dialog = page.getByRole("dialog");
  await expect(dialog).toContainText("Enforce release-from-main?");
  await expect(dialog).toContainText("would have denied 2 times");
  await dialog.getByRole("button", { name: "Enforce", exact: true }).click();
  await expect(page.getByText("release-from-main is enforced.")).toBeVisible();
  await expect(release).toContainText("denied 0 in 7 days");
  await expect(release.getByRole("button", { name: "Enforce" })).toHaveCount(0);
  expect(errors).toEqual([]);
});

test("a new policy shows the checker's findings at its rule, completes field names, and tries the draft", async ({
  page,
}) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/policies");
  await page.getByRole("link", { name: "New policy" }).click();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/policies\/new$/);
  await expect(page.getByRole("combobox", { name: "Mode" })).toHaveText("audit");

  await page.getByRole("textbox", { name: "Name", exact: true }).fill("no-dev-deploys");
  const condition = page.getByRole("textbox", { name: "Condition" });
  await condition.fill("pipeline.nope");
  const problems = page.getByRole("list", { name: "Problems with rule 1" });
  await expect(problems).toContainText("line 1, column 1");
  await expect(problems).toContainText("has no field `nope`");

  // Field names complete from the checkpoint's input schema.
  await condition.fill("");
  await condition.pressSequentially("trigger.re");
  const names = page.getByRole("listbox", { name: "Field names" });
  await expect(names.getByRole("option")).toHaveCount(2);
  await expect(names.getByRole("option").first()).toContainText("ref");
  await condition.press("Tab");
  await expect(condition).toHaveValue("trigger.ref");

  // An optional field read without has() is a warning, not an error.
  await condition.pressSequentially(" == 'refs/heads/dev'");
  await expect(problems).toContainText("has(trigger.ref)");
  await condition.fill("has(trigger.ref) && trigger.ref == 'refs/heads/dev'");
  await expect(problems).toHaveCount(0);
  await expect(page.getByText("Reads trigger, trigger.ref.")).toBeVisible();
  await page.getByRole("textbox", { name: "Message" }).fill("No deploys from dev.");

  await page.getByRole("button", { name: "Try it on recent decisions" }).click();
  await expect(page.getByText(/recent run\.create decisions read/)).toBeVisible();
  const changed = page.getByRole("list", { name: "Decisions that would change" });
  await expect(changed.getByRole("listitem").first()).toContainText("would deny (audit)");

  await page.getByRole("button", { name: "Add policy" }).click();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/policies\/pol-\d+$/);
  await expect(page.getByText("no-dev-deploys is added in audit mode.")).toBeVisible();
  expect(errors).toEqual([]);
});

test("Paste YAML opens the policy in the editor, unsaved", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/policies");
  await page.getByRole("button", { name: "Paste YAML" }).click();
  const dialog = page.getByRole("dialog");
  await dialog
    .getByRole("textbox", { name: "Policy YAML" })
    .fill(
      [
        "kind: policy",
        "name: pasted-rule",
        "checkpoint: step.admit",
        "rules:",
        "  - when: step.deploy",
        "    effect: deny",
        '    message: "No deploy Steps."',
      ].join("\n"),
    );
  await dialog.getByRole("button", { name: "Open in editor" }).click();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/policies\/new$/);
  await expect(page.getByRole("textbox", { name: "Name", exact: true })).toHaveValue("pasted-rule");
  await expect(page.getByRole("combobox", { name: "Checkpoint" })).toHaveText("step.admit");
  await expect(page.getByRole("textbox", { name: "Condition" })).toHaveValue("step.deploy");
  expect(errors).toEqual([]);
});

test("a policy's history diffs each version against the one before", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/policies/pol-1?tab=history");
  const versions = page.getByRole("list", { name: "Versions" });
  await expect(versions.getByRole("listitem")).toHaveCount(3);
  const newest = versions.getByRole("listitem").first();
  await expect(newest).toContainText("version 3");
  await expect(newest).toContainText("no would-denies in a week of audit");
  await expect(newest).toContainText("Changes from version 2");
  await expect(newest).toContainText("- mode: audit");
  await expect(newest).toContainText("+ mode: enforce");
  await page.getByRole("button", { name: "Edit" }).click();
  await expect(page.getByRole("textbox", { name: "Condition" })).toHaveValue("trigger.fork && pipeline.deploy");
  await expect(page.getByText("This policy is enforced: saving a change asks for a passkey.")).toBeVisible();
  expect(errors).toEqual([]);
});

test("a library list is added under Library policies, in audit mode", async ({ page }) => {
  const errors = collectErrors(page);
  // A link with the panel's old anchor still lands on it.
  await page.goto("/orgs/acme/settings/libraries#library-lists");
  const panel = page.locator("#library-policies");
  await expect(panel.getByText("Library policies", { exact: true })).toBeInViewport();
  await panel.getByRole("button", { name: "Libraries blocked" }).click();
  const dialog = page.getByRole("dialog");
  await expect(dialog.getByRole("combobox", { name: "Mode" })).toHaveText("audit");
  await dialog.getByRole("textbox", { name: "Sources" }).fill("github.com/*/libs");
  await dialog.getByRole("button", { name: "Add list" }).click();
  await expect(dialog).toContainText("a prefix ending in *");
  await dialog.getByRole("textbox", { name: "Sources" }).fill("github.com/evil/*");
  await dialog.getByRole("button", { name: "Add list" }).click();
  await expect(page.getByText("libraries-blocked is added in audit mode.")).toBeVisible();
  await expect(page.getByRole("list", { name: "Library policies" })).toContainText("github.com/evil/*");
  expect(errors).toEqual([]);
});

test("duplicating an environment shows what was copied and what is left to do", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/environments/production/manage");
  await page.getByRole("button", { name: "Duplicate" }).click();
  const dialog = page.getByRole("dialog");
  const go = dialog.getByRole("button", { name: "Duplicate", exact: true });
  const name = dialog.getByRole("textbox", { name: "New name" });
  // A taken name cannot be sent.
  await name.fill("qa");
  await expect(dialog.getByText(/qa is already registered/)).toBeVisible();
  await expect(go).toBeDisabled();
  await name.fill("production-eu");
  await expect(dialog.getByRole("textbox", { name: "Display name" })).toHaveAttribute(
    "placeholder",
    "Production (copy)",
  );
  await go.click();

  // The result: the new environment, what was copied, then the checklist.
  await expect(dialog.getByRole("heading", { name: "Created Production (copy) · production-eu" })).toBeVisible();
  await expect(dialog.getByText("2 secrets with their values (scarab)")).toBeVisible();
  await expect(dialog.getByText("Change the pipelines.")).toBeVisible();
  await expect(dialog.getByText("Review the policies.")).toBeVisible();
  await expect(dialog.getByText("Update cloud trust policies.")).toBeVisible();
  await expect(dialog.getByText("scarab:org/acme/repo/*/env/production-eu/ref/*", { exact: false })).toBeVisible();

  await dialog.getByRole("button", { name: "Open Production (copy)" }).click();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/environments\/production-eu\/manage$/);
  await expect(page.getByRole("heading", { level: 1 })).toHaveText(/Environments\s*Production \(copy\)$/);
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

test("Install library opens a dialog, and its source is reviewed on its own page", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/libraries");
  const table = page.getByRole("table", { name: "Installed libraries" });
  await expect(table.getByRole("row")).toHaveCount(6);
  await expect(table.getByRole("row").filter({ hasText: "lib:platform/kaniko-build" })).toContainText("yanked");
  // A yank and a trusted source that no installation holds stay on the list.
  await expect(page.getByRole("list", { name: "Yanked versions no library holds now" })).toContainText("91f03ac2d7e5");
  await expect(page.getByRole("list", { name: "Trusted sources no library comes from now" })).toContainText(
    "release-notes.yaml",
  );

  await page.getByRole("button", { name: "Install library" }).click();
  const dialog = page.getByRole("dialog");
  const go = dialog.getByRole("button", { name: "Fetch and review" });
  await expect(go).toBeDisabled();
  await dialog
    .getByRole("textbox", { name: "Source" })
    .fill("github.com/acme/scarab-libs//deploy-helm.yaml@d41f07b29c3e8a6f5b1d0c9e7a2f4b8c6d3e1a90");
  await go.click();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/libraries\/review\?source=/);
  // The mock answers every fetch with an upgrade of deploy-helm.
  await expect(page.getByRole("heading", { level: 1 })).toHaveText(/Libraries\s*lib:acme\/deploy-helm$/);
  await expect(page.getByText("5d1e0c7a9b3f → 0c4e7b2a91f5")).toBeVisible();
  await page.getByRole("button", { name: "Upgrade", exact: true }).click();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/libraries\/acme~deploy-helm\/manage$/);
  expect(errors).toEqual([]);
});

test("a library's manage page holds its version, trust, yank and remove", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/libraries");
  await page.getByRole("link", { name: "lib:acme/http-probe" }).click();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/libraries\/acme~http-probe\/manage$/);
  const nav = page.getByRole("navigation", { name: "Settings" });
  await expect(nav.getByRole("link", { name: "Libraries", exact: true })).toHaveAttribute("aria-current", "page");
  await expect(page.getByRole("heading", { level: 1 })).toHaveText(/Libraries\s*lib:acme\/http-probe$/);
  await expect(page.getByText("untrusted source", { exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "Trust source" })).toBeVisible();
  await expect(page.getByRole("button", { name: "Yank this version" })).toBeVisible();

  // Upgrade opens the install dialog with the installed source in it.
  await page.getByRole("button", { name: "Upgrade" }).click();
  const upgrade = page.getByRole("dialog");
  await expect(upgrade.getByRole("textbox", { name: "Source" })).toHaveValue(/http-probe\.yaml@8c2f4e1a/);
  await upgrade.getByRole("button", { name: "Cancel" }).click();

  // Remove asks for the name to be typed, then goes back to the list.
  await page.getByRole("button", { name: "Remove library" }).click();
  const remove = page.getByRole("dialog");
  const confirm = remove.getByRole("button", { name: "Remove library" });
  await expect(confirm).toBeDisabled();
  await remove.getByRole("textbox").fill("lib:acme/http-probe");
  await confirm.click();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/libraries$/);
  expect(errors).toEqual([]);
});

test("a plugin installation's manage page holds its state, config, kill and uninstall", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/plugins");
  const table = page.getByRole("table", { name: "Installed plugins" });
  await expect(table.getByRole("row").filter({ hasText: "argocd-us" })).toContainText("killed");
  await expect(table.getByRole("row").filter({ hasText: "argocd-staging" })).toContainText("probe failed");
  await table.getByRole("row").filter({ hasText: "argocd-us" }).getByRole("link", { name: "Manage" }).click();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/plugins\/9c0d33a1e5\/manage$/);
  const nav = page.getByRole("navigation", { name: "Settings" });
  await expect(nav.getByRole("link", { name: "Plugins", exact: true })).toHaveAttribute("aria-current", "page");
  await expect(page.getByRole("heading", { level: 1 })).toHaveText(/Plugins\s*argocd-us$/);
  await expect(page.getByRole("button", { name: "Enable" })).toBeVisible();
  await expect(page.getByText("ARGOCD_UI_URL", { exact: true })).toBeVisible();
  await expect(page.getByText("wait-live", { exact: true })).toBeVisible();

  await page.getByRole("button", { name: "Uninstall plugin" }).click();
  const dialog = page.getByRole("dialog");
  await dialog.getByRole("textbox").fill("argocd-us");
  await dialog.getByRole("button", { name: "Uninstall plugin" }).click();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/plugins$/);
  expect(errors).toEqual([]);
});

test("Install plugin opens its dialog from the section header", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/plugins");
  await page.getByRole("button", { name: "Install plugin" }).click();
  const dialog = page.getByRole("dialog");
  await dialog
    .getByRole("textbox", { name: "Source" })
    .fill("github.com/acme/scarab-plugin-argocd//plugin.json@6e1d0c9b8a7f6e5d4c3b2a1f0e9d8c7b6a5f4e3d");
  await dialog.getByRole("button", { name: "Read and review" }).click();
  await expect(dialog.getByText("Manifest SHA-256", { exact: true })).toBeVisible();
  await expect(dialog.getByTestId("review-image")).toHaveText(
    "runs only an image of ghcr.io/thulasi-ram/scarab-plugin-argocd",
  );
  await dialog.getByRole("textbox", { name: "Name" }).fill("argocd-dev");
  // An image of another repository than the manifest names cannot be sent.
  const image = dialog.getByRole("textbox", { name: "Image" });
  await image.fill(`ghcr.io/acme/argocd@sha256:${"a".repeat(64)}`);
  await expect(dialog.getByText(/runs only images of/)).toBeVisible();
  await expect(dialog.getByRole("button", { name: "Install", exact: true })).toBeDisabled();
  await image.fill(`ghcr.io/thulasi-ram/scarab-plugin-argocd@sha256:${"a".repeat(64)}`);
  await dialog.getByRole("button", { name: "Install", exact: true }).click();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/plugins\/1d2e3f4a5b\/manage$/);
  await expect(page.getByRole("heading", { level: 1 })).toHaveText(/Plugins\s*argocd-dev$/);
  expect(errors).toEqual([]);
});

test("Upgrade reads a new manifest, shows what changes and whether it needs a passkey", async ({ page }) => {
  const errors = collectErrors(page);
  const repo = "ghcr.io/thulasi-ram/scarab-plugin-argocd";
  // argocd-us was installed before sources were recorded.
  await page.goto("/orgs/acme/settings/plugins/9c0d33a1e5/manage");
  await expect(page.getByText("source not recorded", { exact: true })).toBeVisible();

  // argocd-staging: the new manifest adds a rule and a gate, so it asks for a
  // passkey even on the installed image.
  await page.goto("/orgs/acme/settings/plugins/2b7e90f1c4/manage");
  await page.getByRole("button", { name: "Upgrade" }).click();
  let dialog = page.getByRole("dialog");
  await dialog.getByRole("textbox", { name: "Commit or source" }).fill("d".repeat(40));
  await dialog.getByRole("button", { name: "Read and compare" }).click();
  await expect(dialog.getByText("+ argocd: GET /api/v1/account")).toBeVisible();
  await expect(dialog.getByText("+ wait-synced")).toBeVisible();
  await expect(dialog.getByTestId("upgrade-step-up")).toContainText("confirm with your passkey");
  await dialog.getByRole("button", { name: "Cancel" }).click();

  // argocd: the new manifest drops a rule. On the installed image it needs
  // no passkey, and Upgrade applies it.
  const installed = `${repo}@sha256:${"4f2a1c9e7b3d".padEnd(64, "0")}`;
  await page.goto("/orgs/acme/settings/plugins/4f2a1c9e7b/manage");
  await expect(page.getByText(/^github\.com\/acme\/scarab-plugin-argocd\/\/manifest\.json@/)).toBeVisible();
  await page.getByRole("button", { name: "Upgrade" }).click();
  dialog = page.getByRole("dialog");
  await dialog.getByRole("textbox", { name: "Commit or source" }).fill("e".repeat(40));
  await dialog.getByRole("textbox", { name: "Image" }).fill(installed);
  const upgrade = dialog.getByRole("button", { name: "Upgrade", exact: true });
  await expect(upgrade).toBeDisabled();
  await dialog.getByRole("button", { name: "Read and compare" }).click();
  await expect(dialog.getByText("− argocd: GET /api/version")).toBeVisible();
  await expect(dialog.getByText("0 added · 1 removed", { exact: false })).toBeVisible();
  await expect(dialog.getByTestId("upgrade-step-up")).toContainText("needs no passkey");

  // A new digest is new code: it drops the answer, and read again it needs a passkey.
  const newDigest = `${repo}@sha256:${"9".repeat(64)}`;
  await dialog.getByRole("textbox", { name: "Image" }).fill(newDigest);
  await expect(upgrade).toBeDisabled();
  await dialog.getByRole("button", { name: "Read and compare" }).click();
  await expect(dialog.getByTestId("upgrade-step-up")).toContainText("confirm with your passkey");
  await expect(dialog.getByTestId("upgrade-step-up")).toContainText(`runs new code: image \`${newDigest}\``);

  // Back to the installed image: no passkey, and it applies.
  await dialog.getByRole("textbox", { name: "Image" }).fill(installed);
  await dialog.getByRole("button", { name: "Read and compare" }).click();
  await expect(dialog.getByTestId("upgrade-step-up")).toContainText("needs no passkey");
  await upgrade.click();
  await expect(page.getByText("argocd is upgraded to sha256:4f2a1c9e7b3d.")).toBeVisible();
  await expect(page.getByText(`github.com/acme/scarab-plugin-argocd//manifest.json@${"e".repeat(40)}`)).toBeVisible();
  expect(errors).toEqual([]);
});

test("switching org on a section shows the other org's settings, not the first's", async ({ page }) => {
  const errors = collectErrors(page);
  // The mock's caller administers acme and globex.
  await page.goto("/orgs/acme/settings/secrets");
  await expect(page.getByText("Org secrets · acme", { exact: true })).toBeVisible();

  await choose(page, page.getByRole("combobox", { name: "Organization" }), "globex");
  await expect(page).toHaveURL(/\/orgs\/globex\/settings\/secrets$/);
  await expect(page.getByText("Org secrets · globex", { exact: true })).toBeVisible();
  await expect(page.getByText("Org secrets · acme", { exact: true })).toHaveCount(0);
  expect(errors).toEqual([]);
});

test("the org dropdown works from the keyboard, keeps the section, and skips an org you do not administer", async ({
  page,
}) => {
  const errors = collectErrors(page);
  // The caller administers acme only, so globex, the org in the URL, shows
  // but cannot be picked.
  await page.goto("/orgs/globex/settings/secrets?admin_orgs=acme");
  const pick = page.getByRole("combobox", { name: "Organization" });
  await expect(pick).toHaveText("globex");
  await pick.focus();
  await page.keyboard.press("Enter");
  await expect(pick).toHaveAttribute("aria-expanded", "true");
  await expect(listOptions(page)).toHaveText(["globex", "acme", "New organization… (2 waiting)"]);
  await expect(listOptions(page).first()).toHaveAttribute("aria-disabled", "true");
  // The list opens on acme, the first org that can be picked; ArrowUp does
  // not move onto globex.
  await page.keyboard.press("ArrowUp");
  await page.keyboard.press("Enter");
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/secrets(\?|$)/);
  await expect(page.getByText("Org secrets · acme", { exact: true })).toBeVisible();
  await expect(pick).toBeFocused();

  // Type-ahead opens the list on the entry it matches; Enter takes it.
  await page.keyboard.press("n");
  await expect(listOptions(page).last()).toHaveClass(/\bon\b/);
  await page.keyboard.press("Enter");
  await expect(page).toHaveURL(/\/orgs\/_new$/);
  await expect(page.getByRole("heading", { level: 1, name: "New organization", exact: true })).toBeVisible();
  expect(errors).toEqual([]);
});

test("a dropdown in a dialog opens above it, and Escape closes the list, not the dialog", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/libraries");
  await page.locator("#library-policies").getByRole("button", { name: "Libraries blocked" }).click();
  const dialog = page.getByRole("dialog");
  const mode = dialog.getByRole("combobox", { name: "Mode" });
  await mode.click();
  // Two options: a plain list, no search box.
  await expect(listOptions(page)).toHaveText(["audit", "enforce"]);
  await expect(page.getByRole("textbox", { name: /^Search/ })).toHaveCount(0);
  await page.keyboard.press("Escape");
  await expect(page.getByRole("listbox")).toHaveCount(0);
  await expect(dialog).toBeVisible();
  await expect(mode).toBeFocused();
  // A click lands on the option, so the list is above the dialog.
  await choose(page, mode, "enforce");
  await expect(mode).toHaveText("enforce");
  await expect(dialog).toBeVisible();
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

test("Connections lists only the org's own connections", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/connections");
  const page_ = page.locator("section.page");
  const list = page_.getByRole("table", { name: "Forge connections" });
  await expect(list.getByText("forgejo-7f3a91c2", { exact: true })).toBeVisible();
  // The connections that serve no org wait on the New organization page,
  // even for a global admin.
  await expect(page_.getByText("Waiting for an organization")).toHaveCount(0);
  await expect(page_.getByText("Unassigned connections")).toHaveCount(0);
  await expect(page_.getByText(/github-install-/)).toHaveCount(0);

  // One row per connection: forge and host linking to its page, how many
  // repositories it has enabled, and none of them listed.
  const row = list.getByRole("row", { name: /forgejo-7f3a91c2/ });
  await expect(row.getByRole("link", { name: "Forgejo git.acme.dev" })).toHaveAttribute(
    "href",
    "/orgs/acme/settings/connections/forgejo-7f3a91c2",
  );
  await expect(row).toContainText("1 enabled repository");
  await expect(list.getByRole("row", { name: /gh-acme/ })).toContainText("7 enabled repositories");
  await expect(list.getByRole("link", { name: "acme/infra" })).toHaveCount(0);
  // Re-sync and Remove live on the connection's page, not on the list.
  await expect(list.getByRole("button", { name: /Re-sync|Remove/ })).toHaveCount(0);

  // Add Forgejo connection sits in the panel header and opens a dialog.
  await page_.getByRole("button", { name: "Add Forgejo connection" }).click();
  await expect(page.getByRole("dialog")).toContainText("Connect a Forgejo or Codeberg instance");
  await page.getByRole("dialog").getByRole("button", { name: "Cancel" }).click();
  expect(errors).toEqual([]);
});

test("a connection's page holds its credential, deliveries, app check, re-sync and remove", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/connections");
  await page.getByRole("row", { name: /gh-acme/ }).getByRole("link", { name: "Manage", exact: true }).click();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/connections\/gh-acme$/);
  // Inside the settings frame, with Connections marked.
  const nav = page.getByRole("navigation", { name: "Settings" });
  await expect(nav.getByRole("link", { name: "Connections" })).toHaveAttribute("aria-current", "page");
  const page_ = page.locator("section.page");
  await expect(page_.getByRole("heading", { level: 1 })).toContainText("gh-acme");
  await expect(page_.locator(".panel", { hasText: "Credential" })).toContainText(/github-app\s*Status\s*Resolves\./);
  await expect(page_).toContainText("app configuration matches what Scarab needs");
  await expect(page_.getByRole("link", { name: "Manage on GitHub →" })).toHaveAttribute(
    "href",
    "https://github.com/settings/installations",
  );
  await expect(page_.getByRole("link", { name: "Manage repositories" })).toHaveAttribute(
    "href",
    "/orgs/acme?connection=gh-acme",
  );
  await page_.getByRole("button", { name: /Re-sync/ }).click();
  await expect(page_.locator(".notice-ok")).toContainText("enabled repositories confirmed");

  // Removing it asks for its id typed exactly.
  await page_.getByRole("button", { name: "Remove connection" }).click();
  const dialog = page.getByRole("dialog");
  await expect(dialog).toContainText("This also removes its 7 projects");
  const go = dialog.getByRole("button", { name: "Remove connection" });
  await expect(go).toBeDisabled();
  await dialog.getByRole("textbox", { name: "Type gh-acme to confirm" }).fill("gh-acme");
  await expect(go).toBeEnabled();
  await dialog.getByRole("button", { name: "Cancel" }).click();

  // A connection the org does not have says so.
  await page.goto("/orgs/acme/settings/connections/nope");
  await expect(page.locator("section.page")).toContainText("nope is not a connection of acme");
  expect(errors).toEqual([]);
});

// Manage repositories opens the org page narrowed to the connection, where
// its repositories are enabled; a Forgejo repository's webhook is registered
// again from its Settings tab.
test("a connection's repositories are managed on the org page, and a Forgejo webhook on the repository", async ({
  page,
}) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/connections/forgejo-7f3a91c2");
  await page.getByRole("link", { name: "Manage repositories" }).click();
  await expect(page).toHaveURL(/\/orgs\/acme\?connection=forgejo-7f3a91c2$/);
  const rows = page.locator(".org-repo-row");
  await expect(rows).toHaveCount(3);
  await expect(rows).toContainText(["infra", "runbooks", "terraform-modules"]);

  const tf = rows.filter({ hasText: "terraform-modules" });
  await tf.getByRole("button", { name: "Enable" }).click();
  await expect(tf).toContainText("Enabled");
  await page.getByRole("button", { name: "Show all" }).click();
  await expect(page).toHaveURL(/\/orgs\/acme$/);
  await expect(page.getByText("1–25 of 33")).toBeVisible();

  // In the page, not a reload: the mock keeps the enable only until one.
  await page.getByRole("textbox", { name: "Filter repositories by name" }).fill("terraform");
  await page.getByRole("link", { name: "terraform-modules", exact: true }).click();
  await page.locator(".tabs").getByRole("button", { name: "Settings" }).click();
  await expect(page).toHaveURL(/\/acme\/terraform-modules\?tab=settings$/);
  const repository = page.locator(".panel", {
    has: page.locator(".panel-h", { hasText: "Repository" }),
  });
  await expect(repository.getByRole("link", { name: "forgejo-7f3a91c2" })).toBeVisible();
  await repository.getByRole("button", { name: "Register webhook" }).click();
  await expect(repository).toContainText("Webhook registered.");

  // GitHub delivers every repository's events to the App: no webhook here.
  await page.goto("/acme/scarab?tab=settings");
  await expect(page.getByRole("button", { name: "Disable repository" })).toBeVisible();
  await expect(page.getByRole("button", { name: "Register webhook" })).toHaveCount(0);
  expect(errors).toEqual([]);
});

test("a global admin makes a new org from the org dropdown and lands on its connections", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/members");
  const pick = page.getByRole("combobox", { name: "Organization" });
  await pick.click();
  await expect(listOptions(page).last()).toHaveText("New organization… (2 waiting)");
  await listOptions(page).last().click();
  await expect(page).toHaveURL(/\/orgs\/_new$/);
  await expect(page.getByRole("heading", { level: 1, name: "New organization", exact: true })).toBeVisible();
  // The org settings frame, with New organization… picked and no sections.
  await expect(pick).toHaveText("New organization… (2 waiting)");
  const nav = page.getByRole("navigation", { name: "Settings" });
  await expect(nav.getByText("Organization settings", { exact: true })).toBeVisible();
  await expect(nav.getByRole("link")).toHaveCount(0);
  await expect(page.getByText(/install the Scarab App on the account/)).toBeVisible();
  const add = page.getByRole("button", { name: "Add connection" });
  await expect(add).toBeDisabled();
  await page.locator("#conn-org").fill("initech");
  await page.locator("#conn-url").fill("https://git.initech.dev");
  await page.locator("#conn-token").fill("x".repeat(24));
  await add.click();
  await expect(page).toHaveURL(/\/orgs\/initech\/settings\/connections$/);
  // The page renders initech's connections, not the "You do not administer"
  // panel: the new org reached `admin_orgs` on the next `/v1/me`.
  const page_ = page.locator("section.page");
  await expect(page_.getByText("forgejo-5be0d4a1", { exact: true })).toBeVisible();
  await expect(page_.getByRole("link", { name: "Forgejo git.initech.dev" })).toBeVisible();
  await expect(page_.getByText(/You do not administer/)).toHaveCount(0);
  expect(errors).toEqual([]);
});

test("a global admin with no org lands on New organization, and the old server page sends them there", async ({ page }) => {
  const errors = collectErrors(page);
  // `?admin_orgs=,` makes the mock caller a global admin of no org.
  await page.goto("/settings?admin_orgs=,");
  await expect(page).toHaveURL(/\/orgs\/_new(\?|$)/);
  await expect(page.getByRole("heading", { level: 1, name: "New organization", exact: true })).toBeVisible();
  const pick = page.getByRole("combobox", { name: "Organization" });
  await pick.click();
  await expect(listOptions(page)).toHaveText(["New organization… (2 waiting)"]);
  await page.keyboard.press("Escape");
  await page.locator("button.avatar").click();
  await expect(
    page.getByRole("menu").getByRole("menuitem", { name: "Organization settings" }),
  ).toHaveAttribute("href", "/orgs/_new");

  await page.goto("/settings/server/connect-forge");
  await expect(page).toHaveURL(/\/orgs\/_new$/);
  await page.goto("/settings/server");
  await expect(page).toHaveURL(/\/orgs\/_new$/);
  expect(errors).toEqual([]);
});

test("the org dropdown and the palette count the connections waiting for an org", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/members");
  const pick = page.getByRole("combobox", { name: "Organization" });
  await pick.click();
  await expect(listOptions(page).last()).toHaveText("New organization… (2 waiting)");
  await page.keyboard.press("Escape");
  await page.keyboard.press("Control+k");
  const palette = page.getByRole("dialog", { name: "Command palette" });
  await palette.getByRole("textbox").fill("new organization");
  await expect(palette.getByText("New organization (2 waiting)", { exact: true })).toBeVisible();
  await page.keyboard.press("Escape");
  expect(errors).toEqual([]);
});

test("New organization lists the connections waiting for an org, and a global admin gives each one", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/_new");
  const page_ = page.locator("section.page");
  const waiting = page_.locator(".panel", { hasText: "Waiting for an organization" });
  // Above the new-org form.
  const form = page_.locator(".panel", { hasText: "First forge connection" });
  expect((await waiting.boundingBox())!.y).toBeLessThan((await form.boundingBox())!.y);
  await expect(
    waiting.getByText(
      "Scarab could not prove these accounts own organizations of the same names, so they serve none until you choose.",
    ),
  ).toBeVisible();
  const acmeRow = waiting.locator("li.conn", { hasText: "github-install-90211" });
  const otherRow = waiting.locator("li.conn", { hasText: "github-install-90377" });
  await expect(acmeRow.getByText("GitHub · github.com")).toBeVisible();
  await expect(acmeRow.getByText("90211", { exact: true })).toBeVisible();
  await expect(acmeRow.getByText("resolves")).toBeVisible();
  await expect(acmeRow.getByText(/none recorded yet/)).toBeVisible();
  await expect(acmeRow.getByRole("button", { name: "Remove", exact: true })).toBeVisible();
  // It binds nothing on a re-sync, so it offers none.
  await expect(acmeRow.getByRole("button", { name: /Re-sync/ })).toHaveCount(0);

  // acme is also served by a Forgejo connection, so the server refuses to
  // assign a GitHub connection to it, and the page shows its reason as is.
  await acmeRow.getByRole("button", { name: "Assign to an organization" }).click();
  await expect(acmeRow.getByRole("combobox", { name: "Organization to assign" })).toHaveText("acme");
  await acmeRow.getByRole("button", { name: "Assign", exact: true }).click();
  await expect(
    acmeRow.getByText(
      "connection `github-install-90211` cannot serve org `acme`: org `acme` has connection `forgejo-7f3a91c2` to https://git.acme.dev, another forge or host than https://api.github.com; an account of the same name there is not known to be the same account",
    ),
  ).toBeVisible();
  await acmeRow.getByRole("button", { name: "Cancel" }).click();

  // A new org from the other one: an existing org's name is refused here.
  await otherRow.getByRole("button", { name: "Create a new organization from it" }).click();
  const name = otherRow.getByRole("textbox", { name: "New organization name" });
  const create = otherRow.getByRole("button", { name: "Create organization" });
  await name.fill("acme");
  await expect(otherRow.getByText("acme is already an organization. Use Assign to an organization.")).toBeVisible();
  await expect(create).toBeDisabled();
  await name.fill("a/b");
  await expect(create).toBeDisabled();
  await name.fill("umbrella");
  await create.click();
  await expect(page).toHaveURL(/\/orgs\/umbrella\/settings\/connections$/);
  await expect(page_.getByText("github-install-90377", { exact: true })).toBeVisible();
  const pick = page.getByRole("combobox", { name: "Organization" });
  await pick.click();
  await expect(listOptions(page).last()).toHaveText("New organization… (1 waiting)");
  await page.keyboard.press("Escape");

  // The other one joins umbrella, which a GitHub connection now serves.
  await choose(page, pick, "New organization… (1 waiting)");
  await expect(page).toHaveURL(/\/orgs\/_new$/);
  await expect(
    waiting.getByText("Scarab could not prove this account owns an organization of the same name, so it serves none until you choose."),
  ).toBeVisible();
  await acmeRow.getByRole("button", { name: "Assign to an organization" }).click();
  await choose(page, acmeRow.getByRole("combobox", { name: "Organization to assign" }), "umbrella");
  await acmeRow.getByRole("button", { name: "Assign", exact: true }).click();
  await expect(page).toHaveURL(/\/orgs\/umbrella\/settings\/connections$/);
  await expect(page_.getByText("github-install-90211", { exact: true })).toBeVisible();
  await pick.click();
  await expect(listOptions(page).last()).toHaveText("New organization…");
  await page.keyboard.press("Escape");

  // None wait, so the panel is gone.
  await choose(page, pick, "New organization…");
  await expect(page).toHaveURL(/\/orgs\/_new$/);
  await expect(page_.locator(".panel", { hasText: "First forge connection" })).toBeVisible();
  await expect(waiting).toHaveCount(0);
  expect(errors).toEqual([]);
});

test("org secrets: New secret in the header, Replace with the name filled in, Delete with the name typed", async ({
  page,
}) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/secrets");
  const panel = page.locator(".panel", { hasText: "Org secrets · acme" });
  const list = panel.getByRole("list", { name: "Secrets" });
  await expect(list.getByRole("listitem")).toHaveCount(2);

  // New secret, from the panel header.
  await panel.getByRole("button", { name: "New secret" }).click();
  let dialog = page.getByRole("dialog");
  await expect(dialog.getByRole("heading", { name: "New secret" })).toBeVisible();
  await dialog.getByRole("textbox", { name: "Name" }).fill("NPM_TOKEN");
  await dialog.getByLabel("Value").fill("npm_x");
  await dialog.getByRole("button", { name: "Save" }).click();
  await expect(list.getByText("NPM_TOKEN", { exact: true })).toBeVisible();

  // Replace keeps the name and still asks before overwriting.
  await list.getByRole("listitem").filter({ hasText: "GHCR_TOKEN" }).getByRole("button", { name: "Replace" }).click();
  dialog = page.getByRole("dialog");
  await expect(dialog.getByRole("heading", { name: "Replace secret" })).toBeVisible();
  await expect(dialog.getByRole("textbox", { name: "Name" })).toHaveValue("GHCR_TOKEN");
  const save = dialog.getByRole("button", { name: "Save" });
  await expect(save).toBeDisabled();
  await dialog.getByRole("checkbox").check();
  await expect(save).toBeEnabled();
  await dialog.getByRole("button", { name: "Cancel" }).click();

  // Delete asks for the name typed exactly.
  await list.getByRole("listitem").filter({ hasText: "SENTRY_DSN" }).getByRole("button", { name: "Delete" }).click();
  dialog = page.getByRole("dialog");
  const go = dialog.getByRole("button", { name: "Delete secret" });
  await expect(go).toBeDisabled();
  await dialog.getByRole("textbox", { name: "Type SENTRY_DSN to confirm" }).fill("SENTRY_DSN");
  await go.click();
  await expect(list.getByText("SENTRY_DSN", { exact: true })).toHaveCount(0);

  // One note, inside the panel; no button under the list.
  await expect(panel.getByText(/Values are never shown\. Setting a name again replaces it\./)).toHaveCount(1);
  await expect(page.locator("section.page").getByRole("button", { name: "New secret" })).toHaveCount(1);
  expect(errors).toEqual([]);
});

test("a token minted under Account settings shows in the org's list", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/settings/tokens");
  await expect(page.getByRole("heading", { level: 1, name: "API tokens", exact: true })).toBeVisible();
  const page_ = page.locator("section.page");
  const mine = page_.getByRole("table", { name: "Your API tokens" });
  await expect(mine.getByText("release-bot", { exact: true })).toBeVisible();
  // Your own list has no owner column; each row carries its state tags.
  await expect(mine.getByRole("columnheader", { name: "Owner" })).toHaveCount(0);
  await expect(mine.getByRole("row", { name: /status poller/ })).toContainText("expires soon");
  await expect(mine.getByRole("row", { name: /amy's laptop/ })).toContainText("revoked");
  await expect(mine.getByRole("row", { name: /amy's laptop/ }).getByRole("button", { name: "Revoke" })).toHaveCount(0);

  // New token sits in the panel header.
  await page_.locator(".panel-h").getByRole("button", { name: "New token" }).click();
  await page.getByRole("textbox", { name: "name" }).fill("e2e laptop");
  await page.getByRole("button", { name: "Mint token" }).click();
  await expect(page.getByText(/^scarab_pat_/)).toBeVisible();
  await page.getByRole("button", { name: "I've copied it" }).click();
  await expect(page_.getByText("e2e laptop", { exact: true })).toBeVisible();
  await expect(mine.getByRole("row", { name: /e2e laptop/ })).toContainText("never used");

  // The org's list: the new token with its owner, and no mint button there.
  // Organization settings is its own page, reached from the user menu.
  await page.locator("button.avatar").click();
  await page.getByRole("menu").getByRole("menuitem", { name: "Organization settings" }).click();
  const nav = page.getByRole("navigation", { name: "Settings" });
  await nav.getByRole("link", { name: "API tokens", exact: true }).click();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/tokens$/);
  const row = page_.getByRole("row", { name: /e2e laptop/ });
  await expect(row).toBeVisible();
  await expect(row.getByText("a.kim", { exact: true })).toBeVisible();
  await expect(row).toContainText("active");
  await expect(row).toContainText("up to viewer");
  await expect(page_.getByRole("button", { name: "New token" })).toHaveCount(0);

  // By last use the project token is second; by calls the status poller,
  // which is used less recently but more often, takes its place.
  const second = page_.getByRole("row").nth(2);
  await expect(second).toContainText("orders-api deploy gate");
  await page_.getByRole("button", { name: "Calls", exact: true }).click();
  await expect(second).toContainText("status poller");
  await expect(page_.getByRole("row").nth(3)).toContainText("orders-api deploy gate");

  // Revoking asks for the token's name typed exactly.
  await row.getByRole("button", { name: "Revoke", exact: true }).click();
  const dialog = page.getByRole("dialog");
  const go = dialog.getByRole("button", { name: "Revoke token" });
  await expect(go).toBeDisabled();
  await dialog.getByRole("textbox", { name: "Type e2e laptop to confirm" }).fill("e2e laptop");
  await expect(go).toBeEnabled();
  expect(errors).toEqual([]);
});

test("Members lists one row per person, each managed on its own page", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/members");
  const list = page.getByRole("list", { name: "Members" });
  const jordan = list.getByRole("listitem").filter({ hasText: "Jordan Lee" });
  await expect(jordan).toContainText("member all of acme");
  await expect(jordan).toContainText("2 repositories");
  // Repository roles only: listed, with no org role.
  const okafor = list.getByRole("listitem").filter({ hasText: "r.okafor" });
  await expect(okafor).toContainText("no org role");
  await expect(okafor).toContainText("not signed in yet");
  // No inline revoke or grant form: those are on the person's page and in
  // the Grant role dialog. Re-sync lives on the repository.
  const view = page.locator("section.page");
  await expect(view.getByRole("button", { name: /revoke/i })).toHaveCount(0);
  await expect(view.getByRole("textbox", { name: "Find a user" })).toHaveCount(0);
  await expect(view.getByRole("button", { name: /re-sync/i })).toHaveCount(0);

  await jordan.getByRole("link", { name: "Manage" }).click();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/members\/j\.lee\/manage$/);
  await expect(page.getByRole("heading", { level: 1 })).toHaveText(/Members\s*Jordan Lee$/);
  const nav = page.getByRole("navigation", { name: "Settings" });
  await expect(nav.locator('[aria-current="page"]')).toHaveText("Members");

  // Repository roles are read-only links to each repository's Access.
  await expect(page.getByRole("link", { name: "scarab", exact: true })).toHaveAttribute(
    "href",
    "/acme/scarab?tab=settings",
  );
  await expect(page.getByText("granted in Scarab").first()).toBeVisible();
  await expect(page.getByText(/changed on that repository's Settings › Access/)).toHaveCount(1);

  // Change the org role: a grant replaces it.
  const role = page.getByRole("combobox", { name: "Org role" });
  await expect(role).toHaveText("member");
  await expect(page.getByRole("button", { name: "Change role" })).toBeDisabled();
  await choose(page, role, "admin");
  await page.getByRole("button", { name: "Change role" }).click();
  await expect(page.getByText("Jordan Lee is admin on all of acme.")).toBeVisible();

  // Revoke asks for the subject typed exactly; repository roles stay.
  await page.getByRole("button", { name: "Revoke" }).click();
  const dialog = page.getByRole("dialog");
  const go = dialog.getByRole("button", { name: "Revoke role" });
  await expect(go).toBeDisabled();
  await dialog.getByRole("textbox").fill("j.lee");
  await go.click();
  await expect(page.getByText("Jordan Lee no longer holds an org role in acme.")).toBeVisible();
  await expect(page.locator(".kv").getByText("no org role")).toBeVisible();

  await page.getByRole("link", { name: "scarab", exact: true }).click();
  await expect(page).toHaveURL(/\/acme\/scarab\?tab=settings$/);
  await expect(page.locator(".panel-h", { hasText: "Access" })).toBeVisible();
  expect(errors).toEqual([]);
});

test("Grant role opens a dialog from the section header and grants an org role", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/members");
  await page.locator(".panel-h").getByRole("button", { name: "Grant role" }).click();
  const dialog = page.getByRole("dialog");
  const grant = dialog.getByRole("button", { name: "Grant role" });
  await expect(grant).toBeDisabled();
  await dialog.getByRole("textbox", { name: "Find a user" }).fill("t.ng");
  await dialog.getByRole("button", { name: /Taylor Nguyen/ }).click();
  await choose(page, dialog.getByRole("combobox", { name: "Role" }), "viewer");
  await grant.click();
  await expect(dialog).toHaveCount(0);
  await expect(page.getByText("t.nguyen is viewer on all of acme.")).toBeVisible();
  await expect(
    page.getByRole("list", { name: "Members" }).getByRole("listitem").filter({ hasText: "Taylor Nguyen" }),
  ).toContainText("viewer all of acme");
  expect(errors).toEqual([]);
});

test("a member's page takes a subject with any characters", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/members");
  await page.getByRole("list", { name: "Members" }).getByRole("link", { name: "Sam Patel" }).click();
  await expect(page).toHaveURL(/\/members\/okta%3As\.patel%40acme\.example\/manage$/);
  await expect(page.locator(".page-sub")).toContainText("okta:s.patel@acme.example");
  await expect(page.getByText("No roles on single repositories.")).toBeVisible();
  await page.goto("/orgs/acme/settings/members/nobody/manage");
  await expect(page.getByText("nobody holds no role in acme.")).toBeVisible();
  expect(errors).toEqual([]);
});

test("a repository's Access lists its roles and the org roles that reach it, and re-syncs from the forge", async ({
  page,
}) => {
  const errors = collectErrors(page);
  await page.goto("/acme/scarab?tab=settings");
  const view = page.locator("section.page");
  // The placeholder rows are gone; the connection comes from the server.
  await expect(view.getByText("Default branch")).toHaveCount(0);
  await expect(view.getByRole("button", { name: "Disable repository" })).toBeVisible();
  await expect(view.getByRole("link", { name: "gh-acme" })).toBeVisible();

  const access = view.locator(".panel", { has: page.locator(".panel-h", { hasText: "Access" }) });
  await expect(access.locator("li.tok", { hasText: "Jordan Lee" })).toContainText("granted in Scarab");
  const okafor = access.locator("li.tok", { hasText: "r.okafor" });
  await expect(okafor).toContainText("from GitHub collaborators");
  await expect(okafor).toContainText("not signed in yet");
  const avery = access.locator("li.tok.access-via-org", { hasText: "Avery Kim" });
  await expect(avery).toContainText("via org role owner");
  await expect(avery.getByRole("button")).toHaveCount(0);

  await access.getByRole("button", { name: "Re-sync collaborators" }).click();
  const dialog = page.getByRole("dialog");
  await expect(dialog.getByRole("heading", { name: "Re-sync collaborators from GitHub" })).toBeVisible();
  await expect(dialog).toContainText("write → member");
  await expect(dialog).toContainText("Roles granted or revoked in Scarab are never changed.");
  await dialog.getByRole("button", { name: "Re-sync", exact: true }).click();
  await expect(dialog).toContainText("Added: 1");
  await expect(dialog).toContainText("p.ito");
  await expect(dialog).toContainText("Changed: 1");
  await expect(dialog).toContainText("viewer → member");
  await dialog.getByRole("button", { name: "Done" }).click();
  await expect(dialog).toHaveCount(0);
  await expect(access.locator("li.tok", { hasText: "Taylor Nguyen" })).toContainText("member");
  await expect(access.locator("li.tok", { hasText: "p.ito" })).toBeVisible();
  expect(errors).toEqual([]);
});

test("a repository's Access is for org admins", async ({ page }) => {
  await page.goto("/acme/scarab?tab=settings&admin_orgs=globex");
  const view = page.locator("section.page");
  await expect(view.getByText(/Only an admin of acme can see and change who has access/)).toBeVisible();
  await expect(view.getByRole("button", { name: "Re-sync collaborators" })).toHaveCount(0);
  await expect(view.getByRole("button", { name: "Disable repository" })).toHaveCount(0);
});

// The Settings tab's Disable is the org page's: the same typed-name
// confirmation and unbind, then the repository has no page, so it lands on
// the org page with the repository not enabled.
test("an admin disables an unreachable repository from its Settings tab", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/acme/messaging?tab=settings");
  const repository = page.locator(".panel", {
    has: page.locator(".panel-h", { hasText: "Repository" }),
  });
  await expect(repository.locator(".repo-disable")).toContainText(
    "The GitHub App no longer covers this repository",
  );
  await repository.getByRole("button", { name: "Disable repository" }).click();

  const dialog = page.getByRole("dialog");
  await expect(dialog.getByRole("heading", { name: "Disable acme/messaging?" })).toBeVisible();
  await expect(dialog).toContainText("environments, secrets and access grants");
  const go = dialog.getByRole("button", { name: "Disable repository" });
  await expect(go).toBeDisabled();
  await dialog.getByRole("textbox").fill("acme/messaging");
  await go.click();

  await expect(page).toHaveURL(/\/orgs\/acme$/);
  await page.getByRole("textbox", { name: "Filter repositories by name" }).fill("messaging");
  const row = page.locator(".org-repo-row", { hasText: "messaging" });
  await expect(row).toContainText("Not enabled");
  await expect(row.getByRole("button", { name: "Enable" })).toBeVisible();
  expect(errors).toEqual([]);
});
