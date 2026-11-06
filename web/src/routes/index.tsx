import { createFileRoute, redirect } from "@tanstack/react-router";

// Home. Until the pipeline board of a later stage arrives, Machines is all a workspace has, so
// home is Machines (decisions.md).
export const Route = createFileRoute("/")({
  beforeLoad: () => {
    // TanStack Router redirects by throwing.
    throw redirect({ to: "/machines", replace: true });
  },
});
