import { describe, expect, test } from "vitest";

import { ApiError, apiClient, type Fetch } from "./client";

/** A server that answers every request with `status` and `body`, recording what it was asked. */
function answering(status: number, body: string) {
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

describe("session", () => {
  test("asks GET /api/session for JSON and returns the view", async () => {
    const server = answering(200, JSON.stringify({ state: "unclaimed", login: "ada@example.com" }));
    const view = await apiClient(server.fetcher).session();
    expect(view).toEqual({ state: "unclaimed", login: "ada@example.com" });
    expect(server.requests).toHaveLength(1);
    expect(server.requests[0]?.input).toBe("/api/session");
    expect(server.requests[0]?.init.method).toBe("GET");
    expect(server.requests[0]?.init.headers).toEqual({ accept: "application/json" });
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
