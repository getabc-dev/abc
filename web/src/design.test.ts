// DESIGN.md is the source of the look: its token tables and the stylesheet must say the same, every
// pair it lists must reach its contrast in both themes, and no component writes a colour of its own.

/// <reference types="node" />

import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

import { describe, expect, test } from "vitest";

const WEB = new URL("..", import.meta.url).pathname;

// Read from disk: Vitest hands stylesheets to tests empty, whatever the query.
const design = readFileSync(join(WEB, "../DESIGN.md"), "utf8");
const styles = readFileSync(join(WEB, "src/styles.css"), "utf8");

/** The text of DESIGN.md's section `## <title>`, up to the next one. */
function section(title: string): string {
  const start = design.indexOf(`\n## ${title}\n`);
  if (start === -1) {
    throw new Error(`DESIGN.md has no section "${title}"`);
  }
  const end = design.indexOf("\n## ", start + 1);
  return design.slice(start, end === -1 ? undefined : end);
}

/** The rows of `markdown`'s tables, as the code in their cells: `| \`a\` | \`b\` | ... |`. */
function codeRows(markdown: string): string[][] {
  return markdown
    .split("\n")
    .filter((line) => line.startsWith("| `"))
    .map((line) =>
      line
        .split(" | ")
        .map((cell) => /^\|? ?`([^`]+)`/u.exec(cell)?.[1])
        .filter((cell) => cell !== undefined),
    );
}

type Theme = "light" | "dark";

/** DESIGN.md's colours, by theme: name to value, in upper case as the table writes them. */
function designColours(theme: Theme): Map<string, string> {
  return new Map(
    codeRows(section("Colour")).map(([name = "", light = "", dark = ""]) => [
      name,
      theme === "light" ? light : dark,
    ]),
  );
}

/** DESIGN.md's other tokens: the rows of its Type and Space sections. */
function designTokens(): Map<string, string> {
  return new Map(
    [...codeRows(section("Type")), ...codeRows(section("Space, radius and elevation"))].map(
      ([name = "", value = ""]) => [name, value],
    ),
  );
}

/** The declarations of the stylesheet's first block that opens with `opening`. */
function declarations(opening: string): Map<string, string> {
  const start = styles.indexOf(`${opening} {`);
  if (start === -1) {
    throw new Error(`styles.css has no "${opening}" block`);
  }
  const body = styles.slice(start, styles.indexOf("\n}", start));
  const found = new Map<string, string>();
  for (const [, name, value] of body.matchAll(/(--[a-z0-9-]+):\s*([^;]+);/gu)) {
    if (name !== undefined && value !== undefined && value !== "initial") {
      found.set(name, value.trim());
    }
  }
  return found;
}

/** Each colour, by theme, as the stylesheet sets it, in upper case. */
function styledColours(theme: Theme): Map<string, string> {
  const block = declarations(theme === "light" ? ":root" : ".dark");
  return new Map([...block].map(([name, value]) => [name, value.toUpperCase()]));
}

function sorted(map: Map<string, string>): [string, string][] {
  return [...map].toSorted(([first], [second]) => first.localeCompare(second));
}

/** A colour's relative luminance (WCAG 2.2). */
function luminance(hex: string): number {
  const channels = [1, 3, 5].map((start) => {
    const value = Number.parseInt(hex.slice(start, start + 2), 16) / 255;
    return value <= 0.040_45 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4;
  });
  const [red = 0, green = 0, blue = 0] = channels;
  return 0.2126 * red + 0.7152 * green + 0.0722 * blue;
}

function contrast(first: string, second: string): number {
  const [light, dark] = [luminance(first), luminance(second)].toSorted((a, b) => b - a);
  return ((light ?? 0) + 0.05) / ((dark ?? 0) + 0.05);
}

/** The pairs of the table after `heading` in the Contrast section that fall below `least`. */
function shortfalls(heading: "| Text |" | "| Edge |", least: number): string[] {
  const contrastSection = section("Contrast");
  const start = contrastSection.indexOf(heading);
  const table = contrastSection.slice(start, contrastSection.indexOf("\n\n", start));
  const pairs = codeRows(table);
  expect(pairs.length).toBeGreaterThan(5);
  return (["light", "dark"] as const).flatMap((theme) => {
    const colours = designColours(theme);
    return pairs.flatMap(([front = "", back = ""]) => {
      const ratio = contrast(colours.get(front) ?? "", colours.get(back) ?? "");
      const known = colours.has(front) && colours.has(back);
      return known && ratio >= least
        ? []
        : [`${theme}: ${front} on ${back} is ${ratio.toFixed(2)}`];
    });
  });
}

/** Every component's source: abc's and the registry's. */
function componentSources(): { path: string; source: string }[] {
  return readdirSync(join(WEB, "src"), { recursive: true, encoding: "utf8" })
    .filter((path) => path.endsWith(".tsx") && !path.endsWith(".test.tsx"))
    .map((path) => ({ path, source: readFileSync(join(WEB, "src", path), "utf8") }));
}

describe("DESIGN.md and the stylesheet", () => {
  test("set the same colours, in both themes", () => {
    for (const theme of ["light", "dark"] as const) {
      expect(sorted(styledColours(theme))).toEqual(sorted(designColours(theme)));
    }
    expect(designColours("light").size).toBeGreaterThan(30);
    expect(
      [...designColours("dark").values()].filter((value) => !/^#[0-9A-F]{6}$/u.test(value)),
    ).toEqual([]);
  });

  test("make every colour, and only those, a Tailwind colour", () => {
    const utilities = declarations("@theme inline");
    expect(sorted(utilities)).toEqual(
      sorted(
        new Map(
          [...designColours("light").keys()].map((name) => [
            `--color-${name.slice(2)}`,
            `var(${name})`,
          ]),
        ),
      ),
    );
  });

  test("declare the same other tokens with the same values", () => {
    expect(sorted(declarations("@theme"))).toEqual(sorted(designTokens()));
    expect(designTokens().size).toBeGreaterThan(20);
  });

  test("reset Tailwind's own colours, fonts, sizes, radii and shadows", () => {
    for (const namespace of ["color", "font", "text", "radius", "shadow"]) {
      expect(styles).toContain(`--${namespace}-*: initial;`);
    }
  });

  test("every listed text pair reaches 4.5:1 in both themes", () => {
    expect(shortfalls("| Text |", 4.5)).toEqual([]);
  });

  test("every listed edge and focus ring reaches 3:1 in both themes", () => {
    expect(shortfalls("| Edge |", 3)).toEqual([]);
  });

  test("contrast is computed as WCAG defines it", () => {
    expect(contrast("#000000", "#FFFFFF")).toBeCloseTo(21, 5);
    expect(contrast("#FFFFFF", "#FFFFFF")).toBe(1);
    expect(contrast("#767676", "#FFFFFF")).toBeCloseTo(4.54, 2);
  });

  test("no component writes a colour of its own", () => {
    const literal =
      /#[0-9a-fA-F]{3,8}\b|\b(?:rgba?|hsla?|oklch|oklab|lab|lch|color)\(|\b(?:bg|text|border|ring|fill|stroke)-(?:black|white)\b/u;
    const sources = componentSources();
    expect(sources.length).toBeGreaterThan(20);
    expect(sources.filter(({ source }) => literal.test(source)).map(({ path }) => path)).toEqual(
      [],
    );
  });
});
