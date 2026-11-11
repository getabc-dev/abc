import { describe, expect, test } from "vitest";

import type { LiveUpdate } from "../generated/LiveUpdate";
import { ApiError, apiClient, type Fetch } from "./client";

/** A server that answers every request with `status` and `body`, recording what it was asked. */
function answering(status: number, body: BodyInit | null) {
  const requests: { input: string; init: RequestInit }[] = [];
  const fetcher: Fetch = (input, init) => {
    requests.push({ input, init });
    return Promise.resolve(new Response(body, { status }));
  };
  return { fetcher, requests };
}

/** A server that cannot be reached. */
const refusing: Fetch = () => Promise.reject(new TypeError("connection refused"));

/** A failure that is not an Error, as some fetch implementations throw. */
const offline: Fetch = () => Promise.reject("offline");

const WORKSPACE = "0190f5a2-0000-7000-8000-000000000001";

describe("session", () => {
  test("asks GET /api/session for JSON and returns the view", async () => {
    const server = answering(200, JSON.stringify({ state: "unclaimed", login: "ada@example.com" }));
    const view = await apiClient(server.fetcher).session();
    expect(view).toEqual({ state: "unclaimed", login: "ada@example.com" });
    expect(server.requests).toHaveLength(1);
    expect(server.requests[0]?.input).toBe("/api/session");
    expect(server.requests[0]?.init.method).toBe("GET");
    expect(server.requests[0]?.init.headers).toEqual({ accept: "application/json" });
    expect(server.requests[0]?.init.body).toBeUndefined();
  });

  test("passes the abort signal on", async () => {
    const server = answering(200, JSON.stringify({ state: "no_identity" }));
    const controller = new AbortController();
    await apiClient(server.fetcher).session(controller.signal);
    expect(server.requests[0]?.init.signal).toBe(controller.signal);
  });

  test("throws the API's error code and message", async () => {
    const server = answering(
      403,
      JSON.stringify({ error: { code: "cross_site_request", message: "another site" } }),
    );
    const failure = apiClient(server.fetcher).session();
    await expect(failure).rejects.toBeInstanceOf(ApiError);
    await expect(failure).rejects.toMatchObject({
      code: "cross_site_request",
      message: "another site",
      status: 403,
    });
  });

  test("an error without the API's body is internal and names the status", async () => {
    const server = answering(502, "<html>Bad gateway</html>");
    await expect(apiClient(server.fetcher).session()).rejects.toMatchObject({
      code: "internal",
      message: "the server answered 502",
      status: 502,
    });
  });

  test("an error body of the wrong shape is internal", async () => {
    const server = answering(500, JSON.stringify({ error: { code: 7 } }));
    await expect(apiClient(server.fetcher).session()).rejects.toMatchObject({
      code: "internal",
      status: 500,
    });
  });

  test("a success that is not JSON is internal", async () => {
    const server = answering(200, "not json");
    await expect(apiClient(server.fetcher).session()).rejects.toMatchObject({
      code: "internal",
      message: "the server's answer was not JSON",
      status: 200,
    });
  });

  test("an unreachable server is a network error", async () => {
    await expect(apiClient(refusing).session()).rejects.toMatchObject({
      code: "network",
      message: "the server could not be reached: connection refused",
      status: null,
    });
  });

  test("a failure that is not an Error is still a network error", async () => {
    await expect(apiClient(offline).session()).rejects.toMatchObject({
      code: "network",
      message: "the server could not be reached: offline",
    });
  });
});

describe("claim", () => {
  test("posts the claim as JSON and returns the answer", async () => {
    const answer = { owner: "p", workspace: "w" };
    const server = answering(200, JSON.stringify(answer));
    const request = { code: "abcd", display_name: "Ada", workspace_name: "Home" };
    expect(await apiClient(server.fetcher).claim(request)).toEqual(answer);
    expect(server.requests[0]?.input).toBe("/api/owner/claim");
    expect(server.requests[0]?.init.method).toBe("POST");
    expect(server.requests[0]?.init.headers).toEqual({
      accept: "application/json",
      "content-type": "application/json",
    });
    expect(server.requests[0]?.init.body).toBe(JSON.stringify(request));
  });

  test("throws the claim's refusal", async () => {
    const server = answering(
      403,
      JSON.stringify({ error: { code: "setup_code_mismatch", message: "no match" } }),
    );
    await expect(
      apiClient(server.fetcher).claim({ code: "x", display_name: "A", workspace_name: "B" }),
    ).rejects.toMatchObject({ code: "setup_code_mismatch", status: 403 });
  });
});

