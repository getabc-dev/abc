// Approving a machine: the page a machine's `abc worker setup` sends the owner to. It shows who is
// asking and the code the machine's terminal shows, and the owner approves or denies. Every rule
// is here; the server decides, and its answer (`enrolment_request_*`) settles a request that was
// decided or expired while the page was open.

import { type ApiClient, type ApiError, asApiError } from "../api/client";
import { type Dispatch, type Program, type Step, settle } from "../elm";
import type { EnrolmentRequestId } from "../generated/EnrolmentRequestId";
import type { RequestCard } from "../generated/RequestCard";
import type { WorkspaceId } from "../generated/WorkspaceId";

/** How a request ended, when it is no longer waiting. */
export type Outcome = "approved" | "denied" | "expired";

type Stage =
  | { readonly state: "loading" }
  /** The request could not be read: it does not exist for this person, or the server failed. */
  | { readonly state: "failed"; readonly error: ApiError }
  /** Waiting for the owner. `deciding` is the answer being sent. */
  | {
      readonly state: "open";
      readonly card: RequestCard;
      readonly deciding: "approve" | "deny" | null;
      readonly error: string | null;
    }
  | { readonly state: "closed"; readonly card: RequestCard; readonly outcome: Outcome };

export type Model = {
  readonly workspace: WorkspaceId;
  readonly request: EnrolmentRequestId;
  readonly stage: Stage;
};

export type Message =
  | { readonly type: "loaded"; readonly card: RequestCard }
  | { readonly type: "load_failed"; readonly error: ApiError }
  | { readonly type: "approve" }
  | { readonly type: "deny" }
  /** The server took the owner's answer. */
  | { readonly type: "decided"; readonly outcome: "approved" | "denied" }
  | { readonly type: "decision_refused"; readonly error: ApiError }
  | { readonly type: "retry" };

export type Effect =
  | { readonly type: "load"; readonly workspace: WorkspaceId; readonly request: EnrolmentRequestId }
  | {
      readonly type: "decide";
      readonly workspace: WorkspaceId;
      readonly request: EnrolmentRequestId;
      readonly decision: "approve" | "deny";
    };

export function init(workspace: WorkspaceId, request: EnrolmentRequestId): Step<Model, Effect> {
  return [
    { workspace, request, stage: { state: "loading" } },
    [{ type: "load", workspace, request }],
  ];
}

/** Where a loaded card stands: waiting, or how it ended. */
function staged(card: RequestCard): Stage {
  switch (card.status.status) {
    case "pending":
      return { state: "open", card, deciding: null, error: null };
    case "approved":
      return { state: "closed", card, outcome: "approved" };
    case "denied":
      return { state: "closed", card, outcome: "denied" };
    case "expired":
      return { state: "closed", card, outcome: "expired" };
  }
}

/** The outcome an API refusal says the request already has, if it does. */
function endedBy(code: ApiError["code"]): Outcome | null {
  if (code === "enrolment_request_expired") {
    return "expired";
  }
  if (code === "enrolment_request_denied") {
    return "denied";
  }
  if (code === "enrolment_request_approved") {
    return "approved";
  }
  return null;
}

export function update(model: Model, message: Message): Step<Model, Effect> {
  const { stage } = model;
  switch (message.type) {
    case "loaded":
      return [{ ...model, stage: staged(message.card) }, []];
    case "load_failed":
      return [{ ...model, stage: { state: "failed", error: message.error } }, []];
    case "retry":
      if (stage.state !== "failed") {
        return [model, []];
      }
      return [
        { ...model, stage: { state: "loading" } },
        [{ type: "load", workspace: model.workspace, request: model.request }],
      ];
    case "approve":
    case "deny": {
      // One answer at a time, and only to a request that is waiting.
      if (stage.state !== "open" || stage.deciding !== null) {
        return [model, []];
      }
      const decision = message.type;
      return [
        { ...model, stage: { ...stage, deciding: decision, error: null } },
        [{ type: "decide", workspace: model.workspace, request: model.request, decision }],
      ];
    }
    case "decided":
      if (stage.state !== "open") {
        return [model, []];
      }
      return [
        { ...model, stage: { state: "closed", card: stage.card, outcome: message.outcome } },
        [],
      ];
    case "decision_refused": {
      if (stage.state !== "open") {
        return [model, []];
      }
      // Decided or expired while the page was open: say how it ended, from the server.
      const outcome = endedBy(message.error.code);
      if (outcome !== null) {
        return [{ ...model, stage: { state: "closed", card: stage.card, outcome } }, []];
      }
      return [{ ...model, stage: { ...stage, deciding: null, error: message.error.message } }, []];
    }
  }
}

/** The Approve page of `request` in `workspace`, over `api`. */
export function approval(
  api: ApiClient,
  workspace: WorkspaceId,
  request: EnrolmentRequestId,
): Program<Model, Message, Effect> {
  return {
    init: () => init(workspace, request),
    update,
    perform: (effect: Effect, dispatch: Dispatch<Message>, signal: AbortSignal) => {
      switch (effect.type) {
        case "load":
          void settle(
            api.enrolmentRequest(effect.workspace, effect.request, signal),
            (card): Message => ({ type: "loaded", card }),
            (error): Message => ({ type: "load_failed", error: asApiError(error) }),
            dispatch,
          );
          return;
        case "decide": {
          const approving = effect.decision === "approve";
          void settle(
            approving
              ? api.approveEnrolmentRequest(effect.workspace, effect.request, signal)
              : api.denyEnrolmentRequest(effect.workspace, effect.request, signal),
            (): Message => ({ type: "decided", outcome: approving ? "approved" : "denied" }),
            (error): Message => ({ type: "decision_refused", error: asApiError(error) }),
            dispatch,
          );
          return;
        }
      }
    },
  };
}
