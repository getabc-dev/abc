// The app: the router over the generated route tree, with the API client and the query cache in
// every route's context. main.tsx mounts it; tests mount it on a memory history.

import { type QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createRouter, type RouterHistory, RouterProvider } from "@tanstack/react-router";

import type { ApiClient } from "./api/client";
import { routeTree } from "./routeTree.gen";

/** The router, on the browser's history unless `history` is given. */
export function createAppRouter(
  context: {
    readonly api: ApiClient;
    readonly queryClient: QueryClient;
    readonly origin: string;
    readonly setupCode: string;
  },
  history?: RouterHistory,
) {
  return createRouter({ routeTree, context, ...(history === undefined ? {} : { history }) });
}

type AppRouter = ReturnType<typeof createAppRouter>;

declare module "@tanstack/react-router" {
  interface Register {
    router: AppRouter;
  }
}

export function App({
  router,
  queryClient,
}: {
  readonly router: AppRouter;
  readonly queryClient: QueryClient;
}) {
  return (
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>
  );
}
