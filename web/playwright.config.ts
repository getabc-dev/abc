// The end-to-end tests: `cargo xtask e2e` builds abc, starts `abc server` and runs these against
// it in Chromium, through e2e/sidecar.ts, the stand-in for the Tailscale sidecar. The browser
// loads the app from the origin the stand-in answers on, which is the server's public URL.

import { defineConfig, devices } from "@playwright/test";

/**
 * The server's public origin, which `cargo xtask e2e` sets. Without it the stand-in refuses to
 * start, so no test runs; knip reads this file without it.
 */
const { ABC_E2E_ORIGIN: origin } = process.env;

export default defineConfig({
  testDir: "e2e",
  // A focused test (`test.only`) would hide every other one.
  forbidOnly: true,
  retries: 0,
  workers: 1,
  reporter: [["list"]],
  outputDir: "test-results",
  use: {
    ...(origin === undefined ? {} : { baseURL: origin }),
    // The stand-in's certificate is made for the run and signed by nobody.
    ignoreHTTPSErrors: true,
    trace: "retain-on-failure",
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
  webServer: {
    command: "node e2e/sidecar.ts",
    ...(origin === undefined ? {} : { url: origin }),
    ignoreHTTPSErrors: true,
    reuseExistingServer: false,
    stdout: "pipe",
  },
});
