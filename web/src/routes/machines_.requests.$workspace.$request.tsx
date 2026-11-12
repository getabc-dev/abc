import { createFileRoute } from "@tanstack/react-router";

import { ApproveScreen } from "../machines/ApproveScreen";

function Approve() {
  const { workspace, request } = Route.useParams();
  return <ApproveScreen workspace={workspace} request={request} />;
}

export const Route = createFileRoute("/machines_/requests/$workspace/$request")({
  component: Approve,
});
