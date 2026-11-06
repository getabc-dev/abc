import tailwindcss from "@tailwindcss/vite";
import { tanstackRouter } from "@tanstack/router-plugin/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

export default defineConfig({
  // The router plugin writes src/routeTree.gen.ts from src/routes/, so it runs before React's.
  plugins: [tanstackRouter({ target: "react", autoCodeSplitting: true }), react(), tailwindcss()],
  build: {
    // The abc binary embeds dist/ (crates/abc/build.rs); hashed files go to assets/, which the
    // server caches for a year.
    outDir: "dist",
    assetsDir: "assets",
    // Never inline a file as a `data:` URL: the CSP (`default-src 'self'`) allows the app's own
    // files only, so the font subsets must be files.
    assetsInlineLimit: 0,
    emptyOutDir: true,
    // Hex hashes, so the server can tell a hashed name (`<name>-<hash>.<extension>`) apart.
    rolldownOptions: { output: { hashCharacters: "hex" } },
  },
  test: {
    include: ["src/**/*.test.{ts,tsx}"],
    // A focused test (`test.only`) would hide every other one.
    allowOnly: false,
    // Tests run in a shuffled order from a fixed seed, so every run repeats exactly; change the
    // seed to explore another order.
    sequence: { shuffle: true, seed: 20261031 },
    coverage: {
      provider: "v8",
      include: ["src/**/*.{ts,tsx}"],
      // main.tsx only mounts the app on the page, which the end-to-end test loads (e2e/).
      exclude: [
        "src/**/*.test.{ts,tsx}",
        "src/generated/**",
        "src/routeTree.gen.ts",
        "src/main.tsx",
      ],
      reporter: ["lcov", "text-summary"],
      reportsDirectory: "coverage",
    },
  },
});
