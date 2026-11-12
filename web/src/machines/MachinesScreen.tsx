// The Machines screen: a card per machine, live from the workspace's stream (live.ts, run by the
// shell), and the enrolment panel (enrol.ts). Its states: loading, error with retry, empty (how to
// enrol the first machine), live, and offline while the stream resumes.

import { Link } from "@tanstack/react-router";
import { LaptopIcon, ServerIcon, ShieldQuestionIcon } from "lucide-react";
import { type ReactNode, useEffect, useMemo, useState } from "react";

import { CopyableText } from "@/components/CopyableText";
import { Status, type Tone } from "@/components/Status";
import { TextField } from "@/components/TextField";
import { TitledCard } from "@/components/TitledCard";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button, buttonVariants } from "@/components/ui/button";

import { useProgram } from "../elm";
import type { MachineCard } from "../generated/MachineCard";
import type { RequestCard } from "../generated/RequestCard";
import { useWorkspace } from "../workspace/SignedIn";
import { type Message as EnrolMessage, type Model as Enrolment, enrolment } from "./enrol";
import { describeMachine, formatInstant } from "./format";
import { type Connection, usable } from "./live";

function connectionPill(connection: Connection): { tone: Tone; label: string } {
  switch (connection.state) {
    case "connecting":
      return { tone: "neutral", label: "Connecting" };
    case "live":
      return { tone: "success", label: "Live" };
    case "reconnecting":
      return { tone: "warning", label: "Reconnecting" };
    case "failed":
      return { tone: "danger", label: "Stopped" };
  }
}

function machinePill(machine: MachineCard): { tone: Tone; label: string } {
  if (machine.revoked) {
    return { tone: "danger", label: "Revoked" };
  }
  return machine.online
    ? { tone: "success", label: "Online" }
    : { tone: "neutral", label: "Offline" };
}

function Machine({ machine }: { readonly machine: MachineCard }) {
  const pill = machinePill(machine);
  return (
    <TitledCard
      title={machine.name}
      status={pill}
      className={machine.revoked ? "opacity-70" : undefined}
    >
      <div className="flex items-center gap-3">
        <span className="flex size-9 shrink-0 items-center justify-center rounded-lg border bg-sunken text-muted-foreground">
          {machine.os === "macos" ? (
            <LaptopIcon aria-hidden="true" className="size-4.5" />
          ) : (
            <ServerIcon aria-hidden="true" className="size-4.5" />
          )}
        </span>
        <div className="min-w-0">
          <p className="text-foreground">{describeMachine(machine)}</p>
          <p className="truncate font-mono text-subtle-foreground text-xs">{machine.hostname}</p>
        </div>
      </div>
      <dl className="mt-4 grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 border-t pt-3 text-xs">
        <dt className="font-mono text-2xs text-subtle-foreground uppercase tracking-label">
          Last seen
        </dt>
        <dd className="text-muted-foreground">
          {machine.online
            ? "Now"
            : machine.last_seen_at === null
              ? "Never connected"
              : formatInstant(machine.last_seen_at)}
        </dd>
        <dt className="font-mono text-2xs text-subtle-foreground uppercase tracking-label">
          Enrolled
        </dt>
        <dd className="text-muted-foreground">{formatInstant(machine.enrolled_at)}</dd>
        <dt className="font-mono text-2xs text-subtle-foreground uppercase tracking-label">
          Release
        </dt>
        <dd className="font-mono text-muted-foreground">{machine.build}</dd>
      </dl>
    </TitledCard>
  );
}

/** A machine asking to join, with a way to its Approve page: shown until the owner answers. */
function RequestNotice({
  request,
  workspace,
}: {
  readonly request: RequestCard;
  readonly workspace: string;
}) {
  const params = useMemo(() => ({ workspace, request: request.id }), [workspace, request.id]);
  return (
    <Alert className="border-warning bg-warning-tint">
      <ShieldQuestionIcon aria-hidden="true" />
      <AlertTitle className="text-warning-foreground">{request.hostname} wants to join</AlertTitle>
      <AlertDescription className="flex flex-wrap items-center justify-between gap-3 text-warning-foreground">
        <p>
          {describeMachine(request)}, asked by {request.login}. Its code is{" "}
          <span className="font-mono font-semibold">{request.code}</span>.
        </p>
        <Link
          to="/machines/requests/$workspace/$request"
          params={params}
          className={buttonVariants({ size: "sm" })}
        >
          Review request
        </Link>
      </AlertDescription>
    </Alert>
  );
}

