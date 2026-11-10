// The live machines program's rules, without a DOM.

import { describe, expect, test, vi } from "vitest";

import { type ApiClient, ApiError, apiClient } from "../api/client";
import type { LiveUpdate } from "../generated/LiveUpdate";
import { card, enrolmentRequest } from "../testing";
import { backoff, init, liveMachines, type Message, type Model, update } from "./live";

const WORKSPACE = "0190f5a2-0000-7000-8000-000000000001";

function received(live: LiveUpdate): Message {
  return { type: "received", update: live };
}

/** The model after `messages`, from the start. */
function replay(...messages: Message[]): Model {
  let [model] = init(WORKSPACE);
  for (const message of messages) {
    [model] = update(model, message);
  }
  return model;
}

const snapshot = received({
  type: "snapshot",
  requests: [],
  head: 5,
  machines: [card("zeta", "w2", true), card("alpha", "w1", false)],
});

const restarting = new ApiError("internal", "the server answered 502", 502);

describe("live machines", () => {
  test("starts connecting, asking for a snapshot", () => {
    const [model, effects] = init(WORKSPACE);
    expect(model.connection).toEqual({ state: "connecting" });
    expect(model.head).toBeNull();
    expect(effects).toEqual([{ type: "connect", workspace: WORKSPACE, after: null }]);
  });

  test("a snapshot shows every machine by name and goes live", () => {
    const model = replay(snapshot);
    expect(model.connection).toEqual({ state: "live" });
    expect(model.head).toBe(5);
    expect(model.machines.map((machine) => machine.name)).toEqual(["alpha", "zeta"]);
  });

  test("changes replace their machines' cards and add new ones", () => {
    const model = replay(
      snapshot,
      received({
        type: "machines",
        requests: [],
        head: 9,
        changed: [card("alpha", "w1", true), card("beta", "w3", true)],
      }),
    );
    expect(model.head).toBe(9);
    expect(model.machines.map((machine) => [machine.name, machine.online])).toEqual([
      ["alpha", true],
      ["beta", true],
      ["zeta", true],
    ]);
  });

  test("machines of one name are ordered by id", () => {
    const model = replay(
      received({
        type: "snapshot",
        requests: [],
        head: 1,
        machines: [card("a", "w9", true), card("a", "w1", true)],
      }),
    );
    expect(model.machines.map((machine) => machine.worker)).toEqual(["w1", "w9"]);
  });

  test("a drop after the cards resumes from their position, waiting longer each time", () => {
    const [dropped, wait] = update(replay(snapshot), { type: "ended" });
    expect(dropped.connection).toMatchObject({ state: "reconnecting", attempt: 1 });
    expect(wait).toEqual([{ type: "wait", ms: 500 }]);
    const [resuming, connect] = update(dropped, { type: "resume" });
    expect(connect).toEqual([{ type: "connect", workspace: WORKSPACE, after: 5 }]);
    const [again, longer] = update(resuming, { type: "failed", error: restarting });
    expect(again.connection).toEqual({
      state: "reconnecting",
      attempt: 2,
      problem: { code: "internal", message: "the server answered 502" },
    });
    expect(longer).toEqual([{ type: "wait", ms: 1000 }]);
    const live = replay(
      snapshot,
      { type: "ended" },
      { type: "resume" },
      received({
        type: "machines",
        requests: [],
        head: 6,
        changed: [],
      }),
    );
    expect(live.connection).toEqual({ state: "live" });
    expect(live.machines).toHaveLength(2);
  });

  test("the wait doubles from half a second up to eight", () => {
    expect([1, 2, 3, 4, 5, 6, 9].map(backoff)).toEqual([500, 1000, 2000, 4000, 8000, 8000, 8000]);
  });

  test("an unreachable or rate-limited server is retried too", () => {
    for (const error of [
      new ApiError("network", "unreachable", null),
      new ApiError("rate_limited", "slow down", 429),
    ]) {
      expect(update(replay(snapshot), { type: "failed", error })[0].connection.state).toBe(
        "reconnecting",
      );
    }
  });

  test("a refusal a person must answer stops, keeping the cards, until they retry", () => {
    const refused = new ApiError("not_a_member", "not a member", 403);
    const [failed, none] = update(replay(snapshot), { type: "failed", error: refused });
    expect(failed.connection).toEqual({
      state: "failed",
      problem: { code: "not_a_member", message: "not a member" },
    });
    expect(none).toEqual([]);
    expect(failed.machines).toHaveLength(2);
    const [retrying, connect] = update(failed, { type: "retry" });
    expect(retrying.connection).toEqual({ state: "connecting" });
    expect(connect).toEqual([{ type: "connect", workspace: WORKSPACE, after: 5 }]);
  });

  test("a first connection that fails waits for the person, whatever the reason", () => {
    const [failed, none] = update(replay(), { type: "failed", error: restarting });
    expect(failed.connection.state).toBe("failed");
    expect(none).toEqual([]);
    expect(update(replay(), { type: "ended" })[0].connection).toEqual({
      state: "failed",
      problem: { code: "network", message: "the server closed the connection" },
    });
    expect(update(failed, { type: "retry" })[1]).toEqual([
      { type: "connect", workspace: WORKSPACE, after: null },
    ]);
  });

  test("ended access stops the stream for good", () => {
    const stopped = replay(snapshot, received({ type: "access_ended", head: 5 }));
    expect(stopped.connection).toEqual({
      state: "failed",
      problem: { code: "access_ended", message: "Your access to this workspace ended." },
    });
    // The server closes the stream after saying access ended, and a failure may race it: a
    // failed state is final until the person retries, so neither resumes it.
    expect(update(stopped, { type: "ended" })).toEqual([stopped, []]);
    expect(update(stopped, { type: "failed", error: restarting })).toEqual([stopped, []]);
  });

  test("a resume or retry out of turn does nothing", () => {
    const live = replay(snapshot);
    expect(update(live, { type: "resume" })).toEqual([live, []]);
    expect(update(live, { type: "retry" })).toEqual([live, []]);
  });
});

