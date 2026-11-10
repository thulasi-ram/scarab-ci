// Settings shell (git-bug 734ffa1), in mock mode: account and org settings
// are separate pages, every section in a page's sidebar opens at its own URL
// under its own heading, the user menu reaches each page, a global admin makes
// a new org from the org dropdown, the old addresses redirect, and nothing
// errors on the way.
import { test, expect, type Page } from "@playwright/test";

function collectErrors(page: Page): string[] {
  const errors: string[] = [];
  page.on("pageerror", (err) => errors.push(`pageerror: ${err.message}`));
  page.on("console", (msg) => {
    if (msg.type() === "error") errors.push(`console.error: ${msg.text()}`);
  });
  return errors;
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
  await expect(page.getByRole("combobox", { name: "Organization" })).toHaveValue("globex");

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
  await expect(pick.locator("option")).toHaveText(["acme", "New organization… (2 waiting)"]);

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

test("an org the caller does not administer shows no org sections", async ({ page }) => {
  await page.goto("/orgs/initech/settings/members");
  await expect(page.getByText(/You do not administer/)).toBeVisible();
  const nav = page.getByRole("navigation", { name: "Settings" });
  await expect(nav.getByRole("link", { name: "Members" })).toHaveCount(0);
});

test("an environment opens read-only and its Edit button opens the edit screen", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/environments");
  // The list shows the display name, with the name pipelines use beside it.
  await expect(page.locator(".oenv-name", { hasText: "production" })).toBeVisible();
  await page.getByRole("link", { name: "Production", exact: true }).click();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/environments\/production$/);

  // The view: the record, and no way to change it from here.
  const view = page.locator("section.page");
  await expect(view.getByText("customer-facing, eu-west-1 + us-east-1")).toBeVisible();
  await expect(view.getByText("projects that may use Production")).toBeVisible();
  await expect(view.getByText("Project overlays and grants")).toBeVisible();
  await expect(view.getByText(/who may deploy/i)).toHaveCount(0);
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
  await expect(page.getByRole("heading", { level: 1 })).toHaveText(/Environments\s*Production$/);
  await expect(page.locator(".page-sub")).toContainText("production");

  await view.getByRole("link", { name: "Edit", exact: true }).click();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/environments\/production\/edit$/);

  // The edit screen: every change the view left out, in the same frame.
  await expect(nav).toBeVisible();
  await expect(nav.getByRole("link", { name: "Environments", exact: true })).toHaveAttribute(
    "aria-current",
    "page",
  );
  await expect(nav.locator('[aria-current="page"]')).toHaveCount(1);
  await expect(page.getByRole("heading", { level: 1 })).toHaveText(/Environments\s*Production\s*Edit/);
  const edit = page.locator("section.page");
  await expect(edit.getByRole("textbox", { name: "description", exact: true })).toHaveValue(
    "customer-facing, eu-west-1 + us-east-1",
  );
  await expect(edit.getByRole("textbox", { name: "Display name", exact: true })).toHaveValue("Production");
  await expect(edit.getByRole("textbox", { name: "Approvers", exact: true })).toHaveValue("ops-lead");
  await expect(edit.getByRole("checkbox", { name: /every project in acme/i })).toBeChecked();
  await expect(edit.getByRole("heading", { name: /Projects that may use Production/ })).toBeVisible();
  // Nothing changed yet, so there is nothing to save.
  await expect(edit.getByRole("button", { name: "Save", exact: true })).toBeDisabled();
  await edit.getByRole("textbox", { name: "description", exact: true }).fill("customer-facing, eu-west-1");
  await expect(edit.getByRole("button", { name: "Save", exact: true })).toBeEnabled();

  // Delete asks, in a dialog, for the exact name before it is allowed.
  await edit.getByRole("button", { name: "Delete environment" }).click();
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

  // Cancel goes back to the view without saving.
  await edit.getByRole("link", { name: "Cancel" }).click();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/environments\/production$/);
  expect(errors).toEqual([]);
});

test("an environment's approval gate settings save and show on its page", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/environments/production");
  const view = page.locator("section.page");
  await expect(view.getByText("add an approval gate to the run")).toBeVisible();
  await expect(view.getByText("the approver", { exact: true })).toBeVisible();

  await view.getByRole("link", { name: "Edit", exact: true }).click();
  const edit = page.locator("section.page");
  const mode = edit.getByRole("combobox", { name: "When a pipeline has no approval gate" });
  await expect(mode).toHaveValue("add");
  await expect(edit.getByRole("textbox", { name: "Added gate expiry" })).toHaveValue("86400");
  await expect(edit.getByRole("textbox", { name: "Maximum approval age" })).toHaveValue("86400");
  // One approver: a count above it cannot be saved.
  const required = edit.getByRole("textbox", { name: "Required approvals" });
  await required.fill("2");
  await expect(edit.getByRole("alert")).toContainText("With one approver, the only count is 1.");
  await expect(edit.getByRole("button", { name: "Save", exact: true })).toBeDisabled();
  await required.fill("1");
  await mode.selectOption("refuse");
  await edit.getByRole("button", { name: "Save", exact: true }).click();

  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/environments\/production$/);
  await expect(view.getByText("refuse the run")).toBeVisible();
  await expect(view.getByText("1 of 1")).toBeVisible();
  expect(errors).toEqual([]);
});

