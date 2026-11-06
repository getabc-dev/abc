// @vitest-environment jsdom

// The whole app in a DOM, over a stand-in server: the session gate, the claim, the shell and the
// live Machines screen.

import { focusManager, QueryClient } from "@tanstack/react-query";
import { createMemoryHistory } from "@tanstack/react-router";
import { act, cleanup, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, test, vi } from "vitest";

import { apiClient, type Fetch } from "./api/client";
import { App, createAppRouter } from "./app";
import type { LiveUpdate } from "./generated/LiveUpdate";
import type { SessionView } from "./generated/SessionView";
import { card, stubMatchMedia } from "./testing";

// The router restores scroll positions, which jsdom cannot do.
Reflect.set(window, "scrollTo", () => undefined);
// The shell's sidebar asks whether the window is phone-sized, which jsdom cannot say.
stubMatchMedia(false);

afterEach(cleanup);

const ORIGIN = "https://abc.example.ts.net";
const WORKSPACE = "0190f5a2-0000-7000-8000-000000000001";

const member: SessionView = {
  state: "member",
  person: "0190f5a2-0000-7000-8000-000000000002",
  display_name: "Ada Lovelace",
  workspaces: [{ id: WORKSPACE, name: "Home lab", role: "owner" }],
};

function json(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), { status });
}

function refusal(status: number, code: string, message: string): Response {
  return json(status, { error: { code, message } });
}

/** A live stream the test writes updates into, and can end. */
function stream() {
  const encoder = new TextEncoder();
  let control: ReadableStreamDefaultController<Uint8Array> | undefined;
  const body = new ReadableStream<Uint8Array>({
    start(controller) {
      control = controller;
    },
  });
  return {
    response: new Response(body, { status: 200 }),
    send(update: LiveUpdate) {
      control?.enqueue(
        encoder.encode(
          `event: ${update.type}\nid: ${update.head}\ndata: ${JSON.stringify(update)}\n\n`,
        ),
      );
    },
    end() {
      control?.close();
    },
  };
}

type Request = { readonly method: string; readonly path: string; readonly init: RequestInit };

/** A server answering each request with `answer`, which sees it; records every request. */
function server(answer: (request: Request) => Response | Promise<Response>) {
  const requests: Request[] = [];
  const fetcher: Fetch = (path, init) => {
    const request = { method: init.method ?? "GET", path, init };
    requests.push(request);
    return Promise.resolve(answer(request));
  };
  return { fetcher, requests };
}

function mount(path: string, fetcher: Fetch) {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const router = createAppRouter(
    { api: apiClient(fetcher), queryClient, origin: ORIGIN },
    createMemoryHistory({ initialEntries: [path] }),
  );
  render(<App router={router} queryClient={queryClient} />);
  return router;
}

test("an unclaimed server sends every route to the claim, which lands on home", async () => {
  let claimed = false;
  const live = stream();
  const { fetcher, requests } = server(({ method, path }) => {
    if (path === "/api/session") {
      return json(200, claimed ? member : { state: "unclaimed", login: "ada@example.com" });
    }
    if (method === "POST" && path === "/api/owner/claim") {
      claimed = true;
      return json(200, { owner: "0190f5a2-0000-7000-8000-000000000002", workspace: WORKSPACE });
    }
    return live.response;
  });
  const router = mount("/somewhere/else", fetcher);
  await screen.findByRole("heading", { level: 1, name: "Claim this server" });
  expect(screen.getByText("ada@example.com")).toBeDefined();
  await userEvent.type(screen.getByRole("textbox", { name: "Setup code" }), "abcd-efgh");
  await userEvent.type(screen.getByRole("textbox", { name: "Your name" }), "Ada Lovelace");
  await userEvent.type(screen.getByRole("textbox", { name: "Workspace name" }), "Home lab");
  await userEvent.click(screen.getByRole("button", { name: "Claim this server" }));
  await screen.findByRole("heading", { level: 1, name: "Machines" });
  expect(router.state.location.pathname).toBe("/machines");
  const claim = requests.find((request) => request.method === "POST");
  expect(claim?.init.body).toBe(
    JSON.stringify({ code: "abcd-efgh", display_name: "Ada Lovelace", workspace_name: "Home lab" }),
  );
  live.send({ type: "snapshot", head: 4, machines: [] });
  await screen.findByRole("heading", { name: "Enrol your first machine" });
});

