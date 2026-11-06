// The claim's rules, without a DOM.

import { describe, expect, test } from "vitest";

import { ApiError } from "../api/client";
import { type Fields, init, type Message, type Model, update, validate } from "./claim";

const filled: Fields = { code: " abcd-efgh ", displayName: " Ada ", workspaceName: "Home" };

/** The model after `messages`, from the start. */
function replay(...messages: Message[]): Model {
  let [model] = init();
  for (const message of messages) {
    [model] = update(model, message);
  }
  return model;
}

function typed(fields: Fields): Message[] {
  return [
    { type: "edited", field: "code", value: fields.code },
    { type: "edited", field: "displayName", value: fields.displayName },
    { type: "edited", field: "workspaceName", value: fields.workspaceName },
  ];
}

function refusedWith(code: ApiError["code"], message = "refused"): Message {
  return { type: "refused", error: new ApiError(code, message, 403) };
}

const claiming = replay(...typed(filled), { type: "submitted" });

describe("claim", () => {
  test("starts with an empty form", () => {
    const [model, effects] = init();
    expect(model).toEqual({
      fields: { code: "", displayName: "", workspaceName: "" },
      phase: { state: "editing", errors: {}, problem: null },
    });
    expect(effects).toEqual([]);
  });

  test("submitting a filled form claims with the trimmed fields", () => {
    const [model, effects] = update(replay(...typed(filled)), { type: "submitted" });
    expect(model.phase).toEqual({ state: "claiming" });
    expect(effects).toEqual([
      {
        type: "claim",
        request: { code: "abcd-efgh", display_name: "Ada", workspace_name: "Home" },
      },
    ]);
  });

  test("submitting an empty form names every missing field and claims nothing", () => {
    const [model, effects] = update(replay(), { type: "submitted" });
    expect(model.phase).toEqual({
      state: "editing",
      errors: {
        code: "Enter the setup code from the server's host.",
        displayName: "Enter your name.",
        workspaceName: "Enter a workspace name.",
      },
      problem: null,
    });
    expect(effects).toEqual([]);
  });

  test("names are 1 to 64 characters after trimming, counted as characters", () => {
    expect(validate({ ...filled, displayName: "   " }).displayName).toBe("Enter your name.");
    expect(validate({ ...filled, workspaceName: "x".repeat(65) }).workspaceName).toBe(
      "Use at most 64 characters for a workspace name.",
    );
    expect(validate({ ...filled, displayName: "é".repeat(64) })).toEqual({});
  });

  test("editing a field clears its error and the form's problem, not the others", () => {
    const failed = replay({ type: "submitted" });
    const model = replay({ type: "submitted" }, { type: "edited", field: "code", value: "x" });
    expect(Object.keys(failed.phase.state === "editing" ? failed.phase.errors : {})).toHaveLength(
      3,
    );
    expect(model.fields.code).toBe("x");
    expect(model.phase).toEqual({
      state: "editing",
      errors: { displayName: "Enter your name.", workspaceName: "Enter a workspace name." },
      problem: null,
    });
  });

  test("a wrong code is the code field's error, keeping what was typed", () => {
    const [model] = update(claiming, refusedWith("setup_code_mismatch"));
    expect(model.fields.code).toBe(" abcd-efgh ");
    expect(model.phase.state === "editing" && model.phase.errors.code).toBe(
      "This is not the server's current setup code. Check it, or print a new one.",
    );
  });

  test("a server that printed no code yet says how to print one", () => {
    const [model] = update(claiming, refusedWith("setup_code_required"));
    expect(model.phase.state === "editing" && model.phase.errors.code).toBe(
      "This server has not printed a setup code yet. Run abc server setup-code on its host.",
    );
  });

  test("an expired code has its own state, and starting over clears only the code", () => {
    const [expired] = update(claiming, refusedWith("setup_code_expired"));
    expect(expired.phase).toEqual({ state: "expired" });
    const [model, effects] = update(expired, { type: "start_over" });
    expect(model).toEqual({
      fields: { code: "", displayName: " Ada ", workspaceName: "Home" },
      phase: { state: "editing", errors: {}, problem: null },
    });
    expect(effects).toEqual([]);
  });

  test("an already claimed server has its own state, and continuing goes home", () => {
    const [claimed] = update(claiming, refusedWith("server_already_claimed"));
    expect(claimed.phase).toEqual({ state: "already_claimed" });
    expect(update(claimed, { type: "continue" })).toEqual([claimed, [{ type: "go_home" }]]);
  });

  test("any other refusal is the form's problem, in the server's words", () => {
    const [model] = update(claiming, refusedWith("rate_limited", "too many tries; wait"));
    expect(model.phase).toEqual({ state: "editing", errors: {}, problem: "too many tries; wait" });
    const [fixed] = update(model, { type: "edited", field: "code", value: "y" });
    expect(fixed.phase).toEqual({ state: "editing", errors: {}, problem: null });
  });

  test("a claim goes home", () => {
    expect(update(claiming, { type: "claimed" })).toEqual([
      { ...claiming, phase: { state: "claimed" } },
      [{ type: "go_home" }],
    ]);
  });

  test("messages out of turn change nothing", () => {
    const editing = replay();
    for (const [model, message] of [
      [claiming, { type: "edited", field: "code", value: "z" }],
      [claiming, { type: "submitted" }],
      [editing, refusedWith("setup_code_expired")],
      [editing, { type: "start_over" }],
      [editing, { type: "continue" }],
    ] satisfies [Model, Message][]) {
      expect(update(model, message)).toEqual([model, []]);
    }
  });
});
