// The enrolment panel's rules, without a DOM.

import { expect, test, vi } from "vitest";

import { ApiError, apiClient } from "../api/client";
import type { TokenIssuedResponse } from "../generated/TokenIssuedResponse";
import { enrolment, init, type Message, type Model, update } from "./enrol";

const WORKSPACE = "0190f5a2-0000-7000-8000-000000000001";

const token: TokenIssuedResponse = {
  id: "0190f5a2-0000-7000-8000-00000000000a",
  token: "abc_enr_secret",
  expires_at: "2026-11-06T10:00:00Z",
};

function replay(...messages: Message[]): Model {
  let [model] = init(WORKSPACE);
  for (const message of messages) {
    [model] = update(model, message);
  }
  return model;
}

test("starts closed, asking, with no name", () => {
  expect(init(WORKSPACE)).toEqual([
    { workspace: WORKSPACE, open: false, label: "", step: { state: "asking", error: null } },
    [],
  ]);
  expect(replay({ type: "opened" }).open).toBe(true);
});

test("asks for a token named by the trimmed label, or by the hostname when blank", () => {
  const [named, issue] = update(replay({ type: "labelled", label: " lab box " }), {
    type: "requested",
  });
  expect(named.step).toEqual({ state: "issuing" });
  expect(named.open).toBe(true);
  expect(issue).toEqual([{ type: "issue", workspace: WORKSPACE, label: "lab box" }]);
  expect(update(replay({ type: "labelled", label: "  " }), { type: "requested" })[1]).toEqual([
    { type: "issue", workspace: WORKSPACE, label: null },
  ]);
});

test("a name over 64 characters is refused before asking", () => {
  const [model, effects] = update(replay({ type: "labelled", label: "m".repeat(65) }), {
    type: "requested",
  });
  expect(model.step).toEqual({
    state: "asking",
    error: "Use at most 64 characters for the name.",
  });
  expect(effects).toEqual([]);
  expect(
    replay(
      { type: "labelled", label: "m".repeat(65) },
      { type: "requested" },
      {
        type: "labelled",
        label: "m",
      },
    ).step,
  ).toEqual({ state: "asking", error: null });
});

test("shows the issued token, and closing forgets it and the name", () => {
  const issued = replay(
    { type: "opened" },
    { type: "labelled", label: "lab" },
    { type: "requested" },
    {
      type: "issued",
      token,
    },
  );
  expect(issued.step).toEqual({ state: "issued", token });
  expect(
    replay(
      { type: "opened" },
      { type: "requested" },
      { type: "issued", token },
      {
        type: "closed",
      },
    ),
  ).toEqual(init(WORKSPACE)[0]);
});

test("a refusal is shown in the server's words, and the owner can ask again", () => {
  const model = replay(
    { type: "requested" },
    {
      type: "refused",
      error: new ApiError("rate_limited", "too many tokens; wait", 429),
    },
  );
  expect(model.step).toEqual({ state: "asking", error: "too many tokens; wait" });
  expect(update(model, { type: "requested" })[1]).toHaveLength(1);
});

test("answers out of turn change nothing", () => {
  const asking = replay();
  const issuing = replay({ type: "requested" });
  for (const [model, message] of [
    [asking, { type: "issued", token }],
    [asking, { type: "refused", error: new ApiError("internal", "x", 500) }],
    [issuing, { type: "labelled", label: "late" }],
    [issuing, { type: "requested" }],
  ] satisfies [Model, Message][]) {
    expect(update(model, message)).toEqual([model, []]);
  }
});

test("its effect asks the server and dispatches the answer or the refusal", async () => {
  const answers = [
    new Response(JSON.stringify(token), { status: 200 }),
    new Response(JSON.stringify({ error: { code: "not_a_member", message: "no" } }), {
      status: 403,
    }),
  ];
  const asked: { path: string; body: unknown }[] = [];
  const client = apiClient((path, request) => {
    asked.push({ path, body: request.body });
    return Promise.resolve(answers.shift() ?? new Response(null, { status: 500 }));
  });
  const program = enrolment(client, WORKSPACE);
  const seen: Message[] = [];
  const signal = new AbortController().signal;
  for (let round = 0; round < 2; round += 1) {
    program.perform(
      { type: "issue", workspace: WORKSPACE, label: "lab" },
      (message) => {
        seen.push(message);
      },
      signal,
    );
  }
  await vi.waitFor(() => {
    expect(seen).toHaveLength(2);
  });
  expect(asked[0]).toEqual({
    path: `/api/workspaces/${WORKSPACE}/enrolment-tokens`,
    body: JSON.stringify({ label: "lab" }),
  });
  expect(seen).toContainEqual({ type: "issued", token });
  expect(seen).toContainEqual({
    type: "refused",
    error: new ApiError("not_a_member", "no", 403),
  });
});
