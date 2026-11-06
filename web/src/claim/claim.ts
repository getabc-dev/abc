// Claiming an unclaimed server: the person enters the setup code `abc server setup-code` printed on
// the server's host, their name and their first workspace's name, and becomes the owner
// (`POST /api/owner/claim`). Each refusal the claim can meet has a state of its own.

import type { ApiError } from "../api/client";
import type { Step } from "../elm";
import type { ClaimRequest } from "../generated/ClaimRequest";

/** The form's fields, as typed. */
export type Fields = {
  readonly code: string;
  readonly displayName: string;
  readonly workspaceName: string;
};

type Field = keyof Fields;

/** What is wrong with each field, if anything. */
export type FieldErrors = Partial<Record<Field, string>>;

type Phase =
  /** Filling the form in, with what is wrong with it and a problem the whole form met. */
  | {
      readonly state: "editing";
      readonly errors: FieldErrors;
      readonly problem: string | null;
    }
  | { readonly state: "claiming" }
  /** The code was right but has expired: the host must print a new one. */
  | { readonly state: "expired" }
  /** Someone claimed the server first, perhaps this person in another tab. */
  | { readonly state: "already_claimed" }
  /** Claimed: the app is reloading who the visitor is. */
  | { readonly state: "claimed" };

export type Model = { readonly fields: Fields; readonly phase: Phase };

export type Message =
  | { readonly type: "edited"; readonly field: Field; readonly value: string }
  | { readonly type: "submitted" }
  | { readonly type: "claimed" }
  | { readonly type: "refused"; readonly error: ApiError }
  /** From the expired state: enter a new code. */
  | { readonly type: "start_over" }
  /** From the already-claimed state: ask the server who the visitor is now. */
  | { readonly type: "continue" };

export type Effect =
  | { readonly type: "claim"; readonly request: ClaimRequest }
  /** Reload who the visitor is and go home, where the app now shows what they may see. */
  | { readonly type: "go_home" };

/** The longest name the server accepts, in characters (`NAME_MAX` in abc-core). */
const NAME_MAX = 64;

const editing: Phase = { state: "editing", errors: {}, problem: null };

export function init(): Step<Model, Effect> {
  return [{ fields: { code: "", displayName: "", workspaceName: "" }, phase: editing }, []];
}

/** A name's problem, by the server's rule: 1 to 64 characters after trimming. */
function nameError(value: string, what: string): string | undefined {
  // Characters as the server counts them: code points.
  const length = Array.from(value.trim()).length;
  if (length === 0) {
    return `Enter ${what}.`;
  }
  if (length > NAME_MAX) {
    return `Use at most ${NAME_MAX} characters for ${what}.`;
  }
  return undefined;
}

/** What is wrong with `fields`, before asking the server. */
export function validate(fields: Fields): FieldErrors {
  const errors: FieldErrors = {};
  if (fields.code.trim() === "") {
    errors.code = "Enter the setup code from the server's host.";
  }
  const displayName = nameError(fields.displayName, "your name");
  if (displayName !== undefined) {
    errors.displayName = displayName;
  }
  const workspaceName = nameError(fields.workspaceName, "a workspace name");
  if (workspaceName !== undefined) {
    errors.workspaceName = workspaceName;
  }
  return errors;
}

/** The phase a refusal leads to: its own state, the code field's error, or the form's problem. */
function refused(error: ApiError): Phase {
  if (error.code === "setup_code_expired") {
    return { state: "expired" };
  }
  if (error.code === "server_already_claimed") {
    return { state: "already_claimed" };
  }
  if (error.code === "setup_code_mismatch") {
    return {
      state: "editing",
      errors: {
        code: "This is not the server's current setup code. Check it, or print a new one.",
      },
      problem: null,
    };
  }
  if (error.code === "setup_code_required") {
    return {
      state: "editing",
      errors: {
        code: "This server has not printed a setup code yet. Run abc server setup-code on its host.",
      },
      problem: null,
    };
  }
  return { state: "editing", errors: {}, problem: error.message };
}

export function update(model: Model, message: Message): Step<Model, Effect> {
  switch (message.type) {
    case "edited": {
      if (model.phase.state !== "editing") {
        return [model, []];
      }
      const { [message.field]: _fixed, ...errors } = model.phase.errors;
      return [
        {
          fields: { ...model.fields, [message.field]: message.value },
          phase: { state: "editing", errors, problem: null },
        },
        [],
      ];
    }
    case "submitted": {
      if (model.phase.state !== "editing") {
        return [model, []];
      }
      const errors = validate(model.fields);
      if (Object.keys(errors).length > 0) {
        return [{ ...model, phase: { state: "editing", errors, problem: null } }, []];
      }
      const request: ClaimRequest = {
        code: model.fields.code.trim(),
        display_name: model.fields.displayName.trim(),
        workspace_name: model.fields.workspaceName.trim(),
      };
      return [{ ...model, phase: { state: "claiming" } }, [{ type: "claim", request }]];
    }
    case "claimed":
      return [{ ...model, phase: { state: "claimed" } }, [{ type: "go_home" }]];
    case "refused":
      if (model.phase.state !== "claiming") {
        return [model, []];
      }
      return [{ ...model, phase: refused(message.error) }, []];
    case "start_over":
      if (model.phase.state !== "expired") {
        return [model, []];
      }
      return [{ fields: { ...model.fields, code: "" }, phase: editing }, []];
    case "continue":
      if (model.phase.state !== "already_claimed") {
        return [model, []];
      }
      return [model, [{ type: "go_home" }]];
  }
}
