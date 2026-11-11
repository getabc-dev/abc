// A stand-in for the Tailscale sidecar. Like Serve, it answers the browser over HTTPS on the
// server's public origin and forwards each request to abc's sidecar socket with the headers Serve
// sets for a signed-in person, as abc-server's own integration tests do. It passes the browser's
// `Host` on as `X-Forwarded-Host` and leaves `Origin` as the browser sent it, so the server checks
// both as it does behind Serve, and passes a worker's WebSocket upgrade through, as Serve does.
// `cargo xtask e2e` names the socket, the origin and the certificate it made for the run.

import { readFileSync, rmSync } from "node:fs";
import {
  createServer as createHttpServer,
  type IncomingHttpHeaders,
  type IncomingMessage,
  type OutgoingHttpHeaders,
  request,
} from "node:http";
import { createServer } from "node:https";
import { dirname, join } from "node:path";
import type { Duplex } from "node:stream";

import { LOGIN, NODE } from "./identity.ts";

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

/**
 * The headers Serve sends abc for a request with `incoming` headers, from the tailnet address
 * `peer`.
 */
function forwarded(incoming: IncomingHttpHeaders, peer: string | undefined): OutgoingHttpHeaders {
  // Serve drops any identity or forwarding headers a client sends and sets its own.
  const headers: OutgoingHttpHeaders = { ...incoming };
  for (const name of Object.keys(headers)) {
    if (name.startsWith("tailscale-") || name.startsWith("x-forwarded-")) {
      delete headers[name];
    }
  }
  const { host } = incoming;
  return {
    ...headers,
    host: "localhost",
    // The `Host` the browser sent, as Serve forwards it; none when it sent none.
    ...(host === undefined ? {} : { "x-forwarded-host": host }),
    "x-forwarded-proto": "https",
    // The address Serve accepted the connection from, which abc asks its tailscaled about.
    "x-forwarded-for": peer ?? "127.0.0.1",
    "tailscale-user-login": LOGIN,
  };
}

const sidecar = createServer(tls, (incoming, outgoing) => {
  const forward = request(
    {
      socketPath: socket,
      method: incoming.method,
      path: incoming.url,
      headers: forwarded(incoming.headers, incoming.socket.remoteAddress),
    },
    (answer) => {
      outgoing.writeHead(answer.statusCode ?? 502, answer.headers);
      answer.pipe(outgoing);
    },
  );
  forward.on("error", (error) => {
    outgoing.writeHead(502, { "content-type": "text/plain" });
    outgoing.end(`the sidecar stand-in could not reach abc: ${error.message}`);
  });
  incoming.pipe(forward);
});

// A worker's WebSocket (`GET /api/worker/connect`): Serve passes the upgrade through, and then the
// connection's bytes both ways.
sidecar.on("upgrade", (incoming: IncomingMessage, client: Duplex, head: Buffer) => {
  const forward = request({
    socketPath: socket,
    method: incoming.method,
    path: incoming.url,
    headers: forwarded(incoming.headers, incoming.socket.remoteAddress),
  });
  forward.on("upgrade", (answer, server, serverHead) => {
    const lines = [`HTTP/1.1 ${answer.statusCode ?? 101} ${answer.statusMessage ?? ""}`];
    for (let index = 0; index < answer.rawHeaders.length; index += 2) {
      lines.push(`${answer.rawHeaders[index] ?? ""}: ${answer.rawHeaders[index + 1] ?? ""}`);
    }
    client.write(`${lines.join("\r\n")}\r\n\r\n`);
    if (serverHead.length > 0) {
      client.write(serverHead);
    }
    server.pipe(client).pipe(server);
    server.on("error", () => client.destroy());
    client.on("error", () => server.destroy());
  });
  // abc refused the upgrade: pass its answer on and close.
  forward.on("response", (answer) => {
    const lines = [`HTTP/1.1 ${answer.statusCode ?? 502} ${answer.statusMessage ?? ""}`];
    for (let index = 0; index < answer.rawHeaders.length; index += 2) {
      lines.push(`${answer.rawHeaders[index] ?? ""}: ${answer.rawHeaders[index + 1] ?? ""}`);
    }
    client.write(`${lines.join("\r\n")}\r\n\r\n`);
    answer.pipe(client);
  });
  forward.on("error", () => client.destroy());
  if (head.length > 0) {
    forward.write(head);
  }
  forward.end();
});

sidecar.listen(Number(origin.port), "127.0.0.1");

// A stand-in for abc's own tailscaled: LocalAPI's `whois` over a Unix socket beside abc's, which
// the server is started with (`--tailscaled-socket`). Every address is the one node, as every
// connection to the stand-in comes from this machine, and its person is the one the stand-in
// vouches for.
const tailscaled = join(dirname(socket), "tailscaled.sock");
rmSync(tailscaled, { force: true });
createHttpServer((_incoming, outgoing) => {
  const body = JSON.stringify({
    Node: { Name: `${NODE}.` },
    UserProfile: { LoginName: LOGIN },
  });
  outgoing.writeHead(200, {
    "content-type": "application/json",
    "content-length": Buffer.byteLength(body),
  });
  outgoing.end(body);
}).listen(tailscaled);
