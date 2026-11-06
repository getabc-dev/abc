import type { QueryClient } from "@tanstack/react-query";
import { createRootRouteWithContext, Link, Outlet } from "@tanstack/react-router";

import { TitledCard } from "@/components/TitledCard";

import type { ApiClient } from "../api/client";
import { SessionGate } from "../session/SessionGate";

/** What every route can reach. */
type RouterContext = {
  readonly api: ApiClient;
  readonly queryClient: QueryClient;
  /** The origin the app was loaded from: the server's public URL. */
  readonly origin: string;
};

function Root() {
  const { api, origin } = Route.useRouteContext();
  return (
    <SessionGate api={api} origin={origin}>
      <Outlet />
    </SessionGate>
  );
}

function NotFound() {
  return (
    <TitledCard title="No page here" level={1}>
      <p className="text-muted-foreground">
        abc has no page at this address.{" "}
        <Link to="/" className="text-primary underline underline-offset-4">
          Go home
        </Link>
      </p>
    </TitledCard>
  );
}

export const Route = createRootRouteWithContext<RouterContext>()({
  component: Root,
  notFoundComponent: NotFound,
});
