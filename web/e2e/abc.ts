// The abc processes the end-to-end tests drive, as a person would on the server's host and on a
// machine they enrol: `abc server`, `abc server setup-code`, `abc worker enrol` and `abc worker`.
// `cargo xtask e2e` builds the binary, makes the run's database, socket directory and certificate,
// and names them here; the tests start and stop the processes, since a test restarts the server
// and stops a worker. Each Playwright worker runs one server on the run's one database, so what a
// test does persists, as it does on a real server, and a restart keeps it.

import { type ChildProcess, spawn } from "node:child_process";
import { createWriteStream, mkdirSync, mkdtempSync } from "node:fs";
import { createServer } from "node:net";
import { join } from "node:path";

import { type BrowserContext, test as base, expect, type Page } from "@playwright/test";

function required(name: string): string {
  const value = process.env[name];
  if (value === undefined || value === "") {
    throw new Error(`${name} is not set; run the end-to-end tests with \`cargo xtask e2e\``);
  }
  return value;
}

const binary = required("ABC_E2E_BINARY");
const database = required("ABC_E2E_DATABASE_URL");
const socket = required("ABC_E2E_SOCKET");
/** The server's public URL: the stand-in sidecar's origin. */
export const ORIGIN = required("ABC_E2E_ORIGIN");
/** The stand-in's certificate, which workers trust as the platform's CA bundle. */
const certificate = required("ABC_E2E_TLS_CERT");
/** Where the processes' logs and the workers' homes go: `target/e2e`. */
const out = required("ABC_E2E_OUT");
/** Where the screenshots go: `target/e2e-screens`. */
const screens = required("ABC_E2E_SCREENS");

/** Where the processes find their tools; they get nothing else of the tests' environment. */
const { PATH = "" } = process.env;

/** How long the server may take to migrate and start. */
const READY_WITHIN_MS = 60_000;

/** A loopback port nothing listens on yet. */
function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const probe = createServer();
    probe.once("error", reject);
    probe.listen(0, "127.0.0.1", () => {
      const address = probe.address();
      probe.close(() => {
        if (address === null || typeof address === "string") {
          reject(new Error("the probe has no port"));
        } else {
          resolve(address.port);
        }
      });
    });
  });
}

/** Starts `abc` with `args` and `env`, appending its output to `log` under `target/e2e`. */
function run(args: readonly string[], env: Record<string, string>, log: string): ChildProcess {
  mkdirSync(out, { recursive: true });
  const file = createWriteStream(join(out, log), { flags: "a" });
  const child = spawn(binary, args, {
    env: { PATH, ...env },
    stdio: ["pipe", "pipe", "pipe"],
  });
  child.stdout?.pipe(file);
  child.stderr?.pipe(file);
  return child;
}

/** Resolves with the exit code once `child` has exited. */
function exited(child: ChildProcess): Promise<number | null> {
  if (child.exitCode !== null || child.signalCode !== null) {
    return Promise.resolve(child.exitCode);
  }
  return new Promise((resolve) => {
    child.once("exit", (code) => {
      resolve(code);
    });
  });
}

/** Stops `child` as a service manager does, with SIGTERM, and waits for it. */
async function stop(child: ChildProcess): Promise<void> {
  const done = exited(child);
  child.kill("SIGTERM");
  await done;
}

/** Runs `abc` to completion with `input` on standard input; its standard output, or a failure. */
async function complete(
  args: readonly string[],
  env: Record<string, string>,
  input: string,
): Promise<string> {
  const child = spawn(binary, args, {
    env: { PATH, ...env },
    stdio: ["pipe", "pipe", "pipe"],
  });
  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (chunk: Buffer) => {
    stdout += chunk.toString();
  });
  child.stderr.on("data", (chunk: Buffer) => {
    stderr += chunk.toString();
  });
  child.stdin.end(input);
  const code = await exited(child);
  if (code !== 0) {
    throw new Error(`abc ${args.join(" ")} exited with ${code}: ${stderr}`);
  }
  return stdout;
}

