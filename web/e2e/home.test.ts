// The app on an unclaimed server, loaded from the real abc binary over a fresh database.

import type { Page } from "@playwright/test";

import { expect, test } from "./abc.ts";
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

test("an unclaimed server shows the claim, naming who would own it", async ({ page }) => {
  const seen = errors(page);
  const response = await page.goto("/");
  expect(response?.status()).toBe(200);
  expect(response?.headers()["cache-control"]).toBe("no-cache");
  expect(response?.headers()["content-security-policy"]).toContain("default-src 'self'");
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("Claim this server");
  await expect(page.getByText(LOGIN)).toBeVisible();
  // The self-hosted fonts load under the CSP.
  expect(
    await page.evaluate(async () => (await document.fonts.ready).check("13px 'Inter Variable'")),
  ).toBe(true);
  expect(seen).toEqual([]);
});

test("every path of an unclaimed server leads to the claim", async ({ page }) => {
  const seen = errors(page);
  const response = await page.goto("/nowhere");
  expect(response?.status()).toBe(200);
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("Claim this server");
  expect(seen).toEqual([]);
});

test("the API answers its own paths, never with the app", async ({ request }) => {
  const response = await request.get("/api/nowhere");
  expect(response.status()).toBe(404);
  expect(await response.json()).toMatchObject({ error: { code: "not_found" } });
});
