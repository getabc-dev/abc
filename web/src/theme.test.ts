// @vitest-environment jsdom

// The theme: public/theme.js decides and applies it before the app runs, and src/theme.ts reads
// and changes it. Each test loads the script afresh, as a page load does.

import { afterEach, describe, expect, test, vi } from "vitest";

import { stubMatchMedia } from "./testing";
import { chooseTheme, themeChoice } from "./theme";

const root = document.documentElement;

/** Loads public/theme.js on a system whose scheme is dark when `dark` is set. */
async function load(dark: boolean) {
  const system = stubMatchMedia(dark);
  vi.resetModules();
  await import("../public/theme.js");
  return system;
}

afterEach(() => {
  window.localStorage.clear();
  vi.restoreAllMocks();
});

describe("public/theme.js", () => {
  test("with no choice kept, the system's scheme decides, and follows it live", async () => {
    const system = await load(true);
    expect(root.classList.contains("dark")).toBe(true);
    expect(themeChoice()).toBe("system");
    system.set(false);
    expect(root.classList.contains("dark")).toBe(false);
    system.set(true);
    expect(root.classList.contains("dark")).toBe(true);
  });

  test("a kept choice holds at load, whatever the system's scheme", async () => {
    window.localStorage.setItem("abc-theme", "light");
    const system = await load(true);
    expect(root.classList.contains("dark")).toBe(false);
    expect(themeChoice()).toBe("light");
    system.set(false);
    system.set(true);
    expect(root.classList.contains("dark")).toBe(false);
  });

  test("a kept value that is no choice counts as System", async () => {
    window.localStorage.setItem("abc-theme", "sepia");
    await load(true);
    expect(themeChoice()).toBe("system");
    expect(root.classList.contains("dark")).toBe(true);
  });

  test("a choice from the app is applied and kept; System forgets it", async () => {
    await load(false);
    chooseTheme("dark");
    expect(root.classList.contains("dark")).toBe(true);
    expect(themeChoice()).toBe("dark");
    expect(window.localStorage.getItem("abc-theme")).toBe("dark");
    chooseTheme("system");
    expect(root.classList.contains("dark")).toBe(false);
    expect(window.localStorage.getItem("abc-theme")).toBeNull();
  });

  test("another tab's choice arrives through storage", async () => {
    await load(false);
    window.localStorage.setItem("abc-theme", "dark");
    window.dispatchEvent(new StorageEvent("storage", { key: "abc-theme" }));
    expect(root.classList.contains("dark")).toBe(true);
    window.dispatchEvent(new StorageEvent("storage", { key: "elsewhere" }));
    expect(themeChoice()).toBe("dark");
  });

  test("when storage is refused, the system decides and a choice holds for the page", async () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new DOMException("refused", "SecurityError");
    });
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new DOMException("refused", "SecurityError");
    });
    await load(true);
    expect(themeChoice()).toBe("system");
    expect(root.classList.contains("dark")).toBe(true);
    chooseTheme("light");
    expect(themeChoice()).toBe("light");
    expect(root.classList.contains("dark")).toBe(false);
  });
});
