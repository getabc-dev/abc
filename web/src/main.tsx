import { QueryClient } from "@tanstack/react-query";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { apiClient } from "./api/client";
import { App, createAppRouter } from "./app";
import "./styles.css";

const queryClient = new QueryClient();
const router = createAppRouter({
  api: apiClient((input, init) => fetch(input, init)),
  queryClient,
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
