import type { QueryClient } from "@tanstack/react-query";
import { createRootRouteWithContext, Outlet, useRouter } from "@tanstack/react-router";
import { RouterProvider } from "react-aria-components";

import type { ApiClient } from "../api/client";
import { SessionGate } from "../session/SessionGate";
import { Card } from "../ui/Card";
import { Link } from "../ui/Link";

/** What every route can reach. */
type RouterContext = {
  readonly api: ApiClient;
  readonly queryClient: QueryClient;
  /** The origin the app was loaded from: the server's public URL. */
  readonly origin: string;
};

function Root() {
  const router = useRouter();
  const { api, origin } = Route.useRouteContext();
  return (
    // React Aria's links navigate through the router, without a page load.
    <RouterProvider
      navigate={(href) => {
        router.history.push(href);
      }}
    >
      <SessionGate api={api} origin={origin}>
        <Outlet />
      </SessionGate>
    </RouterProvider>
  );
}

function NotFound() {
  return (
    <Card title="No page here" level={1}>
      <p className="text-ink-muted">
        abc has no page at this address. <Link href="/">Go home</Link>
      </p>
    </Card>
  );
}

export const Route = createRootRouteWithContext<RouterContext>()({
  component: Root,
  notFoundComponent: NotFound,
});
