// What the tests share: example API values. Only tests import this file.

import type { MachineCard } from "./generated/MachineCard";
import type { RequestCard } from "./generated/RequestCard";

/** A machine named `name`, with worker id `worker`, enrolled at 09:00 UTC on 6 November 2026. */
export function card(name: string, worker: string, online: boolean): MachineCard {
  return {
    worker,
    name,
    hostname: `${name}.local`,
    os: "linux",
    arch: "x86_64",
    build: "0.1.0",
    online,
    enrolled_at: "2026-11-06T09:00:00Z",
    last_seen_at: online ? "2026-11-06T09:05:00Z" : null,
    revoked: false,
    changed_at: 3,
  };
}

/** A pending request from a MacBook called `hostname`, id `id`, asked at 09:00 UTC on 6 November 2026. */
export function enrolmentRequest(
  hostname: string,
  id: string,
  status: RequestCard["status"] = { status: "pending" },
): RequestCard {
  return {
    id,
    hostname,
    os: "macos",
    arch: "aarch64",
    build: "0.1.0",
    login: "alice@example.com",
    node: "alice-mbp.tail1234.ts.net",
    code: "K7QX-M2PD",
    requested_at: "2026-11-06T09:00:00Z",
    expires_at: "2026-11-06T09:10:00Z",
    status,
    changed_at: 4,
  };
}

/**
 * Gives jsdom, which has none, a `window.matchMedia` whose every query matches while `matching` is
 * set, as `(prefers-color-scheme: dark)` does on a dark system; `set` changes it and tells the
 * listeners, as the browser does when the system's scheme or the window's width changes.
 */
export function stubMatchMedia(matching = false): { set: (matches: boolean) => void } {
  let matches = matching;
  const listeners = new Set<() => void>();
  Reflect.set(window, "matchMedia", (media: string) => ({
    media,
    get matches() {
      return matches;
    },
    addEventListener: (_type: "change", listener: () => void) => {
      listeners.add(listener);
    },
    removeEventListener: (_type: "change", listener: () => void) => {
      listeners.delete(listener);
    },
  }));
  return {
    set(next: boolean) {
      matches = next;
      for (const listener of listeners) {
        listener();
      }
    },
  };
}