test("issueEnrolmentToken posts to the workspace's tokens", async () => {
  const answer = { id: "t", token: "abc_enr_x", expires_at: "2026-11-06T10:00:00Z" };
  const server = answering(200, JSON.stringify(answer));
  const issued = await apiClient(server.fetcher).issueEnrolmentToken(WORKSPACE, { label: "lab" });
  expect(issued).toEqual(answer);
  expect(server.requests[0]?.input).toBe(`/api/workspaces/${WORKSPACE}/enrolment-tokens`);
  expect(server.requests[0]?.init.method).toBe("POST");
  expect(server.requests[0]?.init.body).toBe(JSON.stringify({ label: "lab" }));
});

async function all(updates: AsyncIterable<LiveUpdate>): Promise<LiveUpdate[]> {
  const seen: LiveUpdate[] = [];
  for await (const update of updates) {
    seen.push(update);
  }
  return seen;
}

describe("live", () => {
  const snapshot: LiveUpdate = { type: "snapshot", head: 4, machines: [], requests: [] };
  const changed: LiveUpdate = { type: "machines", head: 6, changed: [], requests: [] };

  test("asks for the stream without a position and yields each update until it ends", async () => {
    const server = answering(
      200,
      `event: snapshot\nid: 4\ndata: ${JSON.stringify(snapshot)}\n\n: keep-alive\n\n` +
        `event: machines\nid: 6\ndata: ${JSON.stringify(changed)}\n\n`,
    );
    const signal = new AbortController().signal;
    const updates = await apiClient(server.fetcher).live(WORKSPACE, null, signal);
    expect(await all(updates)).toEqual([snapshot, changed]);
    expect(server.requests[0]?.input).toBe(`/api/workspaces/${WORKSPACE}/live`);
    expect(server.requests[0]?.init.headers).toEqual({ accept: "text/event-stream" });
    expect(server.requests[0]?.init.cache).toBe("no-store");
    expect(server.requests[0]?.init.signal).toBe(signal);
  });

  test("resumes after a position with Last-Event-ID", async () => {
    const server = answering(200, "");
    await all(await apiClient(server.fetcher).live(WORKSPACE, 41, new AbortController().signal));
    expect(server.requests[0]?.init.headers).toEqual({
      accept: "text/event-stream",
      "last-event-id": "41",
    });
  });

  test("throws the server's refusal", async () => {
    const server = answering(
      403,
      JSON.stringify({ error: { code: "not_a_member", message: "not yours" } }),
    );
    await expect(
      apiClient(server.fetcher).live(WORKSPACE, null, new AbortController().signal),
    ).rejects.toMatchObject({ code: "not_a_member", status: 403 });
  });

  test("an answer without a body is internal", async () => {
    const server = answering(200, null);
    await expect(
      apiClient(server.fetcher).live(WORKSPACE, null, new AbortController().signal),
    ).rejects.toMatchObject({ code: "internal", message: "the server's stream had no body" });
  });

  test("a connection that fails mid-stream is a network error", async () => {
    const encoder = new TextEncoder();
    const failing = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(encoder.encode(`data: ${JSON.stringify(snapshot)}\n\n`));
        controller.error(new TypeError("network error"));
      },
    });
    const server = answering(200, failing);
    const updates = await apiClient(server.fetcher).live(
      WORKSPACE,
      null,
      new AbortController().signal,
    );
    await expect(all(updates)).rejects.toMatchObject({
      code: "network",
      message: "the server could not be reached: network error",
    });
  });
});
