// The claim screen: the view of the claim program (claim.ts) and the effects it asks for. There is
// no claim on the design canvas; it follows the canvas's language: one calm card on the ground.

import { useQueryClient } from "@tanstack/react-query";
import { useRouter } from "@tanstack/react-router";
import { Form } from "react-aria-components";

import { type ApiClient, asApiError } from "../api/client";
import { type Program, settle, useProgram } from "../elm";
import type { TailnetLogin } from "../generated/TailnetLogin";
import { Centered } from "../session/Centered";
import { Button } from "../ui/Button";
import { Card } from "../ui/Card";
import { TextField } from "../ui/TextField";
import { type Effect, init, type Message, type Model, update } from "./claim";

/** The claim program, claiming over `api` and going home through `goHome`. */
function claiming(api: ApiClient, goHome: () => void): Program<Model, Message, Effect> {
  return {
    init,
    update,
    perform: (effect, dispatch, signal) => {
      switch (effect.type) {
        case "claim":
          void settle(
            api.claim(effect.request, signal),
            (): Message => ({ type: "claimed" }),
            (error): Message => ({ type: "refused", error: asApiError(error) }),
            dispatch,
          );
          return;
        case "go_home":
          goHome();
          return;
      }
    },
  };
}

/** A command, set in mono, for the person to run. */
function Command({ children }: { readonly children: string }) {
  return (
    <code className="rounded-chip bg-sunken px-1 py-0.5 font-mono text-ink text-sm">
      {children}
    </code>
  );
}

export function ClaimScreen({
  api,
  login,
}: {
  readonly api: ApiClient;
  readonly login: TailnetLogin;
}) {
  const router = useRouter();
  const queryClient = useQueryClient();
  const [model, dispatch] = useProgram(
    claiming(api, () => {
      // Home first: once the session says "member", this screen goes and the app shows the route.
      void router.navigate({ to: "/" }).then(() => queryClient.invalidateQueries());
    }),
  );
  const { phase, fields } = model;
  switch (phase.state) {
    case "expired":
      return (
        <Centered>
          <Card title="This setup code has expired" level={1}>
            <div className="flex flex-col items-start gap-4">
              <p className="text-ink-muted">
                A setup code works for an hour. Run <Command>abc server setup-code</Command> on the
                server's host again for a new one.
              </p>
              <Button variant="primary" onPress={() => dispatch({ type: "start_over" })}>
                Enter a new code
              </Button>
            </div>
          </Card>
        </Centered>
      );
    case "already_claimed":
      return (
        <Centered>
          <Card title="This server already has an owner" level={1}>
            <div className="flex flex-col items-start gap-4">
              <p className="text-ink-muted">
                Someone claimed it while this page was open, perhaps you in another tab.
              </p>
              <Button variant="primary" onPress={() => dispatch({ type: "continue" })}>
                Continue
              </Button>
            </div>
          </Card>
        </Centered>
      );
    case "claimed":
      return (
        <Centered>
          <output className="block text-center text-ink-muted">
            Claimed. Opening your workspace…
          </output>
        </Centered>
      );
    case "editing":
    case "claiming": {
      const errors = phase.state === "editing" ? phase.errors : {};
      const problem = phase.state === "editing" ? phase.problem : null;
      return (
        <Centered>
          <Card title="Claim this server" level={1}>
            <div className="flex flex-col gap-5">
              <p className="text-ink-muted">
                You are signed in as <span className="font-medium text-ink">{login}</span>, and
                become this server's owner. Run <Command>abc server setup-code</Command> on its host
                for the code.
              </p>
              <Form
                className="flex flex-col gap-4"
                onSubmit={(event) => {
                  event.preventDefault();
                  dispatch({ type: "submitted" });
                }}
              >
                <TextField
                  label="Setup code"
                  name="code"
                  autoComplete="one-time-code"
                  mono
                  value={fields.code}
                  onChange={(value) => dispatch({ type: "edited", field: "code", value })}
                  error={errors.code}
                />
                <TextField
                  label="Your name"
                  name="name"
                  autoComplete="name"
                  value={fields.displayName}
                  onChange={(value) => dispatch({ type: "edited", field: "displayName", value })}
                  error={errors.displayName}
                />
                <TextField
                  label="Workspace name"
                  name="workspace"
                  description="Your first workspace. Everything in abc belongs to one."
                  value={fields.workspaceName}
                  onChange={(value) => dispatch({ type: "edited", field: "workspaceName", value })}
                  error={errors.workspaceName}
                />
                {problem === null ? null : (
                  <p role="alert" className="rounded-control bg-danger-tint px-3 py-2 text-danger">
                    {problem}
                  </p>
                )}
                <Button type="submit" variant="primary" isPending={phase.state === "claiming"}>
                  {phase.state === "claiming" ? "Claiming…" : "Claim this server"}
                </Button>
              </Form>
            </div>
          </Card>
        </Centered>
      );
    }
  }
}
