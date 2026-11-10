// The workspace's machines, live: the program behind the shell's machine pills and the Machines
// screen. It follows `GET /api/workspaces/{ws}/live` and keeps the cards. When the stream drops
// after it has shown cards, it resumes from the last position it saw (`Last-Event-ID`), waiting
// longer after each failed attempt; a refusal that a person must answer stops it until they retry.

import { type ApiClient, type ApiError, asApiError } from "../api/client";
import { after, type Dispatch, type Program, type Step } from "../elm";
import type { LiveUpdate } from "../generated/LiveUpdate";
import type { MachineCard } from "../generated/MachineCard";
import type { Position } from "../generated/Position";
import type { RequestCard } from "../generated/RequestCard";
import type { WorkspaceId } from "../generated/WorkspaceId";

/** Why the stream stopped: the API's code, or that access ended, and words for the person. */
type Problem = {
  readonly code: ApiError["code"] | "access_ended";
  readonly message: string;
};

export type Connection =
  /** Opening the stream: at first, or again after the person retried. */
  | { readonly state: "connecting" }
  /** Following the stream. */
  | { readonly state: "live" }
  /** The stream dropped; `attempt` tries so far to resume from `head`. */
  | { readonly state: "reconnecting"; readonly attempt: number; readonly problem: Problem }
  /** Stopped until the person retries. */
  | { readonly state: "failed"; readonly problem: Problem };

export type Model = {
  readonly workspace: WorkspaceId;
  readonly connection: Connection;
  /** The last position the cards reflect, once a first update arrived. */
  readonly head: Position | null;
  /** Every machine, in display order: by name, then id. */
  readonly machines: readonly MachineCard[];
  /** The enrolment requests still waiting for the owner, oldest first. */
  readonly requests: readonly RequestCard[];
};

export type Message =
  | { readonly type: "received"; readonly update: LiveUpdate }
  /** The server ended the stream, as it does when it shuts down. */
  | { readonly type: "ended" }
  | { readonly type: "failed"; readonly error: ApiError }
  /** The wait before resuming is over. */
  | { readonly type: "resume" }
  /** The person asked to try again. */
  | { readonly type: "retry" };

export type Effect =
  | { readonly type: "connect"; readonly workspace: WorkspaceId; readonly after: Position | null }
  | { readonly type: "wait"; readonly ms: number };

/**
 * Whether `machine` can take work: enrolled and not revoked. The one definition the shell's pills
 * and the Machines screen's "first machine" both follow.
 */
export function usable(machine: MachineCard): boolean {
  return !machine.revoked;
}

/** The wait before resume attempt `attempt` (from 1): half a second, doubling, at most 8. */
export function backoff(attempt: number): number {
  return Math.min(500 * 2 ** (attempt - 1), 8000);
}

/**
 * Whether the stream may come back by itself: no answer, the server busy or restarting (a 5xx,
 * which the sidecar also answers while the server is down) or rate-limited.
 */
function transient(error: ApiError): boolean {
  return error.status === null || error.status >= 500 || error.code === "rate_limited";
}

function byName(first: MachineCard, second: MachineCard): number {
  return first.name.localeCompare(second.name) || first.worker.localeCompare(second.worker);
}

/** `machines` with `changed` replacing the cards of the same workers, in display order. */
function merge(
  machines: readonly MachineCard[],
  changed: readonly MachineCard[],
): readonly MachineCard[] {
  const byWorker = new Map(machines.map((card) => [card.worker, card]));
  for (const card of changed) {
    byWorker.set(card.worker, card);
  }
  return [...byWorker.values()].toSorted(byName);
}

/**
 * `requests` with `changed` applied: a request that is pending is kept (or added), and one that
 * was decided or expired leaves, so the notice goes once the owner has answered, on any device.
 */
