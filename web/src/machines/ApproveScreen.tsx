// The Approve page: a machine's request to join the workspace, for its owner to approve or deny.
// `abc worker setup` prints this page's address and opens it. Its states: loading, error with
// retry (or "no such request" for a request that is not this person's), waiting for the owner, and
// closed with how it ended.

import { Link } from "@tanstack/react-router";

import { PermissionRequest } from "@/components/PermissionRequest";
import type { Tone } from "@/components/Status";
import { TitledCard } from "@/components/TitledCard";
import { Button, buttonVariants } from "@/components/ui/button";

import { useProgram } from "../elm";
import type { EnrolmentRequestId } from "../generated/EnrolmentRequestId";
import type { RequestCard } from "../generated/RequestCard";
import type { WorkspaceId } from "../generated/WorkspaceId";
import { useWorkspace } from "../workspace/SignedIn";
import { approval, type Outcome } from "./approval";
import { describeMachine, formatInstant } from "./format";

const ENDED: Record<
  Outcome,
  { readonly status: { readonly tone: Tone; readonly label: string }; readonly text: string }
> = {
  approved: {
    status: { tone: "success", label: "Approved" },
    text: "The machine is enrolled. Its terminal finishes setup and starts its worker.",
  },
  denied: {
    status: { tone: "danger", label: "Denied" },
    text: "The machine was not enrolled, and its terminal says so.",
  },
  expired: {
    status: { tone: "warning", label: "Expired" },
    text: "Nobody approved this request in time. Run abc worker setup on the machine again.",
  },
};

function details(card: RequestCard) {
  return [
    { label: "Machine", value: card.hostname },
    { label: "System", value: describeMachine(card) },
    { label: "Requested by", value: card.login },
    { label: "From node", value: <span className="font-mono">{card.node}</span> },
    { label: "Release", value: <span className="font-mono">{card.build}</span> },
    { label: "Asked", value: formatInstant(card.requested_at) },
  ];
}

/** The request page of the signed-in workspace; a link to another workspace's request is none. */
export function ApproveScreen({
  workspace,
  request,
}: {
  readonly workspace: WorkspaceId;
  readonly request: EnrolmentRequestId;
}) {
  const signedIn = useWorkspace().workspace.id;
  if (workspace !== signedIn) {
    return (
      <TitledCard title="No such request" level={1} className="w-full max-w-xl">
        <div className="flex flex-col items-start gap-4">
          <p className="text-muted-foreground">
            This link is for another workspace than the one you are signed in to. Open it from the
            account that owns that workspace.
          </p>
          <Link to="/machines" className={buttonVariants({ variant: "outline" })}>
            Back to Machines
          </Link>
        </div>
      </TitledCard>
    );
  }
  return <Request key={request} workspace={workspace} request={request} />;
}

function Request({
  workspace,
  request,
}: {
  readonly workspace: WorkspaceId;
  readonly request: EnrolmentRequestId;
}) {
  const { api } = useWorkspace();
  const [model, dispatch] = useProgram(approval(api, workspace, request));
  const { stage } = model;
  const back = (
    <Link to="/machines" className={buttonVariants({ variant: "outline" })}>
      Back to Machines
    </Link>
  );
  switch (stage.state) {
    case "loading":
      return <output className="block text-muted-foreground">Loading the request…</output>;
    case "failed":
      return (
        <TitledCard
          title={
            stage.error.code === "enrolment_request_unknown"
              ? "No such request"
              : "The request could not be loaded"
          }
          level={1}
          className="w-full max-w-xl"
        >
          <div className="flex flex-col items-start gap-4">
            <p className="text-muted-foreground">
              {stage.error.code === "enrolment_request_unknown"
                ? "This workspace has no such request. Run abc worker setup on the machine again for a new one."
                : stage.error.message}
            </p>
            <div className="flex gap-2">
              {stage.error.code === "enrolment_request_unknown" ? null : (
                <Button onClick={() => dispatch({ type: "retry" })}>Try again</Button>
              )}
              {back}
            </div>
          </div>
        </TitledCard>
      );
    case "closed": {
      const ended = ENDED[stage.outcome];
      return (
        <TitledCard
          title={stage.card.hostname}
          level={1}
          status={ended.status}
          className="w-full max-w-xl"
        >
          <div className="flex flex-col items-start gap-4">
            <p className="text-muted-foreground">{ended.text}</p>
            {back}
          </div>
        </TitledCard>
      );
    }
    case "open":
      return (
        <PermissionRequest
          asker={stage.card.hostname}
          os={stage.card.os}
          asks="It asks to be enrolled as a worker: a machine that runs abc's agents for you."
          details={details(stage.card)}
          code={stage.card.code}
          codeHelp="Approve only if it matches the code your terminal shows."
          expires={`Expires ${formatInstant(stage.card.expires_at)}`}
          error={stage.error}
          deciding={stage.deciding}
          onApprove={() => dispatch({ type: "approve" })}
          onDeny={() => dispatch({ type: "deny" })}
        />
      );
  }
}
