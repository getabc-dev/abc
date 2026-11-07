// Claiming the server end to end: the host prints a setup code, or `abc server setup` opens a link
// carrying it, and the person becomes the owner in the browser. A wrong code is refused on its
// field, and a second tab that claims after the first is told the server already has an owner.

import type { Page } from "@playwright/test";

import { expect, test } from "./abc.ts";
import { capture } from "./look.ts";

const PHONE = { width: 390, height: 844 };

/** Opens the claim in `tab` and fills it in with `code`. */
async function fill(tab: Page, code: string): Promise<void> {
  await tab.goto("/");
  await tab.getByRole("textbox", { name: "Setup code" }).fill(code);
  await tab.getByRole("textbox", { name: "Your name" }).fill("Ada Lovelace");
  await tab.getByRole("textbox", { name: "Workspace name" }).fill("Home lab");
}

test("a wrong code is refused on the code field", async ({ page, server }) => {
  // A code exists, so the server compares, and this is not it.
  await server.setupCode();
  await page.goto("/");
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("Claim this server");
  await capture(page, "claim");
  await page.setViewportSize(PHONE);
  await capture(page, "claim-phone");
  await fill(page, "AAAA-BBBB-CCCC-DDDD-EEEE");
  await page.getByRole("button", { name: "Claim this server" }).click();
  const code = page.getByRole("textbox", { name: "Setup code" });
  await expect(code).toHaveAttribute("aria-invalid", "true");
  await expect(page.getByText("This is not the server's current setup code.")).toBeVisible();
  await capture(page, "claim-wrong-code-phone");
});

test("the host's code claims the server and lands on home; a second tab learns it is taken", async ({
  page,
  context,
  server,
}) => {
  const code = await server.setupCode();
  const late = await context.newPage();
  await Promise.all([fill(page, code), fill(late, code)]);
  await page.getByRole("button", { name: "Claim this server" }).click();
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("Machines");
  expect(new URL(page.url()).pathname).toBe("/machines");
  await expect(page.getByRole("banner")).toContainText("Home lab");
  await expect(page.getByRole("banner")).toContainText("Signed in as Ada Lovelace");

  await late.getByRole("button", { name: "Claim this server" }).click();
  await expect(late.getByRole("heading", { level: 1 })).toHaveText(
    "This server already has an owner",
  );
  await capture(late, "claim-already-claimed");
  await late.getByRole("button", { name: "Continue" }).click();
  await expect(late.getByRole("heading", { level: 1 })).toHaveText("Machines");
});

test("the claim link abc server setup opens fills the code in and leaves no trace of it", async ({
  page,
  server,
}) => {
  const code = await server.setupCode();
  await page.goto(`/#code=${code}`);
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("Claim this server");
  await expect(page.getByRole("textbox", { name: "Setup code" })).toHaveValue(code);
  expect(new URL(page.url()).hash).toBe("");
  await page.getByRole("textbox", { name: "Your name" }).fill("Ada Lovelace");
  await page.getByRole("textbox", { name: "Workspace name" }).fill("Home lab");
  await page.getByRole("button", { name: "Claim this server" }).click();
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("Machines");
});
