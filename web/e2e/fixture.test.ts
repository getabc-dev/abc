// The fixture's own guarantees (web/e2e/abc.ts): a server that exits or never gets ready is
// reported with its log and stopped, so no process outlives the run. This is what xtask's
// `a_server_that_exits_before_it_is_ready_is_reported` guarded before the tests took over starting
// the server. Stand-in processes play the server here: node, which never opens the ops port.

import { spawn } from "node:child_process";

import { expect, ready, test } from "./abc.ts";

/** A port nothing listens on: the discard port, which no test machine serves. */
const NOBODY = 9;

test("a server that exits before it is ready is reported, pointing at its log", async () => {
  const server = spawn(process.execPath, ["-e", "process.exit(3)"]);
  await expect(ready(server, NOBODY)).rejects.toThrow(
    /^abc server exited with 3; see .*server\.log$/,
  );
});

test("a server that is never ready is reported and stopped", async () => {
  const server = spawn(process.execPath, ["-e", "setInterval(() => undefined, 1000)"]);
  await expect(ready(server, NOBODY, 500)).rejects.toThrow(
    /^abc server was not ready within 0\.5 s; see .*server\.log$/,
  );
  expect(server.signalCode).toBe("SIGTERM");
});
