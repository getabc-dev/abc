// abc's components come from shadcn/ui's registry (DESIGN.md, decision 46): a component the
// registry offers is added with the shadcn CLI into src/components/ui/, never written by hand. This
// fails when a hand-written component outside src/components/ui/ has the name of a registry
// component, and when React Aria, which shadcn replaced, comes back.
//
// The registry's components are listed in web/shadcn-registry.json, as the shadcn CLI prints them.
// Refresh it from the repository's root, where ui.shadcn.com is reachable:
//   cargo xtask pnpm exec shadcn search @shadcn --type ui --limit 1000 --json > web/shadcn-registry.json

/// <reference types="node" />

import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

import { describe, expect, test } from "vitest";

const WEB = new URL("..", import.meta.url).pathname;

/** The registry's components, by the names their files export: `dropdown-menu` is DropdownMenu. */
function registryComponents(): Set<string> {
  const listed: unknown = JSON.parse(readFileSync(join(WEB, "shadcn-registry.json"), "utf8"));
  const items =
    typeof listed === "object" && listed !== null && "items" in listed ? listed.items : null;
  if (!Array.isArray(items)) {
    throw new Error("web/shadcn-registry.json has no items; refresh it (see this file's top)");
  }
  const entries: readonly unknown[] = items;
  const names = new Set<string>();
  for (const item of entries) {
    if (typeof item === "object" && item !== null && "name" in item) {
      const { name } = item;
      if (typeof name === "string") {
        names.add(
          name
            .split("-")
            .map((word) => word.charAt(0).toUpperCase() + word.slice(1))
            .join(""),
        );
      }
    }
  }
  return names;
}

/** The components `source` declares: capitalised functions, constants and classes. */
function declaredComponents(source: string): string[] {
  const pattern = /^\s*(?:export\s+)?(?:default\s+)?(?:function|const|let|class)\s+([A-Z]\w*)/gmu;
  return [...source.matchAll(pattern)].flatMap((match) =>
    match[1] === undefined ? [] : [match[1]],
  );
}

/** Every TypeScript file under `directory`, by its path from web/. */
function sources(directory: string): string[] {
  return readdirSync(join(WEB, directory), { recursive: true, encoding: "utf8" })
    .filter((path) => /\.tsx?$/u.test(path))
    .map((path) => join(directory, path));
}

/** The hand-written files: all of src/ and e2e/ but the registry's and the generated ones. */
function handWritten(): string[] {
  return [...sources("src"), ...sources("e2e")].filter(
    (path) =>
      !path.startsWith("src/components/ui/") &&
      !path.startsWith("src/generated/") &&
      path !== "src/routeTree.gen.ts",
  );
}

describe("shadcn/ui is the one source of components", () => {
  test("the registry list is the CLI's, with the components abc uses", () => {
    const components = registryComponents();
    expect(components.size).toBeGreaterThan(50);
    for (const name of ["Button", "Input", "Field", "Badge", "Card", "Sidebar", "DropdownMenu"]) {
      expect(components).toContain(name);
    }
  });

  test("no hand-written component has the name of a registry component", () => {
    const components = registryComponents();
    const clashes = handWritten().flatMap((path) =>
      declaredComponents(readFileSync(join(WEB, path), "utf8"))
        .filter((name) => components.has(name))
        .map((name) => `${path}: ${name} is shadcn's; add it with the shadcn CLI instead`),
    );
    expect(clashes).toEqual([]);
  });

  test("a planted hand-made Button is caught", () => {
    const planted = 'export function Button() {\n  return <button type="button" />;\n}\n';
    const caught = declaredComponents(planted).filter((name) => registryComponents().has(name));
    expect(caught).toEqual(["Button"]);
    expect(declaredComponents("const Card = () => null;\nclass Sidebar {}\n")).toEqual([
      "Card",
      "Sidebar",
    ]);
    expect(declaredComponents("type Message = string;\nfunction update() {}\n")).toEqual([]);
  });

  test("React Aria is gone, from the dependencies and from every import", () => {
    const manifest = readFileSync(join(WEB, "package.json"), "utf8");
    expect(manifest).not.toMatch(/react-aria|@react-aria|@react-stately/u);
    const imports = [...sources("src"), ...sources("e2e")].filter((path) =>
      /from\s+["'](?:react-aria|@react-aria|@react-stately)/u.test(
        readFileSync(join(WEB, path), "utf8"),
      ),
    );
    expect(imports).toEqual([]);
  });
});
