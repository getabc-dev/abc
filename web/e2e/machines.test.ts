// The Machines screen end to end, on the claimed server: a real `abc worker` enrols with a token
// from the screen and its card appears with no reload; it goes online when the worker starts and
// offline when it stops; and when the server restarts, the page resumes the stream from where it
// stopped, without reloading.

import type { Page } from "@playwright/test";

import { expect, Machine, ORIGIN, test } from "./abc.ts";
import { capture } from "./look.ts";

const PHONE = { width: 390, height: 844 };
const DESKTOP = { width: 1440, height: 900 };

/** Long enough for a worker to connect, or the stream to resume after a restart. */
const LIVE = { timeout: 30_000 };

async function shoot(page: Page, name: string): Promise<void> {
  await page.setViewportSize(DESKTOP);
  await capture(page, name);
  await page.setViewportSize(PHONE);
  await capture(page, `${name}-phone`);
  await page.setViewportSize(DESKTOP);
}

test("a machine enrols, comes online, goes offline, and the page resumes after a restart", async ({
  page,
  server,
}) => {
  test.setTimeout(120_000);
  const streams: (string | undefined)[] = [];
  page.on("request", (request) => {
    if (request.url().endsWith("/live")) {
      streams.push(request.headers()["last-event-id"]);
    }
  });
  await page.goto("/");
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("Machines");
  await expect(page.getByRole("heading", { name: "Enrol your first machine" })).toBeVisible();
  await expect(page.getByText("Live", { exact: true })).toBeVisible();
  await shoot(page, "machines-empty");

  await page.getByRole("textbox", { name: "Machine name" }).fill("e2e machine");
  await page.getByRole("button", { name: "Create enrolment token" }).click();
  await expect(page.getByText(`abc worker enrol ${ORIGIN}`)).toBeVisible();
  const token = (await page.locator("code", { hasText: /^abc_enr_/ }).textContent()) ?? "";
  expect(token).toMatch(/^abc_enr_/);
  await shoot(page, "machines-enrol-token");
  // Marks this page load: a reload would lose it.
  await page.evaluate(() => {
    Reflect.set(window, "abcE2eLoad", "first");
  });

  const machine = new Machine("e2e-host");
  expect(await machine.enrol(token)).toContain("Enrolled this machine as e2e machine");
  const card = page.getByRole("region", { name: "e2e machine" });
  await expect(card).toBeVisible();
  await expect(card.getByText("Never connected")).toBeVisible();
  await expect(card.getByText("e2e-host")).toBeVisible();
  await page.getByRole("button", { name: "Done" }).click();

  machine.start();
  await expect(card.getByText("Online", { exact: true })).toBeVisible(LIVE);
  await expect(page.getByRole("list", { name: "Machines", exact: true })).toHaveText(
    "e2e machine, online",
  );
  await shoot(page, "machines-live");

  await machine.stop();
  await expect(card.getByText("Offline", { exact: true })).toBeVisible(LIVE);
  await expect(page.getByRole("list", { name: "Machines", exact: true })).toHaveText(
    "e2e machine, offline",
  );
  await shoot(page, "machines-offline");

  await server.stop();
  await expect(page.getByText(/^Offline: the connection to the server dropped/)).toBeVisible(LIVE);
  await expect(page.getByText("Reconnecting", { exact: true })).toBeVisible();
  await capture(page, "machines-reconnecting");
  await server.start();
  await expect(page.getByText("Live", { exact: true })).toBeVisible(LIVE);
  await expect(page.getByText(/^Offline: the connection/)).toBeHidden();
  // Resumed from the last position it saw, on the same page load.
  expect(streams[0]).toBeUndefined();
  expect(streams.at(-1)).toMatch(/^\d+$/);
  expect(await page.evaluate(() => String(Reflect.get(window, "abcE2eLoad")))).toBe("first");

  // The resumed stream is live: the worker's return shows without a reload.
  machine.start();
  await expect(card.getByText("Online", { exact: true })).toBeVisible(LIVE);
  await machine.stop();
});

test("the shell's theme menu chooses Dark over a light system, kept across a reload", async ({
  page,
}) => {
  await page.setViewportSize(PHONE);
  await page.emulateMedia({ colorScheme: "light" });
  await page.goto("/");
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("Machines");
  const dark = () => page.evaluate(() => document.documentElement.classList.contains("dark"));
  expect(await dark()).toBe(false);

  await page.getByRole("button", { name: "Theme: System" }).focus();
  await page.keyboard.press("Enter");
  const menu = page.getByRole("menu");
  await expect(menu.getByRole("menuitemradio")).toHaveText(["Light", "Dark", "System"]);
  // The open menu's items are touch targets too, in both themes.
  await capture(page, "theme-menu-phone");
  await menu.getByRole("menuitemradio", { name: "Dark" }).click();
  await expect(page.getByRole("button", { name: "Theme: Dark" })).toBeFocused();
  expect(await dark()).toBe(true);

  await page.reload();
  await expect(page.getByRole("button", { name: "Theme: Dark" })).toBeVisible();
  expect(await dark()).toBe(true);

  await page.getByRole("button", { name: "Theme: Dark" }).click();
  await page.getByRole("menuitemradio", { name: "System" }).click();
  await expect(page.getByRole("button", { name: "Theme: System" })).toBeVisible();
  expect(await dark()).toBe(false);
});