/** An API whose live stream answers `response`, or cannot be reached. */
function api(response: Response | null): ApiClient {
  return apiClient(() =>
    response === null ? Promise.reject(new TypeError("offline")) : Promise.resolve(response),
  );
}

/** What `connect` dispatches until it has nothing more to say. */
async function perform(client: ApiClient): Promise<Message[]> {
  const seen: Message[] = [];
  liveMachines(client, WORKSPACE).perform(
    { type: "connect", workspace: WORKSPACE, after: null },
    (message) => {
      seen.push(message);
    },
    new AbortController().signal,
  );
  await vi.waitFor(() => {
    expect(seen.at(-1)?.type).toMatch(/^(ended|failed)$/);
  });
  return seen;
}

describe("its effects", () => {
  test("connect dispatches each update, then the end", async () => {
    const live: LiveUpdate = { type: "snapshot", requests: [], head: 1, machines: [] };
    const response = new Response(`data: ${JSON.stringify(live)}\n\n`, { status: 200 });
    expect(await perform(api(response))).toEqual([received(live), { type: "ended" }]);
  });

  test("connect dispatches a refusal or an unreachable server as a failure", async () => {
    const refused = new Response(
      JSON.stringify({ error: { code: "not_a_member", message: "no" } }),
      { status: 403 },
    );
    expect(await perform(api(refused))).toEqual([
      { type: "failed", error: new ApiError("not_a_member", "no", 403) },
    ]);
    expect(await perform(api(null))).toEqual([
      {
        type: "failed",
        error: new ApiError("network", "the server could not be reached: offline", null),
      },
    ]);
  });

  test("connect stays quiet once the screen went", async () => {
    const controller = new AbortController();
    controller.abort();
    const seen: Message[] = [];
    const settled = Promise.withResolvers<void>();
    const client = apiClient(() => {
      settled.resolve();
      return Promise.reject(new DOMException("aborted", "AbortError"));
    });
    liveMachines(client, WORKSPACE).perform(
      { type: "connect", workspace: WORKSPACE, after: null },
      (message) => {
        seen.push(message);
      },
      controller.signal,
    );
    await settled.promise;
    await Promise.resolve();
    expect(seen).toEqual([]);
  });

  test("wait dispatches resume after its time", () => {
    vi.useFakeTimers();
    const seen: Message[] = [];
    liveMachines(api(null), WORKSPACE).perform(
      { type: "wait", ms: 500 },
      (message) => {
        seen.push(message);
      },
      new AbortController().signal,
    );
    vi.advanceTimersByTime(499);
    expect(seen).toEqual([]);
    vi.advanceTimersByTime(1);
    expect(seen).toEqual([{ type: "resume" }]);
    vi.useRealTimers();
  });
});

describe("enrolment requests", () => {
  const waiting = enrolmentRequest("mbp", "r1");
  const later = { ...enrolmentRequest("pc", "r2"), requested_at: "2026-11-06T09:05:00Z" };

  test("a snapshot shows the pending requests, oldest first", () => {
    const model = replay(
      received({ type: "snapshot", head: 5, machines: [], requests: [later, waiting] }),
    );
    expect(model.requests.map((entry) => entry.id)).toEqual(["r1", "r2"]);
  });

  test("a request that was decided or expired leaves, wherever the owner answered", () => {
    const model = replay(
      received({ type: "snapshot", head: 5, machines: [], requests: [waiting, later] }),
      received({
        type: "machines",
        head: 8,
        changed: [],
        requests: [
          { ...waiting, status: { status: "approved", worker: "w9" } },
          { ...later, status: { status: "expired" } },
          enrolmentRequest("new", "r3"),
        ],
      }),
    );
    expect(model.requests.map((entry) => entry.id)).toEqual(["r3"]);
    expect(model.head).toBe(8);
  });
});
