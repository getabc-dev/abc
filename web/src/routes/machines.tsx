import { createFileRoute } from "@tanstack/react-router";

import { MachinesScreen } from "../machines/MachinesScreen";

export const Route = createFileRoute("/machines")({
  component: MachinesScreen,
});
