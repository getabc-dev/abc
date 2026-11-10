// The Approve page's rules, without a DOM.

import { expect, test, vi } from "vitest";

import { ApiError, apiClient } from "../api/client";
import { enrolmentRequest } from "../testing";
import { approval, init, type Message, type Model, update } from "./approval";

const WORKSPACE = "0190f5a2-0000-7000-8000-000000000001";
const REQUEST = "0190f5a2-0000-7000-8000-0000000000aa";

function replay(...messages: Message[]): Model {
  let [model] = init(WORKSPACE, REQUEST);
  for (const message of messages) {
    [model] = update(model, message);
  }
  return model;
}

function refused(code: ApiError["code"], message: string): Message {
  return { type: "decision_refused", error: new ApiError(code, message, 409) };
}

const loaded: Message = { type: "loaded", card: enrolmentRequest("mbp", REQUEST) };

test("loads the request first", () => {
  expect(init(WORKSPACE, REQUEST)).toEqual([
    { workspace: WORKSPACE, request: REQUEST, stage: { state: "loading" } },
    [{ type: "load", workspace: WORKSPACE, request: REQUEST }],
  ]);
});

test("a pending request waits for the owner; one that ended says how", () => {
  expect(replay(loaded).stage).toEqual({
    state: "open",
    card: enrolmentRequest("mbp", REQUEST),
    deciding: null,
    error: null,
  });
  for (const status of ["approved", "denied", "expired"] as const) {
    const card = enrolmentRequest(
      "mbp",
      REQUEST,
      status === "approved"
        ? { status, worker: "0190f5a2-0000-7000-8000-0000000000bb" }
        : { status },
    );
    expect(replay({ type: "loaded", card }).stage).toEqual({
      state: "closed",
      card,
      outcome: status,
    });
  }
});

test("approving and denying send one answer, and only for a waiting request", () => {
  const [approving, effects] = update(replay(loaded), { type: "approve" });
  expect(approving.stage).toMatchObject({ state: "open", deciding: "approve" });
  expect(effects).toEqual([
    { type: "decide", workspace: WORKSPACE, request: REQUEST, decision: "approve" },
  ]);
  expect(update(approving, { type: "deny" })).toEqual([approving, []]);
  expect(update(approving, { type: "approve" })).toEqual([approving, []]);
  expect(update(replay(), { type: "approve" })[1]).toEqual([]);
  expect(update(replay(loaded), { type: "deny" })[1]).toEqual([
    { type: "decide", workspace: WORKSPACE, request: REQUEST, decision: "deny" },
  ]);
  expect(
    replay(loaded, { type: "approve" }, { type: "decided", outcome: "approved" }).stage,
  ).toEqual({
    state: "closed",
    card: enrolmentRequest("mbp", REQUEST),
    outcome: "approved",
  });
  expect(update(replay(), { type: "decided", outcome: "denied" })[0].stage).toEqual({
    state: "loading",
  });
});

test("a refusal says why, or settles a request decided elsewhere", () => {
  expect(
    replay(loaded, { type: "approve" }, refused("unavailable", "retry shortly")).stage,
  ).toEqual({
    state: "open",
    card: enrolmentRequest("mbp", REQUEST),
    deciding: null,
    error: "retry shortly",
  });
  for (const [code, outcome] of [
    ["enrolment_request_expired", "expired"],
    ["enrolment_request_denied", "denied"],
    ["enrolment_request_approved", "approved"],
  ] as const) {
    expect(replay(loaded, { type: "approve" }, refused(code, "x")).stage).toMatchObject({
      state: "closed",
      outcome,
    });
  }
  expect(update(replay(), refused("unavailable", "x"))[1]).toEqual([]);
});

test("a request that cannot be loaded can be tried again", () => {
  const error = new ApiError("unavailable", "the database is unavailable", 503);
  const failed = replay({ type: "load_failed", error });
  expect(failed.stage).toEqual({ state: "failed", error });
  const [again, effects] = update(failed, { type: "retry" });
  expect(again.stage).toEqual({ state: "loading" });
  expect(effects).toEqual([{ type: "load", workspace: WORKSPACE, request: REQUEST }]);
  expect(update(replay(loaded), { type: "retry" })[1]).toEqual([]);
});

test("performs its effects over the API and reports what came back", async () => {
  const sent: string[] = [];
  const fetcher = vi.fn<(path: string, init: RequestInit) => Promise<Response>>((path, options) => {
    sent.push(`${options.method} ${path}`);
    return Promise.resolve(
      options.method === "GET"
        ? new Response(JSON.stringify(enrolmentRequest("mbp", REQUEST)), { status: 200 })
        : new Response(null, { status: path.endsWith("/deny") ? 409 : 204 }),
    );
  });
  const program = approval(apiClient(fetcher), WORKSPACE, REQUEST);
  const messages: Message[] = [];
  const dispatch = (message: Message) => messages.push(message);
  const signal = new AbortController().signal;
  program.perform({ type: "load", workspace: WORKSPACE, request: REQUEST }, dispatch, signal);
  for (const decision of ["approve", "deny"] as const) {
    program.perform(
      { type: "decide", workspace: WORKSPACE, request: REQUEST, decision },
      dispatch,
      signal,
    );
  }
  await vi.waitFor(() => {
    expect(messages).toHaveLength(3);
  });
  const base = `/api/workspaces/${WORKSPACE}/enrolment-requests/${REQUEST}`;
  expect(sent.toSorted()).toEqual([`GET ${base}`, `POST ${base}/approve`, `POST ${base}/deny`]);
  expect(messages).toContainEqual({ type: "loaded", card: enrolmentRequest("mbp", REQUEST) });
  expect(messages).toContainEqual({ type: "decided", outcome: "approved" });
  expect(messages.find((message) => message.type === "decision_refused")).toMatchObject({
    error: { code: "internal", status: 409 },
  });
  expect(program.init()[0].stage).toEqual({ state: "loading" });
});