function mergeRequests(
  requests: readonly RequestCard[],
  changed: readonly RequestCard[],
): readonly RequestCard[] {
  const byId = new Map(requests.map((request) => [request.id, request]));
  for (const request of changed) {
    if (request.status.status === "pending") {
      byId.set(request.id, request);
    } else {
      byId.delete(request.id);
    }
  }
  return [...byId.values()].toSorted(
    (first, second) =>
      first.requested_at.localeCompare(second.requested_at) || first.id.localeCompare(second.id),
  );
}

export function init(workspace: WorkspaceId): Step<Model, Effect> {
  return [
    { workspace, connection: { state: "connecting" }, head: null, machines: [], requests: [] },
    [{ type: "connect", workspace, after: null }],
  ];
}

/** Resumes after a drop: at once the first time, then after a growing wait. */
function dropped(model: Model, problem: Problem): Step<Model, Effect> {
  const attempt = model.connection.state === "reconnecting" ? model.connection.attempt + 1 : 1;
  return [
    { ...model, connection: { state: "reconnecting", attempt, problem } },
    [{ type: "wait", ms: backoff(attempt) }],
  ];
}

/** The model after the stream's `live` update. */
function receive(model: Model, live: LiveUpdate): Step<Model, Effect> {
  switch (live.type) {
    case "snapshot":
      return [
        {
          ...model,
          connection: { state: "live" },
          head: live.head,
          machines: [...live.machines].toSorted(byName),
          requests: mergeRequests([], live.requests),
        },
        [],
      ];
    case "machines":
      return [
        {
          ...model,
          connection: { state: "live" },
          head: live.head,
          machines: merge(model.machines, live.changed),
          requests: mergeRequests(model.requests, live.requests),
        },
        [],
      ];
    case "access_ended":
      return [
        {
          ...model,
          connection: {
            state: "failed",
            problem: { code: "access_ended", message: "Your access to this workspace ended." },
          },
        },
        [],
      ];
  }
}

export function update(model: Model, message: Message): Step<Model, Effect> {
  switch (message.type) {
    case "received":
      return receive(model, message.update);
    case "ended":
    case "failed": {
      // A failed state is final until the person retries: the end that follows ended access, or
      // a failure racing it, resumes nothing.
      if (model.connection.state === "failed") {
        return [model, []];
      }
      const problem: Problem =
        message.type === "ended"
          ? { code: "network", message: "the server closed the connection" }
          : { code: message.error.code, message: message.error.message };
      // Before any card arrived there is nothing to resume, and the person decides.
      if (model.head === null || (message.type === "failed" && !transient(message.error))) {
        return [{ ...model, connection: { state: "failed", problem } }, []];
      }
      return dropped(model, problem);
    }
    case "resume":
      if (model.connection.state !== "reconnecting") {
        return [model, []];
      }
      return [model, [{ type: "connect", workspace: model.workspace, after: model.head }]];
    case "retry":
      if (model.connection.state !== "failed") {
        return [model, []];
      }
      return [
        { ...model, connection: { state: "connecting" } },
        [{ type: "connect", workspace: model.workspace, after: model.head }],
      ];
  }
}

/** Follows the stream until it ends, fails or the screen goes. */
async function connect(
  api: ApiClient,
  effect: Extract<Effect, { type: "connect" }>,
  dispatch: Dispatch<Message>,
  signal: AbortSignal,
): Promise<void> {
  try {
    const updates = await api.live(effect.workspace, effect.after, signal);
    for await (const live of updates) {
      dispatch({ type: "received", update: live });
    }
    dispatch({ type: "ended" });
  } catch (error: unknown) {
    // Aborted because the screen went: nothing listens any more.
    if (!signal.aborted) {
      dispatch({ type: "failed", error: asApiError(error) });
    }
  }
}

/** The live machines of `workspace`, over `api`. */
export function liveMachines(
  api: ApiClient,
  workspace: WorkspaceId,
): Program<Model, Message, Effect> {
  return {
    init: () => init(workspace),
    update,
    perform: (effect, dispatch, signal) => {
      switch (effect.type) {
        case "connect":
          void connect(api, effect, dispatch, signal);
          return;
        case "wait":
          after(effect.ms, { type: "resume" }, dispatch, signal);
          return;
      }
    },
  };
}
