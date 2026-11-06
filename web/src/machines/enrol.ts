// Enrolling a machine from the Machines screen: the owner asks for a one-time enrolment token,
// optionally naming the machine, and the screen shows the commands to run on it. The token lives
// in this model only while it is shown: it is never stored, and closing the panel forgets it.

import { type ApiClient, type ApiError, asApiError } from "../api/client";
import { type Dispatch, type Program, type Step, settle } from "../elm";
import type { TokenIssuedResponse } from "../generated/TokenIssuedResponse";
import type { WorkspaceId } from "../generated/WorkspaceId";

type Stage =
  | { readonly state: "asking"; readonly error: string | null }
  | { readonly state: "issuing" }
  | { readonly state: "issued"; readonly token: TokenIssuedResponse };

export type Model = {
  readonly workspace: WorkspaceId;
  /** Whether the panel is open; the screen also shows it while there is no machine. */
  readonly open: boolean;
  /** The machine's name, as typed; blank for its hostname. */
  readonly label: string;
  readonly step: Stage;
};

export type Message =
  | { readonly type: "opened" }
  | { readonly type: "labelled"; readonly label: string }
  | { readonly type: "requested" }
  | { readonly type: "issued"; readonly token: TokenIssuedResponse }
  | { readonly type: "refused"; readonly error: ApiError }
  | { readonly type: "closed" };

export type Effect = {
  readonly type: "issue";
  readonly workspace: WorkspaceId;
  readonly label: string | null;
};

/** The longest machine name the server accepts, in characters (`NAME_MAX` in abc-core). */
const LABEL_MAX = 64;

const asking: Stage = { state: "asking", error: null };

export function init(workspace: WorkspaceId): Step<Model, Effect> {
  return [{ workspace, open: false, label: "", step: asking }, []];
}

export function update(model: Model, message: Message): Step<Model, Effect> {
  switch (message.type) {
    case "opened":
      return [{ ...model, open: true }, []];
    case "labelled":
      if (model.step.state !== "asking") {
        return [model, []];
      }
      return [{ ...model, label: message.label, step: asking }, []];
    case "requested": {
      if (model.step.state !== "asking") {
        return [model, []];
      }
      const label = model.label.trim();
      if (Array.from(label).length > LABEL_MAX) {
        return [
          {
            ...model,
            step: { state: "asking", error: `Use at most ${LABEL_MAX} characters for the name.` },
          },
          [],
        ];
      }
      // Open from here on, so the commands stay up once the machine appears, until "Done".
      return [
        { ...model, open: true, step: { state: "issuing" } },
        [{ type: "issue", workspace: model.workspace, label: label === "" ? null : label }],
      ];
    }
    case "issued":
      if (model.step.state !== "issuing") {
        return [model, []];
      }
      return [{ ...model, step: { state: "issued", token: message.token } }, []];
    case "refused":
      if (model.step.state !== "issuing") {
        return [model, []];
      }
      return [{ ...model, step: { state: "asking", error: message.error.message } }, []];
    case "closed":
      return [{ ...model, open: false, label: "", step: asking }, []];
  }
}

/** The enrolment panel of `workspace`, over `api`. */
export function enrolment(api: ApiClient, workspace: WorkspaceId): Program<Model, Message, Effect> {
  return {
    init: () => init(workspace),
    update,
    perform: (effect: Effect, dispatch: Dispatch<Message>, signal: AbortSignal) => {
      void settle(
        api.issueEnrolmentToken(effect.workspace, { label: effect.label }, signal),
        (token): Message => ({ type: "issued", token }),
        (error): Message => ({ type: "refused", error: asApiError(error) }),
        dispatch,
      );
    },
  };
}
