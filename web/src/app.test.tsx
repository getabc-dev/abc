// @vitest-environment jsdom

// The app's routes, rendered in a DOM over a stand-in server.

import { QueryClient } from "@tanstack/react-query";
import { createMemoryHistory } from "@tanstack/react-router";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, expect, test } from "vitest";

import { apiClient, type Fetch } from "./api/client";
import { App, createAppRouter } from "./app";

// React warns when updates happen outside act() unless it knows it is under test.
Reflect.set(globalThis, "IS_REACT_ACT_ENVIRONMENT", true);
// The router restores scroll positions, which jsdom cannot do.
Reflect.set(window, "scrollTo", () => undefined);

const mounted: { root: Root; container: HTMLElement }[] = [];

afterEach(() => {
  for (const { root, container } of mounted.splice(0)) {
    act(() => root.unmount());
    container.remove();
  }
});

/** Mounts the app at `path`, asking `fetcher` for the API. */
function mount(path: string, fetcher: Fetch): HTMLElement {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const router = createAppRouter(
    { api: apiClient(fetcher), queryClient },
    createMemoryHistory({ initialEntries: [path] }),
  );
  const container = document.createElement("div");
  document.body.append(container);
  const root = createRoot(container);
  act(() => root.render(<App router={router} queryClient={queryClient} />));
  mounted.push({ root, container });
  return container;
}

/** Lets the app's promises run until `container` shows `text`, or fails naming what it shows. */
async function shows(container: HTMLElement, text: string): Promise<void> {
  for (let turn = 0; turn < 100; turn += 1) {
    if (container.textContent.includes(text)) {
      return;
    }
    // Each turn waits for the last: the page changes only as the app's promises settle.
    // oxlint-disable-next-line eslint/no-await-in-loop -- turns run one after another
    await act(() => new Promise((resolve) => setTimeout(resolve, 5)));
  }
  throw new Error(`expected "${text}", but the page shows "${container.textContent}"`);
}

function json(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), { status });
}

test("the home page shows who the server says the visitor is", async () => {
  const asked: string[] = [];
  const page = mount("/", (input) => {
    asked.push(input);
    return Promise.resolve(
      json(200, {
        state: "member",
        person: "0190f5a2-0000-7000-8000-000000000002",
        display_name: "Ada",
        workspaces: [],
      }),
    );
  });
  await shows(page, "Signed in as Ada");
  expect(page.querySelector("h1")?.textContent).toBe("Signed in as Ada");
  expect(page.textContent).toContain("You belong to 0 workspaces.");
  expect(asked).toEqual(["/api/session"]);
});

test("the home page says it is asking until the server answers", async () => {
  const answer = Promise.withResolvers<Response>();
  const page = mount("/", () => answer.promise);
  await shows(page, "Asking the server who you are");
  answer.resolve(json(200, { state: "no_identity" }));
  await shows(page, "No tailnet identity");
  expect(page.textContent).not.toContain("Asking the server");
});

test("a failed answer is shown, and Try again asks once more", async () => {
  const answers = [
    json(503, { error: { code: "unavailable", message: "the database is down" } }),
    json(200, { state: "unclaimed", login: "ada@example.com" }),
  ];
  let asked = 0;
  const page = mount("/", () => {
    const response = answers[asked];
    asked += 1;
    return response === undefined
      ? Promise.reject(new Error("asked too often"))
      : Promise.resolve(response);
  });
  await shows(page, "The server did not answer");
  expect(page.textContent).toContain("the database is down");
  const retry = page.querySelector("button");
  expect(retry?.textContent).toBe("Try again");
  act(() => retry?.click());
  await shows(page, "This server is unclaimed");
  expect(asked).toBe(2);
});

test("an unknown path says there is no page there and links home", async () => {
  const page = mount("/nowhere", () => Promise.reject(new Error("not asked")));
  await shows(page, "No page here");
  expect(page.querySelector("a")?.getAttribute("href")).toBe("/");
});
