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
  // The tests share the run's one server and database, so what one does the next sees, as on a
  // real server: the unclaimed server first, then the claim, then the claimed server's machines.
  projects: [
    {
      name: "unclaimed",
      testMatch: /(home|origin|theme)\.test\.ts/,
      use: devices["Desktop Chrome"],
    },
    {
      name: "claim",
      testMatch: /claim\.test\.ts/,
      dependencies: ["unclaimed"],
      use: devices["Desktop Chrome"],
    },
    {
      name: "machines",
      testMatch: /machines\.test\.ts/,
      dependencies: ["claim"],
      use: devices["Desktop Chrome"],
    },
  ],
  webServer: {
    command: "node e2e/sidecar.ts",
    // The stand-in is up once its port accepts connections; abc behind it starts with the tests.
    ...(origin === undefined ? {} : { port: Number(new URL(origin).port) }),
    reuseExistingServer: false,
    stdout: "pipe",
  },
});
