// The app for a member: the shell around the routes, and the workspace's live machines, which the
// shell's pills and the Machines screen both show. The live program runs here, for as long as the
// person is signed in, so moving between pages keeps one stream.

import { useRouterState } from "@tanstack/react-router";
import { MonitorSmartphoneIcon } from "lucide-react";
import { createContext, type ReactNode, use, useMemo } from "react";
import { AppShell, type NavItem } from "@/components/AppShell";
import { TitledCard } from "@/components/TitledCard";
import type { ApiClient } from "../api/client";
import { type Dispatch, useProgram } from "../elm";
import type { SessionView } from "../generated/SessionView";
import type { WorkspaceSummary } from "../generated/WorkspaceSummary";

import { liveMachines, type Message, type Model } from "../machines/live";
import { Centered } from "../session/Centered";

/** What the pages of a signed-in person reach. */
export type Workspace = {
  readonly api: ApiClient;
  readonly workspace: WorkspaceSummary;
  /** The server's public URL, which workers enrol with: the origin the app was loaded from. */
  readonly origin: string;
  readonly live: Model;
  readonly dispatch: Dispatch<Message>;
};

const WorkspaceContext = createContext<Workspace | null>(null);

/** The signed-in person's workspace, for a page inside {@link SignedIn}. */
export function useWorkspace(): Workspace {
  const workspace = use(WorkspaceContext);
  if (workspace === null) {
    throw new Error("useWorkspace is for pages inside SignedIn");
  }
  return workspace;
}

/** The navigation: only pages that exist. Later stages add theirs (decisions.md). */
function navigation(pathname: string): NavItem[] {
  return [
    {
      to: "/machines",
      label: "Machines",
      icon: <MonitorSmartphoneIcon aria-hidden="true" />,
      current: pathname === "/machines" || pathname.startsWith("/machines/"),
    },
  ];
}

function Shell({
  api,
  origin,
  person,
  workspace,
  children,
}: {
  readonly api: ApiClient;
  readonly origin: string;
  readonly person: string;
  readonly workspace: WorkspaceSummary;
  readonly children: ReactNode;
}) {
  const [live, dispatch] = useProgram(liveMachines(api, workspace.id));
  const pathname = useRouterState({ select: (state) => state.location.pathname });
  const machines = useMemo(
    () =>
      live.head === null
        ? null
        : live.machines
            .filter((machine) => !machine.revoked)
            .map((machine) => ({ id: machine.worker, name: machine.name, online: machine.online })),
    [live.head, live.machines],
  );
  const pages = useMemo(() => navigation(pathname), [pathname]);
  const value = useMemo(
    () => ({ api, workspace, origin, live, dispatch }),
    [api, workspace, origin, live, dispatch],
  );
  return (
    <WorkspaceContext value={value}>
      <AppShell workspace={workspace.name} person={person} machines={machines} navigation={pages}>
        {children}
      </AppShell>
    </WorkspaceContext>
  );
}

export function SignedIn({
  api,
  origin,
  view,
  children,
}: {
  readonly api: ApiClient;
  readonly origin: string;
  readonly view: Extract<SessionView, { state: "member" }>;
  readonly children: ReactNode;
}) {
  // The claim makes one workspace; choosing among several comes with a workspace switcher.
  const [workspace] = view.workspaces;
  if (workspace === undefined) {
    return (
      <Centered>
        <TitledCard title="No workspace" level={1}>
          <p className="text-muted-foreground">
            {view.display_name} belongs to no workspace on this server any more.
          </p>
        </TitledCard>
      </Centered>
    );
  }
  return (
    <Shell
      key={workspace.id}
      api={api}
      origin={origin}
      person={view.display_name}
      workspace={workspace}
    >
      {children}
    </Shell>
  );
}
