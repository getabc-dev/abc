// The abc processes the end-to-end tests drive, as a person would on the server's host and on a
// machine they enrol: `abc server`, `abc server setup-code`, `abc worker enrol` and `abc worker`.
// `cargo xtask e2e` builds the binary, makes the run's database, socket directory and certificate,
// and names them here; the tests start and stop the processes, since a test restarts the server
// and stops a worker. Each Playwright worker runs one server on the run's one database, so what a
// test does persists, as it does on a real server, and a restart keeps it.

import { type ChildProcess, spawn } from "node:child_process";
import {
  cpSync,
  createWriteStream,
  mkdirSync,
  mkdtempSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { createServer } from "node:net";
import { dirname, join } from "node:path";

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

/**
 * Every process the tests started that has not exited. The fixtures stop theirs in teardown,
 * pass or fail; if the Playwright worker exits first, it kills what is left, so no process outlives
 * the run and the run's database can be dropped after them.
 */
const running = new Set<ChildProcess>();

process.once("exit", () => {
  for (const child of running) {
    child.kill("SIGKILL");
  }
});

/** Starts `abc` with `args` and `env`, appending its output to `log` under `target/e2e`. */
function run(args: readonly string[], env: Record<string, string>, log: string): ChildProcess {
  mkdirSync(out, { recursive: true });
  const file = createWriteStream(join(out, log), { flags: "a" });
  const child = spawn(binary, args, {
    env: { PATH, ...env },
    stdio: ["pipe", "pipe", "pipe"],
  });
  running.add(child);
  child.once("exit", () => {
    running.delete(child);
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
  if (child.exitCode === null && child.signalCode === null) {
    child.kill("SIGTERM");
  }
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

/**
 * Waits until the server's ops listener on port `ops` says it is ready. A server that exits first,
 * or is not ready within `withinMs`, is stopped and reported, pointing at its log.
 */
export async function ready(
  server: ChildProcess,
  ops: number,
  withinMs = READY_WITHIN_MS,
): Promise<void> {
  try {
    await poll(server, ops, withinMs);
  } catch (error: unknown) {
    await stop(server);
    throw error;
  }
}

async function poll(server: ChildProcess, ops: number, withinMs: number): Promise<void> {
  const deadline = Date.now() + withinMs;
  while (Date.now() < deadline) {
    if (server.exitCode !== null || server.signalCode !== null) {
      const status = server.exitCode ?? server.signalCode;
      throw new Error(`abc server exited with ${status}; see ${join(out, "server.log")}`);
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
  throw new Error(
    `abc server was not ready within ${withinMs / 1000} s; see ${join(out, "server.log")}`,
  );
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
        // The stand-in's tailscaled, beside the socket, which names the node of each request.
        ABC_TAILSCALED_SOCKET: join(dirname(socket), "tailscaled.sock"),
        ABC_OPS_ADDR: `127.0.0.1:${ops}`,
        ABC_PUBLIC_URL: ORIGIN,
      },
      "server.log",
    );
    this.#process = server;
    try {
      await ready(server, ops);
    } catch (error: unknown) {
      this.#process = null;
      throw error;
    }
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

/** What `abc worker setup` prints while it waits for the owner, and how it ends. */
class Setup {
  readonly #child: ChildProcess;
  #output = "";
  readonly #ended: Promise<number | null>;

  constructor(child: ChildProcess) {
    this.#child = child;
    child.stdout?.on("data", (chunk: Buffer) => {
      this.#output += chunk.toString();
    });
    child.stderr?.on("data", (chunk: Buffer) => {
      this.#output += chunk.toString();
    });
    this.#ended = exited(child);
  }

  /** Everything it has printed. */
  get output(): string {
    return this.#output;
  }

  /** The page it asks the owner to open, and the code it shows, once it has printed them. */
  async approval(): Promise<{ readonly page: string; readonly code: string }> {
    const deadline = Date.now() + READY_WITHIN_MS;
    while (Date.now() < deadline) {
      const page = /(https:\/\/\S+\/machines\/requests\/\S+)/u.exec(this.#output)?.[1];
      const code = /code ([0-9A-Z]{4}-[0-9A-Z]{4})/u.exec(this.#output)?.[1];
      if (page !== undefined && code !== undefined) {
        return { page, code };
      }
      if (this.#child.exitCode !== null) {
        throw new Error(`abc worker setup ended before it asked for approval: ${this.#output}`);
      }
      // oxlint-disable-next-line eslint/no-await-in-loop -- polling: each look needs the last
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    throw new Error(`abc worker setup did not ask for approval: ${this.#output}`);
  }

  /** Waits for it to end; its exit code. */
  async finished(): Promise<number | null> {
    return await this.#ended;
  }
}

/** Runs the fake service manager or `abc` to completion: its standard output, or a failure. */
async function tool(
  program: string,
  args: readonly string[],
  env: Record<string, string>,
): Promise<{ readonly code: number | null; readonly output: string }> {
  const child = spawn(program, args, { env, stdio: ["ignore", "pipe", "pipe"] });
  let output = "";
  child.stdout.on("data", (chunk: Buffer) => {
    output += chunk.toString();
  });
  child.stderr.on("data", (chunk: Buffer) => {
    output += chunk.toString();
  });
  return { code: await exited(child), output };
}

/**
 * A machine with a worker home of its own, and a home directory with fake service managers, root
 * and browser (the ones `abc-host`'s tests use) on its `PATH`, which run what a service definition
 * says. Tests get one from the `machine` fixture.
 */
class Machine {
  readonly #home = mkdtempSync(join(out, "worker-"));
  /** The fakes' scratch tree: `bin/`, `state/`, `services/` and the user's `home/`. */
  readonly #scratch = mkdtempSync(join(out, "machine-"));
  readonly #hostname: string;
  #process: ChildProcess | null = null;

  constructor(hostname: string) {
    this.#hostname = hostname;
    cpSync(new URL("../../crates/abc-host/tests/fakes/bin", import.meta.url), this.#bin(), {
      recursive: true,
    });
    mkdirSync(join(this.#scratch, "state"));
    // The service definitions `abc worker setup` writes, where the fakes read them.
    const definitions =
      process.platform === "darwin"
        ? join(this.#userHome(), "Library/LaunchAgents")
        : join(this.#userHome(), ".config/systemd/user");
    mkdirSync(definitions, { recursive: true });
    symlinkSync(definitions, join(this.#scratch, "services"));
    // Services get nothing of this environment but their own definition: trust in the stand-in's
    // certificate is added, as a machine's own trust store would hold it.
    writeFileSync(join(this.#scratch, "state/extra.env"), `SSL_CERT_FILE=${certificate}\n`);
  }

  #bin(): string {
    return join(this.#scratch, "bin");
  }

  #userHome(): string {
    return join(this.#scratch, "home");
  }

  /** The environment of a person's shell on this machine: its fakes first on `PATH`. */
  #shell(): Record<string, string> {
    return {
      ...this.#env(),
      HOME: this.#userHome(),
      PATH: `${this.#bin()}:${PATH}`,
      FAKE_ROOT: "1",
    };
  }

  /** `abc worker setup <origin>`: it waits for the owner's approval in the browser. */
  setup(): Setup {
    mkdirSync(out, { recursive: true });
    const log = createWriteStream(join(out, `setup-${this.#hostname}.log`), { flags: "a" });
    const child = spawn(binary, ["worker", "setup", ORIGIN, "--hostname", this.#hostname], {
      env: this.#shell(),
      stdio: ["ignore", "pipe", "pipe"],
    });
    running.add(child);
    child.once("exit", () => {
      running.delete(child);
    });
    child.stdout.pipe(log);
    return new Setup(child);
  }

  /** `abc worker status`: whether it succeeded, and what it said. */
  async status(): Promise<{ readonly code: number | null; readonly output: string }> {
    return await tool(binary, ["worker", "status"], this.#shell());
  }

  /** `abc worker uninstall`, as a person runs it. */
  async uninstall(): Promise<string> {
    const { code, output } = await tool(binary, ["worker", "uninstall"], this.#shell());
    if (code !== 0) {
      throw new Error(`abc worker uninstall exited with ${code}: ${output}`);
    }
    return output;
  }

  /** Restarts the worker's service, as a reboot would start it again. */
  async restartService(): Promise<void> {
    const [program, args] =
      process.platform === "darwin"
        ? ["launchctl", ["kickstart", "-k", `gui/${process.getuid?.() ?? 501}/dev.getabc.worker`]]
        : ["systemctl", ["--user", "restart", "abc-worker.service"]];
    const { code, output } = await tool(join(this.#bin(), program), args, this.#shell());
    if (code !== 0) {
      throw new Error(`restarting the service exited with ${code}: ${output}`);
    }
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
 * Whether `url` is the API's, on the app's origin. Its refusals are answers the tests ask for (a
 * wrong code, a stream while the server restarts); any other failed load, such as the app's own
 * script, style, font or `theme.js`, is a broken page.
 */
function isApi(url: string): boolean {
  const parsed = new URL(url);
  return parsed.origin === new URL(ORIGIN).origin && parsed.pathname.startsWith("/api/");
}

/**
 * Playwright's test, with the run's server started for each Playwright worker. Every page a test
 * opens is watched: a console error or an uncaught exception, such as a script or style the CSP
 * refused, fails the test.
 */
export const test = base.extend<
  { context: BrowserContext; machine: (hostname: string) => Machine },
  { server: Server }
>({
  context: async ({ context }, use) => {
    const seen: string[] = [];
    const watch = (page: Page) => {
      page.on("console", (message) => {
        // Chrome logs every answer that is not a success; only the API's are exempt, as the
        // response listener below decides by URL.
        const refused = ANSWERED.test(message.text()) && isApi(message.location().url);
        if (message.type() === "error" && !refused) {
          seen.push(`${page.url()}: ${message.text()} (${message.location().url})`);
        }
      });
      page.on("response", (response) => {
        if (response.status() >= 400 && !isApi(response.url())) {
          seen.push(`${page.url()}: ${response.url()} answered ${response.status()}`);
        }
      });
      page.on("requestfailed", (request) => {
        if (!isApi(request.url())) {
          seen.push(`${page.url()}: ${request.url()} failed: ${request.failure()?.errorText}`);
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
      try {
        await server.start();
        await use(server);
      } finally {
        await server.stop();
      }
    },
    { scope: "worker", auto: true },
  ],
  // Every machine a test makes is stopped when the test ends, pass or fail.
  /* oxlint-disable eslint/no-empty-pattern -- Playwright requires the pattern */
  // biome-ignore lint/correctness/noEmptyPattern: Playwright requires the pattern
  machine: async ({}, use) => {
    /* oxlint-enable eslint/no-empty-pattern */
    const made: Machine[] = [];
    try {
      await use((hostname) => {
        const machine = new Machine(hostname);
        made.push(machine);
        return machine;
      });
    } finally {
      await Promise.all(made.map((machine) => machine.stop()));
    }
  },
});

export { expect };
