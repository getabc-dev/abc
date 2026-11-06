// The Machines screen: a card per machine, live from the workspace's stream (live.ts, run by the
// shell), and the enrolment panel (enrol.ts). Its states: loading, error with retry, empty (how to
// enrol the first machine), live, and offline while the stream resumes.

import type { ReactNode } from "react";
import { Heading } from "react-aria-components";

import { useProgram } from "../elm";
import type { MachineCard } from "../generated/MachineCard";
import { Button } from "../ui/Button";
import { Card } from "../ui/Card";
import { LaptopIcon, ServerIcon } from "../ui/icons";
import { StatusPill, type Tone } from "../ui/StatusPill";
import { TextField } from "../ui/TextField";
import { useWorkspace } from "../workspace/SignedIn";
import { type Message as EnrolMessage, type Model as Enrolment, enrolment } from "./enrol";
import { describeMachine, formatInstant } from "./format";
import type { Connection } from "./live";

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
    <Card title={machine.name} status={pill} className={machine.revoked ? "opacity-70" : ""}>
      <div className="flex items-center gap-3">
        <span className="flex size-9 shrink-0 items-center justify-center rounded-nav border border-rule bg-sunken text-ink-muted">
          {machine.os === "macos" ? <LaptopIcon /> : <ServerIcon />}
        </span>
        <div className="min-w-0">
          <p className="text-ink">{describeMachine(machine)}</p>
          <p className="truncate font-mono text-ink-subtle text-sm">{machine.hostname}</p>
        </div>
      </div>
      <dl className="mt-4 grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 border-rule border-t pt-3 text-sm">
        <dt className="font-mono text-ink-subtle text-label uppercase tracking-label">Last seen</dt>
        <dd className="text-ink-muted">
          {machine.online
            ? "Now"
            : machine.last_seen_at === null
              ? "Never connected"
              : formatInstant(machine.last_seen_at)}
        </dd>
        <dt className="font-mono text-ink-subtle text-label uppercase tracking-label">Enrolled</dt>
        <dd className="text-ink-muted">{formatInstant(machine.enrolled_at)}</dd>
        <dt className="font-mono text-ink-subtle text-label uppercase tracking-label">Release</dt>
        <dd className="font-mono text-ink-muted">{machine.build}</dd>
      </dl>
    </Card>
  );
}

/** A command to run, set in mono in a well. */
function Command({ children }: { readonly children: string }) {
  return (
    <pre className="mt-1.5 overflow-x-auto rounded-control border border-rule bg-sunken px-3 py-2 font-mono text-ink text-sm">
      <code>{children}</code>
    </pre>
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
      <Card title={title}>
        <ol className="flex list-decimal flex-col gap-4 pl-5 text-ink">
          <li>
            On the machine, with abc installed, run:
            <Command>{`abc worker enrol ${origin}`}</Command>
          </li>
          <li>
            When it asks, paste this token. It works once, until{" "}
            {formatInstant(step.token.expires_at)}:<Command>{step.token.token}</Command>
          </li>
          <li>
            Then start the worker, which keeps the machine online:
            <Command>abc worker</Command>
          </li>
        </ol>
        <p className="mt-4 text-ink-muted text-sm">
          abc shows the token only now and does not keep it. The machine appears here once it
          enrols.
        </p>
        <Button className="mt-4" onPress={() => dispatch({ type: "closed" })}>
          Done
        </Button>
      </Card>
    );
  }
  const error = step.state === "asking" ? step.error : null;
  return (
    <Card title={title}>
      <div className="flex flex-col gap-4">
        <p className="text-ink-muted">
          A machine runs abc's agents for you. Create a one-time token, then run the commands it
          comes with on the machine.
        </p>
        <TextField
          label="Machine name"
          description="Shown on its card. Leave it blank to use the machine's hostname."
          value={model.label}
          isDisabled={step.state === "issuing"}
          onChange={(label) => dispatch({ type: "labelled", label })}
          error={error ?? undefined}
        />
        <div className="flex gap-2">
          <Button
            variant="primary"
            isPending={step.state === "issuing"}
            onPress={() => dispatch({ type: "requested" })}
          >
            Create enrolment token
          </Button>
          {first ? null : <Button onPress={() => dispatch({ type: "closed" })}>Cancel</Button>}
        </div>
      </div>
    </Card>
  );
}

export function MachinesScreen() {
  const { api, workspace, origin, live, dispatch } = useWorkspace();
  const [enrol, enrolDispatch] = useProgram(enrolment(api, workspace.id));
  const { connection, machines, head } = live;
  const pill = connectionPill(connection);
  const retry = (
    <Button variant="primary" onPress={() => dispatch({ type: "retry" })}>
      Try again
    </Button>
  );
  let body: ReactNode;
  if (head === null && connection.state === "failed") {
    body = (
      <Card title="The machines could not be loaded">
        <div className="flex flex-col items-start gap-4">
          <p className="text-ink-muted">{connection.problem.message}</p>
          {retry}
        </div>
      </Card>
    );
  } else if (head === null) {
    body = <output className="block text-ink-muted">Loading machines…</output>;
  } else {
    const first = machines.length === 0;
    body = (
      <div className="flex flex-col gap-4">
        {connection.state === "reconnecting" ? (
          <output className="block rounded-control border border-warning bg-warning-tint px-3 py-2 text-warning-ink">
            Offline: the connection to the server dropped ({connection.problem.message}). Resuming
            where it stopped; the cards may be out of date.
          </output>
        ) : null}
        {connection.state === "failed" ? (
          <div
            role="alert"
            className="flex flex-wrap items-center justify-between gap-3 rounded-control bg-danger-tint px-3 py-2 text-danger"
          >
            <p>{connection.problem.message} The cards may be out of date.</p>
            {retry}
          </div>
        ) : null}
        {first || enrol.open ? (
          <EnrolPanel first={first} origin={origin} model={enrol} dispatch={enrolDispatch} />
        ) : null}
        {first ? null : (
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
        <Heading level={1} className="font-semibold text-xl">
          Machines
        </Heading>
        <StatusPill {...pill} />
        {head !== null && machines.length > 0 && !enrol.open ? (
          <Button className="ml-auto" onPress={() => enrolDispatch({ type: "opened" })}>
            Enrol a machine
          </Button>
        ) : null}
      </div>
      {body}
    </div>
  );
}
