// `abc worker setup` end to end: a real worker asks the real server to enrol, the owner approves
// in the browser on a page showing the same code as the terminal, the machine's card goes Online,
// the worker's service restarts and it is Online again, and `abc worker uninstall` takes it away.
// The service managers, root and browser are fakes that run what the service definition says
// (crates/abc-host/tests/fakes); a real reboot is the maintainer's check on a real machine.

import { expect, test } from "./abc.ts";
import { capture } from "./look.ts";

/** Long enough for a worker to connect after being (re)started. */
const LIVE = { timeout: 30_000 };

test("a machine is approved in the browser, runs as a service and survives a restart", async ({
  page,
  machine: makeMachine,
}) => {
  test.setTimeout(180_000);
  await page.goto("/");
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("Machines");
  // The one command, with a Copy button, for any machine.
  await page.getByRole("button", { name: "Enrol a machine" }).click();
  await expect(page.getByText("abc worker setup https://", { exact: false })).toBeVisible();
  await page.getByRole("button", { name: "Copy the setup command" }).click();
  await expect(
    page.getByRole("status").filter({ hasText: "Copied the setup command" }),
  ).toBeAttached();

  const machine = makeMachine("setup-host");
  const setup = machine.setup();
  const { page: approve, code } = await setup.approval();
  expect(approve).toContain("/machines/requests/");

  // Machines shows a notice for the pending machine, with the code, live and with no reload.
  const notice = page.getByRole("alert").filter({ hasText: "setup-host wants to join" });
  await expect(notice).toBeVisible();
  await expect(notice.getByText(code)).toBeVisible();
  await capture(page, "machines-request");
  await notice.getByRole("link", { name: "Review request" }).click();

  // The Approve page shows the machine, who asked from which node, and the terminal's own code.
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("setup-host wants to join");
  await expect(page.getByText(code, { exact: true })).toBeVisible();
  await expect(page.getByText("e2e@example.com")).toBeVisible();
  await expect(page.getByText("e2e-mac.tail1234.ts.net")).toBeVisible();
  await capture(page, "approve");
  await page.getByRole("button", { name: "Approve" }).click();
  await expect(page.getByText("Approved", { exact: true })).toBeVisible();
  await capture(page, "approved");

  // The terminal learns, and leaves a service running.
  expect(await setup.finished()).toBe(0);
  expect(setup.output).toContain("Approved: enrolled this machine as setup-host");
  expect(setup.output).toContain("runs as a service now");

  await page.getByRole("link", { name: "Back to Machines" }).click();
  const card = page.getByRole("region", { name: "setup-host" });
  await expect(card.getByText("Online", { exact: true })).toBeVisible(LIVE);
  expect((await machine.status()).code).toBe(0);

  // A restart of the service: the worker goes, and comes back by itself.
  await machine.restartService();
  await expect(card.getByText("Online", { exact: true })).toBeVisible(LIVE);

  // The notice went with the decision, and Uninstall stops the worker.
  await expect(notice).toBeHidden();
  expect(await machine.uninstall()).toContain("Removed abc worker's service.");
  await expect(card.getByText("Offline", { exact: true })).toBeVisible(LIVE);
  expect((await machine.status()).code).toBe(1);
});

test("a machine the owner denies is told so and is not enrolled", async ({
  page,
  machine: makeMachine,
}) => {
  test.setTimeout(120_000);
  const machine = makeMachine("denied-host");
  const setup = machine.setup();
  const { page: approve, code } = await setup.approval();
  await page.goto(new URL(approve).pathname);
  await expect(page.getByText(code, { exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Deny" }).click();
  await expect(page.getByText("Denied", { exact: true })).toBeVisible();
  expect(await setup.finished()).not.toBe(0);
  expect(setup.output).toContain("the owner denied this machine");
  await page.goto("/machines");
  await expect(page.getByRole("region", { name: "denied-host" })).toHaveCount(0);
});
