// DESIGN.md is the source of the look: its token tables and the stylesheet's `@theme` must say the
// same, and every text pair it lists must reach 4.5:1.

/// <reference types="node" />

import { readFileSync } from "node:fs";

import { describe, expect, test } from "vitest";

// Read from disk: Vitest hands stylesheets to tests empty, whatever the query.
const design = readFileSync(new URL("../../DESIGN.md", import.meta.url), "utf8");
const styles = readFileSync(new URL("styles.css", import.meta.url), "utf8");

/** The rows of DESIGN.md's tables whose first two cells are code: `| \`a\` | \`b\` | ... |`. */
function codeRows(markdown: string): (readonly [string, string])[] {
  const rows: (readonly [string, string])[] = [];
  for (const line of markdown.split("\n")) {
    const match = /^\| `([^`]+)` \| `([^`]+)` \|/.exec(line);
    if (match?.[1] !== undefined && match[2] !== undefined) {
      rows.push([match[1], match[2]]);
    }
  }
  return rows;
}

/** DESIGN.md's tokens: the rows whose first cell is a CSS variable and second is not one. */
function designTokens(): Map<string, string> {
  return new Map(
    codeRows(design).filter(([name, value]) => name.startsWith("--") && !value.startsWith("--")),
  );
}

/** DESIGN.md's contrast pairs: the rows whose cells are both variables. */
function contrastPairs(): (readonly [string, string])[] {
  return codeRows(design).filter(([, background]) => background.startsWith("--"));
}

/** The declarations of styles.css's `@theme`, without the resets. */
function themeTokens(): Map<string, string> {
  const theme = /@theme \{([^}]*)\}/.exec(styles)?.[1] ?? "";
  const tokens = new Map<string, string>();
  for (const match of theme.matchAll(/(--[a-z0-9-]+):\s*([^;]+);/g)) {
    const [, name, value] = match;
    if (name !== undefined && value !== undefined && value !== "initial") {
      tokens.set(name, value.trim());
    }
  }
  return tokens;
}

/** Orders `[name, value]` rows by name. */
function byName(first: string[], second: string[]): number {
  return (first[0] ?? "").localeCompare(second[0] ?? "");
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

describe("DESIGN.md and the stylesheet", () => {
  test("declare the same tokens with the same values", () => {
    const declared = [...designTokens()].map(([name, value]) => [name, value.toLowerCase()]);
    const themed = [...themeTokens()].map(([name, value]) => [name, value.toLowerCase()]);
    expect(themed.toSorted(byName)).toEqual(declared.toSorted(byName));
    expect(declared.length).toBeGreaterThan(30);
  });

  test("reset Tailwind's own colours, fonts, sizes, radii and shadows", () => {
    for (const namespace of ["color", "font", "text", "radius", "shadow"]) {
      expect(styles).toContain(`--${namespace}-*: initial;`);
    }
  });

  test("every listed text pair reaches 4.5:1", () => {
    const tokens = designTokens();
    const pairs = contrastPairs();
    expect(pairs.length).toBeGreaterThan(10);
    const ratios = pairs.map(([text, background]) => {
      const foreground = tokens.get(text) ?? "";
      const behind = tokens.get(background) ?? "";
      return {
        pair: `${text} on ${background}`,
        colours: `${foreground} ${behind}`,
        ratio: contrast(foreground, behind),
      };
    });
    expect(ratios.filter(({ colours }) => !/^#[0-9A-F]{6} #[0-9A-F]{6}$/.test(colours))).toEqual(
      [],
    );
    expect(ratios.filter(({ ratio }) => ratio < 4.5)).toEqual([]);
  });

  test("a field's edge reaches 3:1 against every surface it sits on", () => {
    const tokens = designTokens();
    const control = tokens.get("--color-control") ?? "";
    const ratios = ["--color-ground", "--color-surface", "--color-sunken"].map((surface) => ({
      surface,
      ratio: contrast(control, tokens.get(surface) ?? ""),
    }));
    expect(ratios.filter(({ ratio }) => ratio < 3)).toEqual([]);
  });

  test("contrast is computed as WCAG defines it", () => {
    expect(contrast("#000000", "#FFFFFF")).toBeCloseTo(21, 5);
    expect(contrast("#FFFFFF", "#FFFFFF")).toBe(1);
    expect(contrast("#767676", "#FFFFFF")).toBeCloseTo(4.54, 2);
  });
});
