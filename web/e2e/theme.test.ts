// The theme is set before the first paint: public/theme.js runs before the app, so with the app's
// script held back, the page already shows in the theme it will keep. It follows the operating
// system until a person chooses, and a kept choice wins over the system's scheme.

import type { Page } from "@playwright/test";

import { expect, test } from "./abc.ts";

/** DESIGN.md's `--background`, as the browser computes it. */
const BACKGROUND = { light: "rgb(247, 247, 245)", dark: "rgb(22, 22, 21)" };

/** Loads the page with the app's own script held back, so only what runs before it has run. */
async function loadWithoutTheApp(page: Page): Promise<void> {
  await page.route(/\/assets\/index-[0-9a-f]+\.js$/u, () => {
    // Never answered: the app does not start.
  });
  await page.goto("/", { waitUntil: "commit" });
  await expect.poll(() => page.evaluate(() => document.readyState)).not.toBe("loading");
  expect(await page.locator("#root").innerHTML()).toBe("");
}

async function shown(page: Page) {
  return page.evaluate(() => ({
    dark: document.documentElement.classList.contains("dark"),
    choice: document.documentElement.getAttribute("data-theme"),
    background: getComputedStyle(document.documentElement).backgroundColor,
  }));
}

test.afterEach(async ({ page }) => {
  await page.unrouteAll({ behavior: "ignoreErrors" });
});

test("with no choice, the page is in the system's dark scheme before the app runs", async ({
  page,
}) => {
  await page.emulateMedia({ colorScheme: "dark" });
  await loadWithoutTheApp(page);
  expect(await shown(page)).toEqual({ dark: true, choice: "system", background: BACKGROUND.dark });
});

test("a kept choice wins over the system's scheme before the app runs", async ({ page }) => {
  await page.addInitScript(() => {
    window.localStorage.setItem("abc-theme", "light");
  });
  await page.emulateMedia({ colorScheme: "dark" });
  await loadWithoutTheApp(page);
  expect(await shown(page)).toEqual({
    dark: false,
    choice: "light",
    background: BACKGROUND.light,
  });
});

// The fixture fails a test on any console error, such as a script the CSP refused.
test("the theme script loads under the CSP, and the app starts in its theme", async ({ page }) => {
  await page.emulateMedia({ colorScheme: "dark" });
  await page.goto("/");
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("Claim this server");
  expect(await shown(page)).toEqual({ dark: true, choice: "system", background: BACKGROUND.dark });
});
