import { QueryClient } from "@tanstack/react-query";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { apiClient } from "./api/client";
import { App, createAppRouter } from "./app";
import { takeSetupCode } from "./claim/link";
import "./styles.css";

const queryClient = new QueryClient();
const router = createAppRouter({
  api: apiClient((input, init) => fetch(input, init)),
  queryClient,
  origin: window.location.origin,
  // Before the router reads the location, so the code never reaches a route or the history.
  setupCode: takeSetupCode(window.location, window.history),
});

const root = document.getElementById("root");
if (root === null) {
  throw new Error("index.html has no #root element");
}
createRoot(root).render(
  <StrictMode>
    <App router={router} queryClient={queryClient} />
  </StrictMode>,
);
