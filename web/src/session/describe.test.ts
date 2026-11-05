import { expect, test } from "vitest";

import type { WorkspaceSummary } from "../generated/WorkspaceSummary";
import { describeSession } from "./describe";

test("a visitor without identity is told to use a person's device", () => {
  const summary = describeSession({ state: "no_identity" });
  expect(summary.title).toBe("No tailnet identity");
  expect(summary.detail).toContain("signed in as a person");
});

test("an unclaimed server names the login and the setup code command", () => {
  const summary = describeSession({ state: "unclaimed", login: "ada@example.com" });
  expect(summary.title).toBe("This server is unclaimed");
  expect(summary.detail).toContain("ada@example.com");
  expect(summary.detail).toContain("`abc server setup-code`");
});

test("a login outside every workspace is not a member", () => {
  const summary = describeSession({ state: "not_a_member", login: "bob@example.com" });
  expect(summary.title).toBe("Not a member");
  expect(summary.detail).toContain("bob@example.com");
});

test("a member is greeted by name and counts their workspaces", () => {
  const workspace: WorkspaceSummary = {
    id: "0190f5a2-0000-7000-8000-000000000001",
    name: "Home",
    role: "owner",
  };
  const one = describeSession({
    state: "member",
    person: "0190f5a2-0000-7000-8000-000000000002",
    display_name: "Ada",
    workspaces: [workspace],
  });
  expect(one).toEqual({ title: "Signed in as Ada", detail: "You belong to 1 workspace." });
  const two = describeSession({
    state: "member",
    person: "0190f5a2-0000-7000-8000-000000000002",
    display_name: "Ada",
    workspaces: [workspace, { ...workspace, name: "Work" }],
  });
  expect(two.detail).toBe("You belong to 2 workspaces.");
});
