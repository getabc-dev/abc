// The home page, loaded from the real abc binary over a fresh database.

import { expect, type Page, test } from "@playwright/test";

import { LOGIN } from "./identity.ts";

/** Collects the page's console errors and uncaught exceptions, such as a refused script. */
function errors(page: Page): string[] {
  const seen: string[] = [];
  page.on("console", (message) => {
    if (message.type() === "error") {
      seen.push(message.text());
    }
  });
  page.on("pageerror", (error) => {
    seen.push(error.message);
  });
  return seen;
}

test("the home page shows who the server says the visitor is", async ({ page }) => {
  const seen = errors(page);
  const response = await page.goto("/");
  expect(response?.status()).toBe(200);
  expect(response?.headers()["cache-control"]).toBe("no-cache");
  expect(response?.headers()["content-security-policy"]).toContain("default-src 'self'");
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("This server is unclaimed");
  await expect(page.getByText(`You are signed in as ${LOGIN}.`)).toBeVisible();
  expect(seen).toEqual([]);
});

test("a path of the router loads the app, which says there is no page there", async ({ page }) => {
  const seen = errors(page);
  const response = await page.goto("/nowhere");
  expect(response?.status()).toBe(200);
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("No page here");
  await page.getByRole("link", { name: "Go home" }).click();
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("This server is unclaimed");
  expect(seen).toEqual([]);
});

test("the API answers its own paths, never with the app", async ({ request }) => {
  const response = await request.get("/api/nowhere");
  expect(response.status()).toBe(404);
  expect(await response.json()).toMatchObject({ error: { code: "not_found" } });
});
