// The server's rule for a name (a display name, a workspace name or a machine label): 1 to 64
// characters after trimming, counted as code points, with no control characters. The bounds come
// from the JSON Schema generated from Rust (`NAME_MAX` in abc-core), never copied here, so the
// form says what the server will say.

import schema from "./generated/web-api.schema.json";

/** The generated types that follow the rule. */
type Name = "DisplayName" | "WorkspaceName" | "MachineLabel";

const CONTROL = /\p{Cc}/u;

/**
 * What is wrong with `raw` as a `kind`, in words that call it `what` ("your name"), or `null`
 * when the server will accept it.
 */
export function nameProblem(kind: Name, raw: string, what: string): string | null {
  const { minLength, maxLength } = schema.$defs[kind];
  const text = raw.trim();
  // Characters as the server counts them: code points.
  const length = Array.from(text).length;
  if (length < minLength) {
    return `Enter ${what}.`;
  }
  if (length > maxLength) {
    return `Use at most ${maxLength} characters for ${what}.`;
  }
  if (CONTROL.test(text)) {
    return `Remove the control characters from ${what}.`;
  }
  return null;
}
