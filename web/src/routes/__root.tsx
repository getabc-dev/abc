import type { QueryClient } from "@tanstack/react-query";
import { createRootRouteWithContext, Link, Outlet } from "@tanstack/react-router";
import { Heading } from "react-aria-components";

import type { ApiClient } from "../api/client";

/** What every route can reach: the API client and the query cache. */
type RouterContext = {
  readonly api: ApiClient;
  readonly queryClient: QueryClient;
};

function RootLayout() {
  return (
    <main className="mx-auto max-w-2xl p-6 font-sans text-ink">
      <Outlet />
    </main>
  );
}

function NotFound() {
  return (
    <section aria-labelledby="not-found-title" className="space-y-2">
      <Heading id="not-found-title" level={1} className="text-xl font-semibold">
        No page here
      </Heading>
      <p className="text-muted">
        abc has no page at this address.{" "}
        <Link to="/" className="text-accent underline">
          Go home
        </Link>
      </p>
    </section>
  );
}

export const Route = createRootRouteWithContext<RouterContext>()({
  component: RootLayout,
  notFoundComponent: NotFound,
});