test("a wrong code is said on the code field, and an expired one has its own screen", async () => {
  const answers = [
    refusal(403, "setup_code_mismatch", "the setup code does not match"),
    refusal(403, "setup_code_expired", "the setup code expired"),
  ];
  const { fetcher } = server(({ path }) =>
    path === "/api/session"
      ? json(200, { state: "unclaimed", login: "ada@example.com" })
      : (answers.shift() ?? refusal(500, "internal", "asked too often")),
  );
  mount("/", fetcher);
  const code = await screen.findByRole("textbox", { name: "Setup code" });
  await userEvent.type(code, "wrong");
  await userEvent.type(screen.getByRole("textbox", { name: "Your name" }), "Ada");
  await userEvent.type(screen.getByRole("textbox", { name: "Workspace name" }), "Home");
  await userEvent.click(screen.getByRole("button", { name: "Claim this server" }));
  await screen.findByText(/not the server's current setup code/);
  expect(code.getAttribute("aria-invalid")).toBe("true");
  await userEvent.click(screen.getByRole("button", { name: "Claim this server" }));
  await screen.findByRole("heading", { level: 1, name: "This setup code has expired" });
  await userEvent.click(screen.getByRole("button", { name: "Enter a new code" }));
  expect(screen.getByRole("textbox", { name: "Setup code" })).toHaveProperty("value", "");
  expect(screen.getByRole("textbox", { name: "Your name" })).toHaveProperty("value", "Ada");
});

test("a server claimed meanwhile has its own screen, and continuing asks again", async () => {
  let asked = 0;
  const live = stream();
  const { fetcher } = server(({ path }) => {
    if (path === "/api/session") {
      asked += 1;
      return json(200, asked === 1 ? { state: "unclaimed", login: "ada@example.com" } : member);
    }
    if (path === "/api/owner/claim") {
      return refusal(409, "server_already_claimed", "the server is already claimed");
    }
    return live.response;
  });
  mount("/", fetcher);
  await userEvent.type(await screen.findByRole("textbox", { name: "Setup code" }), "x");
  await userEvent.type(screen.getByRole("textbox", { name: "Your name" }), "Ada");
  await userEvent.type(screen.getByRole("textbox", { name: "Workspace name" }), "Home");
  await userEvent.click(screen.getByRole("button", { name: "Claim this server" }));
  await screen.findByRole("heading", { level: 1, name: "This server already has an owner" });
  await userEvent.click(screen.getByRole("button", { name: "Continue" }));
  expect(await screen.findByRole("heading", { level: 1, name: "Machines" })).toBeDefined();
  expect(asked).toBe(2);
});

test("visitors who are not members are told why", async () => {
  mount("/", server(() => json(200, { state: "no_identity" })).fetcher);
  await screen.findByRole("heading", { level: 1, name: "No tailnet identity" });
  cleanup();
  mount("/", server(() => json(200, { state: "not_a_member", login: "bob@example.com" })).fetcher);
  await screen.findByRole("heading", { level: 1, name: "Not a member" });
  expect(screen.getByText(/bob@example.com belongs to no workspace/)).toBeDefined();
  cleanup();
  mount("/", server(() => json(200, { ...member, workspaces: [] })).fetcher);
  await screen.findByRole("heading", { level: 1, name: "No workspace" });
});

test("a session that fails is shown with a retry", async () => {
  const first = Promise.withResolvers<Response>();
  const answers = [first.promise, Promise.resolve(json(200, { state: "no_identity" }))];
  mount("/", server(() => answers.shift() ?? refusal(500, "internal", "again")).fetcher);
  expect((await screen.findByRole("status")).textContent).toBe("Asking the server who you are…");
  first.resolve(refusal(503, "unavailable", "the database is down"));
  await screen.findByRole("heading", { level: 1, name: "The server did not answer" });
  expect(screen.getByText("the database is down")).toBeDefined();
  await userEvent.click(screen.getByRole("button", { name: "Try again" }));
  await screen.findByRole("heading", { level: 1, name: "No tailnet identity" });
});

test("a failed refetch of the session keeps the shell and its stream, with a notice", async () => {
  const live = stream();
  const sessions: Response[] = [
    refusal(503, "unavailable", "the database is down"),
    json(200, member),
  ];
  let asked = 0;
  const { fetcher, requests } = server(({ path }) => {
    if (path === "/api/session") {
      asked += 1;
      return asked === 1 ? json(200, member) : (sessions.shift() ?? json(200, member));
    }
    return live.response;
  });
  mount("/machines", fetcher);
  live.send({ type: "snapshot", head: 1, machines: [card("lab box", "w1", true)] });
  await screen.findByRole("region", { name: "lab box" });
  // Coming back to the window asks again; this time the server fails.
  act(() => {
    focusManager.setFocused(false);
    focusManager.setFocused(true);
  });
  const notice = await screen.findByText(/could not check your session/);
  expect(notice.closest("output")?.textContent).toContain("the database is down");
  expect(screen.getByRole("region", { name: "lab box" })).toBeDefined();
  expect(screen.queryByRole("heading", { name: "The server did not answer" })).toBeNull();
  await userEvent.click(screen.getByRole("button", { name: "Check again" }));
  await vi.waitFor(() => {
    expect(screen.queryByText(/could not check your session/)).toBeNull();
  });
  expect(screen.getByRole("region", { name: "lab box" })).toBeDefined();
  expect(asked).toBe(3);
  expect(requests.filter((request) => request.path.endsWith("/live"))).toHaveLength(1);
  focusManager.setFocused(undefined);
});

test("machines appear live in the shell and on their screen, and go offline", async () => {
  const live = stream();
  const { fetcher, requests } = server(({ path }) =>
    path === "/api/session" ? json(200, member) : live.response,
  );
  mount("/", fetcher);
  await screen.findByText("Loading machines…");
  expect(screen.getByRole("banner").textContent).toContain("Home lab");
  live.send({ type: "snapshot", head: 4, machines: [card("lab box", "w1", true)] });
  const region = await screen.findByRole("region", { name: "lab box" });
  expect(within(region).getByText("Online")).toBeDefined();
  expect(within(region).getByText("Linux on x86-64")).toBeDefined();
  expect(within(region).getByText("lab box.local")).toBeDefined();
  expect(
    within(screen.getByRole("list", { name: "Machines" })).getAllByRole("listitem")[0]?.textContent,
  ).toBe("lab box, online");
  expect(screen.getByText("Live")).toBeDefined();
  live.send({
    type: "machines",
    head: 6,
    changed: [{ ...card("lab box", "w1", false), last_seen_at: "2026-11-06T09:30:00Z" }],
  });
  await within(region).findByText("Offline");
  expect(within(region).queryByText("Never connected")).toBeNull();
  expect(requests.filter((request) => request.path.endsWith("/live"))).toHaveLength(1);
  expect(requests.at(-1)?.path).toBe(`/api/workspaces/${WORKSPACE}/live`);
});

test("a dropped stream shows the page offline and resumes where it stopped", async () => {
  const first = stream();
  const second = stream();
  const streams = [first, second];
  const { fetcher, requests } = server(({ path }) =>
    path === "/api/session" ? json(200, member) : (streams.shift() ?? stream()).response,
  );
  mount("/machines", fetcher);
  first.send({ type: "snapshot", head: 4, machines: [card("lab box", "w1", true)] });
  await screen.findByRole("region", { name: "lab box" });
  act(() => {
    first.end();
  });
  await screen.findByText(/Offline: the connection to the server dropped/);
  expect(screen.getByText("Reconnecting")).toBeDefined();
  // The resume waits half a second.
  second.send({ type: "machines", head: 7, changed: [card("laptop", "w2", true)] });
  await screen.findByRole("region", { name: "laptop" }, { timeout: 3000 });
  expect(screen.queryByText(/Offline: the connection/)).toBeNull();
  const resumed = requests.filter((request) => request.path.endsWith("/live"))[1];
  expect(resumed?.init.headers).toEqual({ accept: "text/event-stream", "last-event-id": "4" });
});

test("a stream refused before any card is an error with a retry", async () => {
  const answers = [refusal(403, "not_a_member", "you are not a member of this workspace")];
  const live = stream();
  const { fetcher } = server(({ path }) =>
    path === "/api/session" ? json(200, member) : (answers.shift() ?? live.response),
  );
  mount("/machines", fetcher);
  await screen.findByRole("heading", { name: "The machines could not be loaded" });
  expect(screen.getByText("you are not a member of this workspace")).toBeDefined();
  expect(screen.getByText("Stopped")).toBeDefined();
  await userEvent.click(screen.getByRole("button", { name: "Try again" }));
  live.send({ type: "snapshot", head: 1, machines: [card("lab box", "w1", true)] });
  await screen.findByRole("region", { name: "lab box" });
});

test("ended access keeps the cards, says so and offers a retry", async () => {
  const live = stream();
  mount(
    "/machines",
    server(({ path }) => (path === "/api/session" ? json(200, member) : live.response)).fetcher,
  );
  live.send({ type: "snapshot", head: 1, machines: [card("lab box", "w1", true)] });
  await screen.findByRole("region", { name: "lab box" });
  live.send({ type: "access_ended", head: 1 });
  const alert = await screen.findByRole("alert");
  expect(alert.textContent).toContain("Your access to this workspace ended.");
  expect(within(alert).getByRole("button", { name: "Try again" })).toBeDefined();
});

test("enrolling shows the commands with the server's URL and the token once", async () => {
  const live = stream();
  const { fetcher, requests } = server(({ method, path }) => {
    if (path === "/api/session") {
      return json(200, member);
    }
    if (method === "POST") {
      return json(200, {
        id: "t1",
        token: "abc_enr_secret",
        expires_at: "2026-11-06T10:00:00Z",
      });
    }
    return live.response;
  });
  mount("/machines", fetcher);
  live.send({ type: "snapshot", head: 1, machines: [card("lab box", "w1", false)] });
  await userEvent.click(await screen.findByRole("button", { name: "Enrol a machine" }));
  await userEvent.type(screen.getByRole("textbox", { name: "Machine name" }), "laptop");
  await userEvent.click(screen.getByRole("button", { name: "Create enrolment token" }));
  await screen.findByText("abc_enr_secret");
  expect(screen.getByText(`abc worker enrol ${ORIGIN}`)).toBeDefined();
  expect(screen.getByText("abc worker")).toBeDefined();
  const issue = requests.find((request) => request.method === "POST");
  expect(issue?.path).toBe(`/api/workspaces/${WORKSPACE}/enrolment-tokens`);
  expect(issue?.init.body).toBe(JSON.stringify({ label: "laptop" }));
  await userEvent.click(screen.getByRole("button", { name: "Done" }));
  expect(screen.queryByText("abc_enr_secret")).toBeNull();
  await userEvent.click(screen.getByRole("button", { name: "Enrol a machine" }));
  await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
  expect(screen.queryByRole("heading", { name: "Enrol a machine" })).toBeNull();
});

test("a revoked machine says so and leaves the shell's pills", async () => {
  const live = stream();
  mount(
    "/machines",
    server(({ path }) => (path === "/api/session" ? json(200, member) : live.response)).fetcher,
  );
  live.send({
    type: "snapshot",
    head: 1,
    machines: [{ ...card("old box", "w1", false), revoked: true }, card("mac", "w2", true)],
  });
  const region = await screen.findByRole("region", { name: "old box" });
  expect(within(region).getByText("Revoked")).toBeDefined();
  expect(within(region).getByText("Never connected")).toBeDefined();
  const pills = screen.getAllByRole("list", { name: "Machines" })[0];
  expect(
    pills &&
      within(pills)
        .getAllByRole("listitem")
        .map((item) => item.textContent),
  ).toEqual(["mac, online"]);
  expect(screen.getByText("1 of 1 online")).toBeDefined();
});

test("an unknown path says there is no page there, inside the shell, and links home", async () => {
  const live = stream();
  mount(
    "/nowhere",
    server(({ path }) => (path === "/api/session" ? json(200, member) : live.response)).fetcher,
  );
  await screen.findByRole("heading", { level: 1, name: "No page here" });
  expect(screen.getByRole("navigation", { name: "Main" })).toBeDefined();
  await userEvent.click(screen.getByRole("link", { name: "Go home" }));
  await screen.findByRole("heading", { level: 1, name: "Machines" });
});

test("a workspace with only revoked machines offers the first enrolment", async () => {
  const live = stream();
  mount(
    "/machines",
    server(({ path }) => (path === "/api/session" ? json(200, member) : live.response)).fetcher,
  );
  live.send({
    type: "snapshot",
    head: 1,
    machines: [{ ...card("old box", "w1", false), revoked: true }],
  });
  await screen.findByRole("heading", { name: "Enrol your first machine" });
  expect(screen.getByText("No machines")).toBeDefined();
  expect(screen.getByRole("region", { name: "old box" })).toBeDefined();
});
