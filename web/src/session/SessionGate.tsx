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

/** Says that asking for the session again failed, under the shell's header, with a retry. */
function SessionNotice({
  message,
  onRetry,
}: {
  readonly message: string;
  readonly onRetry: () => void;
}) {
  return (
    <output className="flex flex-wrap items-center gap-x-3 gap-y-1 border-b bg-warning-tint px-4 py-1.5 text-warning-foreground text-xs md:px-5">
      <p className="min-w-0 flex-1">
        abc could not check your session ({message}). The page shows what it last knew.
      </p>
      <Button variant="ghost" size="sm" onClick={onRetry}>
        Check again
      </Button>
    </output>
  );
}

export function SessionGate({
  api,
  origin,
  setupCode,
  children,
}: {
  readonly api: ApiClient;
  /** The server's public URL. */
  readonly origin: string;
  /** The setup code the claim link carried, or "". */
  readonly setupCode: string;
  /** The routes, which a member sees inside the shell. */
  readonly children: ReactNode;
}) {
  const session = useQuery({
    queryKey: SESSION,
    queryFn: ({ signal }) => api.session(signal),
    // Coming back to the window asks again, so a claim, a new membership or ended access shows
    // without a reload.
    refetchOnWindowFocus: true,
    // A failure is shown at once: before any answer, as the error screen with its retry; after
    // one, as a notice in the shell, which the next focus or the person's retry asks again.
    retry: false,
  });
  if (session.isPending) {
    return (
      <Centered>
        <output className="block text-center text-muted-foreground">
          Asking the server who you are…
        </output>
      </Centered>
    );
  }
  // Only a session with no answer yet is an error screen. A failed refetch keeps what the server
  // last said, and the signed-in shell with its live stream stays up under a notice.
  if (session.isLoadingError) {
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
      return <ClaimScreen api={api} login={view.login} setupCode={setupCode} />;
    case "member":
      return (
        <SignedIn
          api={api}
          origin={origin}
          view={view}
          notice={
            session.isRefetchError ? (
              <SessionNotice
                message={session.error.message}
                onRetry={() => {
                  void session.refetch();
                }}
              />
            ) : null
          }
        >
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
