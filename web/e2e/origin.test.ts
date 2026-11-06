// The server's origin check, end to end: the browser loads the app from the server's public
// origin, so a state-changing request the app makes carries the server's own `Origin`, which the
// stand-in passes on unchanged, as Serve does. `POST /api/owner/claim` is the state-changing route.

import { expect, test } from "./abc.ts";

/** A well-formed claim. No setup code was issued, so the claim itself refuses it. */
const claim = {
  code: "0123456789ABCDEFGHJK",
  display_name: "End to end",
  workspace_name: "e2e",
};

test("a state-changing request from the app's origin passes the origin check", async ({ page }) => {
  await page.goto("/");
  const answer = await page.evaluate(async (body) => {
    const response = await fetch("/api/owner/claim", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    return { status: response.status, body: (await response.json()) as unknown };
  }, claim);
  // The claim ran and refused for its own reason: no setup code exists yet.
  expect(answer).toEqual({
    status: 401,
    body: { error: expect.objectContaining({ code: "setup_code_required" }) },
  });
});

test("a state-changing request from another origin is refused", async ({ request }) => {
  const response = await request.post("/api/owner/claim", {
    headers: { origin: "https://elsewhere.example" },
    data: claim,
  });
  expect(response.status()).toBe(403);
  expect(await response.json()).toMatchObject({ error: { code: "cross_site_request" } });
});
