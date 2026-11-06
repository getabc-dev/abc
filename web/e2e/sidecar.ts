// A stand-in for the Tailscale sidecar. Like Serve, it answers the browser over HTTPS on the
// server's public origin and forwards each request to abc's sidecar socket with the headers Serve
// sets for a signed-in person, as abc-server's own integration tests do. It passes the browser's
// `Host` on as `X-Forwarded-Host` and leaves `Origin` as the browser sent it, so the server checks
// both as it does behind Serve. `cargo xtask e2e` names the socket, the origin and the
// certificate it made for the run.

import { readFileSync } from "node:fs";
import { type OutgoingHttpHeaders, request } from "node:http";
import { createServer } from "node:https";

import { LOGIN } from "./identity.ts";

function required(name: string): string {
  const value = process.env[name];
  if (value === undefined || value === "") {
    throw new Error(`${name} is not set; run the end-to-end tests with \`cargo xtask e2e\``);
  }
  return value;
}

const socket = required("ABC_E2E_SOCKET");
const origin = new URL(required("ABC_E2E_ORIGIN"));
const tls = {
  cert: readFileSync(required("ABC_E2E_TLS_CERT")),
  key: readFileSync(required("ABC_E2E_TLS_KEY")),
};

createServer(tls, (incoming, outgoing) => {
  // Serve drops any identity or forwarding headers a client sends and sets its own.
  const headers: OutgoingHttpHeaders = { ...incoming.headers };
  for (const name of Object.keys(headers)) {
    if (name.startsWith("tailscale-") || name.startsWith("x-forwarded-")) {
      delete headers[name];
    }
  }
  const host = incoming.headers.host;
  const forwarded = request(
    {
      socketPath: socket,
      method: incoming.method,
      path: incoming.url,
      headers: {
        ...headers,
        host: "localhost",
        // The `Host` the browser sent, as Serve forwards it; none when it sent none.
        ...(host === undefined ? {} : { "x-forwarded-host": host }),
        "x-forwarded-proto": "https",
        "tailscale-user-login": LOGIN,
      },
    },
    (answer) => {
      outgoing.writeHead(answer.statusCode ?? 502, answer.headers);
      answer.pipe(outgoing);
    },
  );
  forwarded.on("error", (error) => {
    outgoing.writeHead(502, { "content-type": "text/plain" });
    outgoing.end(`the sidecar stand-in could not reach abc: ${error.message}`);
  });
  incoming.pipe(forwarded);
}).listen(Number(origin.port), "127.0.0.1");
