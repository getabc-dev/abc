// The claim link's setup code: read from the fragment, then taken out of the history entry.

import { describe, expect, test, vi } from "vitest";

import { setupCodeFrom, takeSetupCode } from "./link";

describe("claim link", () => {
  test("reads the code from the fragment", () => {
    expect(setupCodeFrom("#code=ABCDE-FGHJK-MNPQR-STVWX")).toBe("ABCDE-FGHJK-MNPQR-STVWX");
    expect(setupCodeFrom("#other=1&code=%20abc%20")).toBe("abc");
  });

  test("a fragment without a code is no code", () => {
    expect(setupCodeFrom("")).toBe("");
    expect(setupCodeFrom("#")).toBe("");
    expect(setupCodeFrom("#codes=x")).toBe("");
  });

  test("taking the code replaces the history entry with one without the fragment", () => {
    const replaceState = vi.fn<(data: unknown, unused: string, url: string) => void>();
    const code = takeSetupCode(
      { hash: "#code=abc", pathname: "/machines", search: "?x=1" },
      { replaceState },
    );
    expect(code).toBe("abc");
    expect(replaceState).toHaveBeenCalledExactlyOnceWith(null, "", "/machines?x=1");
  });

  test("a location without a code is left alone", () => {
    const replaceState = vi.fn<(data: unknown, unused: string, url: string) => void>();
    expect(takeSetupCode({ hash: "#top", pathname: "/", search: "" }, { replaceState })).toBe("");
    expect(replaceState).not.toHaveBeenCalled();
  });
});
