import { expect, test } from "vitest";

import schema from "./generated/web-api.schema.json";
import { nameProblem } from "./names";

test("the bounds are the generated schema's, the same for every kind of name", () => {
  for (const kind of ["DisplayName", "WorkspaceName", "MachineLabel"] as const) {
    expect(schema.$defs[kind]).toMatchObject({ minLength: 1, maxLength: 64 });
  }
});

test("a name is 1 to 64 characters after trimming, counted as code points", () => {
  expect(nameProblem("DisplayName", "  ", "your name")).toBe("Enter your name.");
  expect(nameProblem("DisplayName", ` ${"é".repeat(64)} `, "your name")).toBeNull();
  expect(nameProblem("DisplayName", "😀".repeat(64), "your name")).toBeNull();
  expect(nameProblem("WorkspaceName", "a".repeat(65), "a workspace name")).toBe(
    "Use at most 64 characters for a workspace name.",
  );
});

test("a name with a control character is refused, as the server refuses it", () => {
  expect(nameProblem("MachineLabel", "lab\u0007box", "the name")).toBe(
    "Remove the control characters from the name.",
  );
  expect(nameProblem("MachineLabel", "lab box\n", "the name")).toBeNull();
});