/** Waits until the server's ops listener says it is ready, or it exits. */
async function ready(server: ChildProcess, ops: number): Promise<void> {
  const deadline = Date.now() + READY_WITHIN_MS;
  while (Date.now() < deadline) {
    if (server.exitCode !== null) {
      throw new Error(`abc server exited with ${server.exitCode}; see ${join(out, "server.log")}`);
    }
    // Each poll waits for the last; the server is ready when one answers 200.
    // oxlint-disable-next-line eslint/no-await-in-loop -- polls run one after another
    const answer = await fetch(`http://127.0.0.1:${ops}/readyz`).catch(() => null);
    if (answer?.status === 200) {
      return;
    }
    // oxlint-disable-next-line eslint/no-await-in-loop -- a tenth of a second between polls
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error(`abc server was not ready within a minute; see ${join(out, "server.log")}`);
}

/** The run's server, which a test can restart. */
export class Server {
  #process: ChildProcess | null = null;

  /** Starts `abc server` on the run's database and waits until it is ready. */
  async start(): Promise<void> {
    const ops = await freePort();
    const server = run(
      ["server"],
      {
        ABC_DATABASE_URL: database,
        ABC_SIDECAR_SOCKET: socket,
        ABC_OPS_ADDR: `127.0.0.1:${ops}`,
        ABC_PUBLIC_URL: ORIGIN,
      },
      "server.log",
    );
    this.#process = server;
    await ready(server, ops);
  }

  /** Stops the server with SIGTERM, as a service manager does. */
  async stop(): Promise<void> {
    if (this.#process !== null) {
      await stop(this.#process);
      this.#process = null;
    }
  }

  /** The setup code `abc server setup-code` prints on the host, as the person reads it. */
  async setupCode(): Promise<string> {
    const printed = await complete(["server", "setup-code"], { ABC_DATABASE_URL: database }, "");
    const code = /^Setup code \(valid until \d\d:\d\d UTC\): (\S+)$/m.exec(printed)?.[1];
    if (code === undefined) {
      throw new Error(`abc server setup-code printed no code: ${printed}`);
    }
    return code;
  }
}

/** A machine with a worker home of its own. */
export class Machine {
  readonly #home = mkdtempSync(join(out, "worker-"));
  readonly #hostname: string;
  #process: ChildProcess | null = null;

  constructor(hostname: string) {
    this.#hostname = hostname;
  }

  /** The environment a worker runs in: its home, and trust in the stand-in's certificate. */
  #env(): Record<string, string> {
    // On Linux the platform verifier reads the CA bundle that SSL_CERT_FILE names.
    return { ABC_WORKER_HOME: this.#home, SSL_CERT_FILE: certificate };
  }

  /** `abc worker enrol <origin>`, pasting `token` when it asks, as the Machines screen says. */
  async enrol(token: string): Promise<string> {
    return await complete(
      ["worker", "enrol", ORIGIN, "--hostname", this.#hostname],
      this.#env(),
      `${token}\n`,
    );
  }

  /** `abc worker`, left running. */
  start(): void {
    this.#process = run(["worker"], this.#env(), `worker-${this.#hostname}.log`);
  }

  /** Stops the worker with SIGTERM. */
  async stop(): Promise<void> {
    if (this.#process !== null) {
      await stop(this.#process);
      this.#process = null;
    }
  }
}

/** Where a test saves its screenshot named `name`: `target/e2e-screens/<name>.png`. */
export function screenshot(name: string): string {
  return join(screens, `${name}.png`);
}

/** Chrome's console line for an HTTP answer that is not a success. */
const ANSWERED = /^Failed to load resource: the server responded with a status of \d+/u;

/**
 * Playwright's test, with the run's server started for each Playwright worker. Every page a test
 * opens is watched: a console error or an uncaught exception, such as a script or style the CSP
 * refused, fails the test.
 */
export const test = base.extend<{ context: BrowserContext }, { server: Server }>({
  context: async ({ context }, use) => {
    const seen: string[] = [];
    const watch = (page: Page) => {
      page.on("console", (message) => {
        // An API's refusal is an answer the tests ask for (a wrong code, a restarting server);
        // Chrome logs each one, and those alone are not the page's errors.
        if (message.type() === "error" && !ANSWERED.test(message.text())) {
          seen.push(`${page.url()}: ${message.text()}`);
        }
      });
      page.on("pageerror", (error) => {
        seen.push(`${page.url()}: ${error.message}`);
      });
    };
    context.pages().forEach(watch);
    context.on("page", watch);
    await use(context);
    expect(seen, "the console's errors").toEqual([]);
  },
  server: [
    // Playwright reads a fixture's needs from its first parameter's pattern; this one needs none.
    /* oxlint-disable eslint/no-empty-pattern -- Playwright requires the pattern */
    // biome-ignore lint/correctness/noEmptyPattern: Playwright requires the pattern
    async ({}, use) => {
      /* oxlint-enable eslint/no-empty-pattern */
      const server = new Server();
      await server.start();
      await use(server);
      await server.stop();
    },
    { scope: "worker", auto: true },
  ],
});

export { expect };
