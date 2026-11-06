// What the tests share: example API values. Only tests import this file.

import type { MachineCard } from "./generated/MachineCard";

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
