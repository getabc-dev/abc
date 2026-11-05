import { useQuery } from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import { Button, Heading } from "react-aria-components";

import { describeSession } from "../session/describe";

function Home() {
  const { api } = Route.useRouteContext();
  const session = useQuery({
    queryKey: ["session"],
    queryFn: ({ signal }) => api.session(signal),
  });

  if (session.isPending) {
    return <p className="text-muted">Asking the server who you are…</p>;
  }
  if (session.isError) {
    return (
      <section aria-labelledby="session-error" className="space-y-3">
        <Heading id="session-error" level={1} className="text-xl font-semibold text-danger">
          The server did not answer
        </Heading>
        <p>{session.error.message}</p>
        <Button
          className="rounded bg-accent px-3 py-1.5 text-surface data-[focus-visible]:outline-2 data-[pressed]:opacity-80"
          onPress={() => {
            void session.refetch();
          }}
        >
          Try again
        </Button>
      </section>
    );
  }
  const summary = describeSession(session.data);
  return (
    <section aria-labelledby="session-title" className="space-y-2">
      <Heading id="session-title" level={1} className="text-xl font-semibold">
        {summary.title}
      </Heading>
      <p className="text-muted">{summary.detail}</p>
    </section>
  );
}

export const Route = createFileRoute("/")({
  component: Home,
});
