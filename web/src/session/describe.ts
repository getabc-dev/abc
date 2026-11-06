// What the home route says about the visitor, from the server's answer to `GET /api/session`.

import type { SessionView } from "../generated/SessionView";

export type SessionSummary = {
  readonly title: string;
  readonly detail: string;
};

function plural(count: number, one: string, many: string): string {
  return `${count} ${count === 1 ? one : many}`;
}

/** The visitor's standing on this server, in words. */
export function describeSession(view: SessionView): SessionSummary {
  switch (view.state) {
    case "no_identity":
      return {
        title: "No tailnet identity",
        detail:
          "This device reaches abc without a signed-in Tailscale user, as tagged devices do. Open abc from a device signed in as a person.",
      };
    case "unclaimed":
      return {
        title: "This server is unclaimed",
        detail: `You are signed in as ${view.login}. Run \`abc server setup-code\` on the server's host for a one-time code to claim it.`,
      };
    case "not_a_member":
      return {
        title: "Not a member",
        detail: `${view.login} belongs to no workspace on this server. Ask its owner to add you.`,
      };
    case "member":
      return {
        title: `Signed in as ${view.display_name}`,
        detail: `You belong to ${plural(view.workspaces.length, "workspace", "workspaces")}.`,
      };
  }
}