function EnrolPanel({
  first,
  origin,
  model,
  dispatch,
}: {
  /** Whether no machine is enrolled yet. */
  readonly first: boolean;
  readonly origin: string;
  readonly model: Enrolment;
  readonly dispatch: (message: EnrolMessage) => void;
}) {
  const { step } = model;
  const title = first ? "Enrol your first machine" : "Enrol a machine";
  if (step.state === "issued") {
    return (
      <TitledCard title={title} className="w-full max-w-xl">
        <ol className="flex list-decimal flex-col gap-4 pl-5 text-foreground">
          <li>
            On the machine, with abc installed and Tailscale running, run:
            <CopyableText value={`abc worker enrol ${origin}`} label="the enrol command" />
          </li>
          <li>
            When it asks, paste this token (a script can pass it with <code>--token-file</code>). It
            works once, until {formatInstant(step.token.expires_at)}:
            <CopyableText value={step.token.token} label="the token" />
          </li>
          <li>
            Then start the worker, which keeps the machine online:
            <CopyableText value="abc worker" label="the worker command" />
          </li>
        </ol>
        <p className="mt-4 text-muted-foreground text-xs">
          abc shows the token only now and does not keep it. The machine appears here once it
          enrols.
        </p>
        <Button variant="outline" className="mt-4" onClick={() => dispatch({ type: "closed" })}>
          Done
        </Button>
      </TitledCard>
    );
  }
  const error = step.state === "asking" ? step.error : null;
  return (
    <TitledCard title={title} className="w-full max-w-xl">
      <div className="flex flex-col gap-4">
        <div className="flex flex-col gap-2">
          <p className="text-muted-foreground">
            A machine runs abc's agents for you. On it, with abc installed and signed in to your
            tailnet, run this command. It opens your browser to approve the machine, and leaves a
            worker running that starts again after a reboot.
          </p>
          <CopyableText value={`abc worker setup ${origin}`} label="the setup command" />
        </div>
        <div className="flex flex-col gap-4 border-t pt-4">
          <p className="text-muted-foreground">
            A machine with no browser, or one that is not on your tailnet's login: create a one-time
            token and enrol it with <code>abc worker enrol</code>.
          </p>
          <TextField
            label="Machine name"
            description="Shown on its card. Leave it blank to use the machine's hostname."
            value={model.label}
            disabled={step.state === "issuing"}
            onChange={(label) => dispatch({ type: "labelled", label })}
            error={error ?? undefined}
          />
          <div className="flex gap-2">
            <Button
              disabled={step.state === "issuing"}
              focusableWhenDisabled
              onClick={() => dispatch({ type: "requested" })}
            >
              Create enrolment token
            </Button>
            {first ? null : (
              <Button variant="outline" onClick={() => dispatch({ type: "closed" })}>
                Cancel
              </Button>
            )}
          </div>
        </div>
      </div>
    </TitledCard>
  );
}

/** How often the screen drops the notices of requests that lapsed. */
const CLOCK_MS = 15_000;

/** The time, refreshed as it passes, for what lapses while the screen is open. */
function useNow(): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = setInterval(() => {
      setNow(Date.now());
    }, CLOCK_MS);
    return () => {
      clearInterval(timer);
    };
  }, []);
  return now;
}

export function MachinesScreen() {
  const { api, workspace, origin, live, dispatch } = useWorkspace();
  const [enrol, enrolDispatch] = useProgram(enrolment(api, workspace.id));
  const { connection, machines, requests, head } = live;
  const now = useNow();
  const pill = connectionPill(connection);
  const retry = <Button onClick={() => dispatch({ type: "retry" })}>Try again</Button>;
  let body: ReactNode;
  if (head === null && connection.state === "failed") {
    body = (
      <TitledCard title="The machines could not be loaded">
        <div className="flex flex-col items-start gap-4">
          <p className="text-muted-foreground">{connection.problem.message}</p>
          {retry}
        </div>
      </TitledCard>
    );
  } else if (head === null) {
    body = <output className="block text-muted-foreground">Loading machines…</output>;
  } else {
    // Revoked machines stay listed, but only a usable one means the first is enrolled.
    const first = !machines.some(usable);
    body = (
      <div className="flex flex-col gap-4">
        {connection.state === "reconnecting" ? (
          <Alert className="border-warning bg-warning-tint text-warning-foreground">
            <AlertDescription className="text-warning-foreground">
              Offline: the connection to the server dropped ({connection.problem.message}). Resuming
              where it stopped; the cards may be out of date.
            </AlertDescription>
          </Alert>
        ) : null}
        {connection.state === "failed" ? (
          <Alert variant="destructive" className="bg-destructive-tint">
            <AlertDescription className="flex flex-wrap items-center justify-between gap-3">
              <p>{connection.problem.message} The cards may be out of date.</p>
              {retry}
            </AlertDescription>
          </Alert>
        ) : null}
        {requests
          .filter((request) => Date.parse(request.expires_at) > now)
          .map((request) => (
            <RequestNotice key={request.id} request={request} workspace={workspace.id} />
          ))}
        {first || enrol.open ? (
          <EnrolPanel first={first} origin={origin} model={enrol} dispatch={enrolDispatch} />
        ) : null}
        {machines.length === 0 ? null : (
          <ul aria-label="Enrolled machines" className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
            {machines.map((machine) => (
              <li key={machine.worker}>
                <Machine machine={machine} />
              </li>
            ))}
          </ul>
        )}
      </div>
    );
  }
  return (
    <div className="flex flex-col gap-5">
      <div className="flex flex-wrap items-center gap-3">
        <h1 className="font-semibold text-xl">Machines</h1>
        <Status {...pill} />
        {head !== null && machines.some(usable) && !enrol.open ? (
          <Button
            variant="outline"
            className="ml-auto"
            onClick={() => enrolDispatch({ type: "opened" })}
          >
            Enrol a machine
          </Button>
        ) : null}
      </div>
      {body}
    </div>
  );
}
