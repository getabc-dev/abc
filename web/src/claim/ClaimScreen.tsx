// The claim screen: the view of the claim program (claim.ts) and the effects it asks for. There is
// no claim on the design canvas; it follows the canvas's language: one calm card on the ground.

import { useQueryClient } from "@tanstack/react-query";
import { useRouter } from "@tanstack/react-router";

import { TextField } from "@/components/TextField";
import { TitledCard } from "@/components/TitledCard";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";

import { type ApiClient, asApiError } from "../api/client";
import { type Program, settle, useProgram } from "../elm";
import type { TailnetLogin } from "../generated/TailnetLogin";
import { Centered } from "../session/Centered";
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
function CommandText({ children }: { readonly children: string }) {
  return (
    <code className="rounded-sm bg-muted px-1 py-0.5 font-mono text-foreground text-xs">
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
          <TitledCard title="This setup code has expired" level={1}>
            <div className="flex flex-col items-start gap-4">
              <p className="text-muted-foreground">
                A setup code works for an hour. Run <CommandText>abc server setup-code</CommandText>{" "}
                on the server's host again for a new one.
              </p>
              <Button onClick={() => dispatch({ type: "start_over" })}>Enter a new code</Button>
            </div>
          </TitledCard>
        </Centered>
      );
    case "already_claimed":
      return (
        <Centered>
          <TitledCard title="This server already has an owner" level={1}>
            <div className="flex flex-col items-start gap-4">
              <p className="text-muted-foreground">
                Someone claimed it while this page was open, perhaps you in another tab.
              </p>
              <Button onClick={() => dispatch({ type: "continue" })}>Continue</Button>
            </div>
          </TitledCard>
        </Centered>
      );
    case "claimed":
      return (
        <Centered>
          <output className="block text-center text-muted-foreground">
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
          <TitledCard title="Claim this server" level={1}>
            <div className="flex flex-col gap-5">
              <p className="text-muted-foreground">
                You are signed in as <span className="font-medium text-foreground">{login}</span>,
                and become this server's owner. Run <CommandText>abc server setup-code</CommandText>{" "}
                on its host for the code.
              </p>
              <form
                noValidate
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
                  <Alert variant="destructive">
                    <AlertDescription>{problem}</AlertDescription>
                  </Alert>
                )}
                <Button type="submit" disabled={phase.state === "claiming"} focusableWhenDisabled>
                  {phase.state === "claiming" ? "Claiming…" : "Claim this server"}
                </Button>
              </form>
            </div>
          </TitledCard>
        </Centered>
      );
    }
  }
}