test("a wait timer with no approvers is disabled and its environment warns", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/environments/staging");
  const view = page.locator("section.page");
  await expect(view.getByRole("status")).toContainText(
    "The 10 minutes wait timer does nothing until this environment has approvers",
  );

  await page.goto("/orgs/acme/settings/environments/staging/edit");
  const edit = page.locator("section.page");
  const wait = edit.getByRole("textbox", { name: "Wait timer" });
  await expect(wait).toBeDisabled();
  await expect(edit.getByText(/Needs approvers: the timer waits on the approval gate/)).toBeVisible();
  await expect(edit.getByRole("textbox", { name: "Required approvals" })).toBeDisabled();
  // An approver turns both on.
  await edit.getByRole("textbox", { name: "Approvers", exact: true }).fill("ops-lead");
  await expect(wait).toBeEnabled();
  await expect(edit.getByRole("textbox", { name: "Required approvals" })).toBeEnabled();
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

test("the Policies page lists CEL policies with their counts and the typed settings read-only", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/policies");
  await expect(page.getByRole("heading", { level: 1, name: "Policies", exact: true })).toBeVisible();

  // CEL policies: live ones only, with what they apply to and their counts.
  const cel = page.getByRole("list", { name: "CEL policies" });
  await expect(cel.getByRole("listitem")).toHaveCount(2);
  const friday = cel.getByRole("listitem").filter({ hasText: "no-friday-deploys" });
  await expect(friday).toContainText("Applies to projects");
  await expect(friday).toContainText("all projects");
  await expect(friday).toContainText("3 matched · 1 denied");
  const release = cel.getByRole("listitem").filter({ hasText: "release-from-main" });
  await expect(release).toContainText("audit");
  await expect(release).toContainText("web-*, except web-legacy");
  await expect(release).toContainText("2 matched · 0 denied · 1 would have denied");
  await expect(cel).not.toContainText("old-rule");

  // The decision log's newest entries.
  const recent = page.getByRole("list", { name: "Recent decisions" });
  await expect(recent.getByRole("listitem")).toHaveCount(4);
  await expect(recent).toContainText("No deploys to production on Fridays.");

  // Typed settings, read-only, each linking to where it is edited.
  const envs = page.getByRole("list", { name: "Environment rules" });
  await expect(envs.getByRole("listitem").filter({ hasText: "Production" })).toContainText(
    "adds an approval gate when missing",
  );
  const lists = page.getByRole("list", { name: "Library and plugin lists" });
  await expect(lists).toContainText("Libraries allowed");
  await expect(lists).toContainText("github.com/thulasi-ram/*");
  await expect(page.locator("section.page").getByRole("textbox")).toHaveCount(0);
  await lists.getByRole("link", { name: "Libraries" }).click();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/libraries$/);

  await page.goto("/orgs/acme/settings/policies");
  await envs.getByRole("listitem").filter({ hasText: "Production" }).getByRole("link", { name: "Edit" }).click();
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/environments\/production\/edit$/);
  expect(errors).toEqual([]);
});

