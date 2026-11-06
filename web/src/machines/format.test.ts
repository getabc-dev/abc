import { expect, test } from "vitest";

import { describeMachine, formatInstant } from "./format";

test("a machine is its OS on its architecture, in the words people use", () => {
  expect(describeMachine({ os: "macos", arch: "aarch64" })).toBe("macOS on ARM64");
  expect(describeMachine({ os: "linux", arch: "x86_64" })).toBe("Linux on x86-64");
});

test("an instant is a date and a time in the locale and zone asked for", () => {
  const instant = "2026-11-06T09:05:00Z";
  expect(formatInstant(instant, { locale: "en-GB", timeZone: "UTC" })).toBe("6 Nov 2026, 09:05");
  expect(formatInstant(instant, { locale: "en-US", timeZone: "America/New_York" })).toBe(
    "Nov 6, 2026, 4:05 AM",
  );
});
