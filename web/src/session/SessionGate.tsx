// What the visitor sees, by who the server says they are (`GET /api/session`): an unclaimed
// server sends every route to the claim; a member gets the app; anyone else is told why not.

import { useQuery } from "@tanstack/react-query";
import type { ReactNode } from "react";

import { TitledCard } from "@/components/TitledCard";
import { Button } from "@/components/ui/button";

import type { ApiClient } from "../api/client";
import { ClaimScreen } from "../claim/ClaimScreen";
import { SignedIn } from "../workspace/SignedIn";
import { Centered } from "./Centered";

/** The session query's key. */
const SESSION = ["session"] as const;

export function SessionGate({
  api,
  origin,
  children,
}: {
  readonly api: ApiClient;
  /** The server's public URL. */
  readonly origin: string;
  /** The routes, which a member sees inside the shell. */
  readonly children: ReactNode;
}) {
  const session = useQuery({ queryKey: SESSION, queryFn: ({ signal }) => api.session(signal) });
  if (session.isPending) {
    return (
      <Centered>
        <output className="block text-center text-muted-foreground">
          Asking the server who you are…
        </output>
      </Centered>
    );
  }
  if (session.isError) {
    return (
      <Centered>
        <TitledCard title="The server did not answer" level={1}>
          <div className="flex flex-col items-start gap-4">
            <p className="text-muted-foreground">{session.error.message}</p>
            <Button
              onClick={() => {
                void session.refetch();
              }}
            >
              Try again
            </Button>
          </div>
        </TitledCard>
      </Centered>
    );
  }
  const view = session.data;
  switch (view.state) {
    case "unclaimed":
      return <ClaimScreen api={api} login={view.login} />;
    case "member":
      return (
        <SignedIn api={api} origin={origin} view={view}>
          {children}
        </SignedIn>
      );
    case "no_identity":
      return (
        <Centered>
          <TitledCard title="No tailnet identity" level={1}>
            <p className="text-muted-foreground">
              This device reaches abc without a signed-in Tailscale user, as tagged devices do. Open
              abc from a device signed in as a person.
            </p>
          </TitledCard>
        </Centered>
      );
    case "not_a_member":
      return (
        <Centered>
          <TitledCard title="Not a member" level={1}>
            <p className="text-muted-foreground">
              {view.login} belongs to no workspace on this server. Ask its owner to add you.
            </p>
          </TitledCard>
        </Centered>
      );
  }
}