test("duplicating an environment shows what was copied and what is left to do", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/environments/production");
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
  await expect(page).toHaveURL(/\/orgs\/acme\/settings\/environments\/production-eu$/);
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

test("switching org on a section shows the other org's settings, not the first's", async ({ page }) => {
  const errors = collectErrors(page);
  // The mock's caller administers acme and globex.
  await page.goto("/orgs/acme/settings/secrets");
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

test("Connections lists only the org's own connections", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/connections");
  const page_ = page.locator("section.page");
  await expect(page_.getByText("forgejo-7f3a91c2", { exact: true })).toBeVisible();
  // The connections that serve no org wait on the New organization page,
  // even for a global admin.
  await expect(page_.getByText("Waiting for an organization")).toHaveCount(0);
  await expect(page_.getByText("Unassigned connections")).toHaveCount(0);
  await expect(page_.getByText(/github-install-/)).toHaveCount(0);

  // A connection shows how many repositories it has enabled and links to
  // them on the org page; it lists none and enables or disables none.
  const row = page_.locator("li.conn", { hasText: "forgejo-7f3a91c2" });
  await expect(row).toContainText("1 enabled repository");
  await expect(page_.locator("li.conn", { hasText: "gh-acme" })).toContainText("7 enabled repositories");
  await expect(page_.getByRole("button", { name: "Add repository" })).toHaveCount(0);
  await expect(page_.getByRole("button", { name: "Disable", exact: true })).toHaveCount(0);
  await expect(page_.getByRole("link", { name: "acme/infra" })).toHaveCount(0);
  await expect(row.getByRole("link", { name: "Manage repositories" })).toHaveAttribute(
    "href",
    "/orgs/acme?connection=forgejo-7f3a91c2",
  );

  // Removing a connection asks for its id typed exactly.
  await row.getByRole("button", { name: "Remove", exact: true }).click();
  const dialog = page.getByRole("dialog");
  await expect(dialog).toContainText("This also removes its 1 project");
  const go = dialog.getByRole("button", { name: "Remove connection" });
  await expect(go).toBeDisabled();
  await dialog.getByRole("textbox", { name: "Type forgejo-7f3a91c2 to confirm" }).fill("forgejo-7f3a91c2");
  await expect(go).toBeEnabled();
  expect(errors).toEqual([]);
});

// Manage repositories opens the org page narrowed to the connection, where
// its repositories are enabled; a Forgejo repository's webhook is registered
// again from its Settings tab.
test("a connection's repositories are managed on the org page, and a Forgejo webhook on the repository", async ({
  page,
}) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/connections");
  const row = page.locator("li.conn", { hasText: "forgejo-7f3a91c2" });
  await row.getByRole("link", { name: "Manage repositories" }).click();
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
  await expect(pick.locator("option").last()).toHaveText("New organization… (2 waiting)");
  await pick.selectOption({ label: "New organization… (2 waiting)" });
  await expect(page).toHaveURL(/\/orgs\/_new$/);
  await expect(page.getByRole("heading", { level: 1, name: "New organization", exact: true })).toBeVisible();
  // The org settings frame, with New organization… picked and no sections.
  await expect(pick).toHaveValue("/orgs/_new");
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
  await expect(page_.getByText("https://git.initech.dev", { exact: true })).toBeVisible();
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
  await expect(pick.locator("option")).toHaveText(["New organization… (2 waiting)"]);
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
  await expect(pick.locator("option").last()).toHaveText("New organization… (2 waiting)");
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
  await expect(acmeRow.getByRole("combobox", { name: "Organization to assign" })).toHaveValue("acme");
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
  await expect(pick.locator("option").last()).toHaveText("New organization… (1 waiting)");

  // The other one joins umbrella, which a GitHub connection now serves.
  await pick.selectOption({ label: "New organization… (1 waiting)" });
  await expect(page).toHaveURL(/\/orgs\/_new$/);
  await expect(
    waiting.getByText("Scarab could not prove this account owns an organization of the same name, so it serves none until you choose."),
  ).toBeVisible();
  await acmeRow.getByRole("button", { name: "Assign to an organization" }).click();
  await acmeRow.getByRole("combobox", { name: "Organization to assign" }).selectOption("umbrella");
  await acmeRow.getByRole("button", { name: "Assign", exact: true }).click();
  await expect(page).toHaveURL(/\/orgs\/umbrella\/settings\/connections$/);
  await expect(page_.getByText("github-install-90211", { exact: true })).toBeVisible();
  await expect(pick.locator("option").last()).toHaveText("New organization…");

  // None wait, so the panel is gone.
  await pick.selectOption({ label: "New organization…" });
  await expect(page).toHaveURL(/\/orgs\/_new$/);
  await expect(page_.locator(".panel", { hasText: "First forge connection" })).toBeVisible();
  await expect(waiting).toHaveCount(0);
  expect(errors).toEqual([]);
});

test("a token minted under Account settings shows in the org's list", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/settings/tokens");
  await expect(page.getByRole("heading", { level: 1, name: "API tokens", exact: true })).toBeVisible();
  const page_ = page.locator("section.page");
  await expect(page_.getByText("release-bot", { exact: true })).toBeVisible();

  await page_.getByRole("button", { name: "New token" }).click();
  await page.getByRole("textbox", { name: "name" }).fill("e2e laptop");
  await page.getByRole("button", { name: "Mint token" }).click();
  await expect(page.getByText(/^scarab_pat_/)).toBeVisible();
  await page.getByRole("button", { name: "I've copied it" }).click();
  await expect(page_.getByText("e2e laptop", { exact: true })).toBeVisible();

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

test("Members lists org roles only and links each person's repository roles", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/orgs/acme/settings/members");
  const view = page.locator("section.page");
  const jordan = view.locator("li.tok", { hasText: "Jordan Lee" });
  await expect(jordan).toContainText("member all of acme");
  await expect(jordan).toContainText("also has roles in 2 repositories");
  await expect(jordan.getByRole("link", { name: "scarab" })).toHaveAttribute(
    "href",
    "/acme/scarab?tab=settings",
  );
  // Repository roles only: listed, with no org role and nothing to revoke here.
  const okafor = view.locator("li.tok", { hasText: "r.okafor" });
  await expect(okafor).toContainText("no org role");
  await expect(okafor).toContainText("not signed in yet");
  await expect(okafor.getByRole("button", { name: "revoke" })).toHaveCount(0);
  // Grants are org-wide, and re-sync lives on the repository.
  await expect(view.getByRole("combobox", { name: "scope" })).toHaveCount(0);
  await expect(view.getByRole("button", { name: /re-sync/i })).toHaveCount(0);

  await jordan.getByRole("link", { name: "scarab" }).click();
  await expect(page).toHaveURL(/\/acme\/scarab\?tab=settings$/);
  await expect(page.locator(".panel-h", { hasText: "Access" })).toBeVisible();
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
