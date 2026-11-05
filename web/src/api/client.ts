// The typed client for abc's API. Every request and response type comes from Rust
// (src/generated/, written by ts-rs); nothing here declares an API type of its own.

import type { ApiCode } from "../generated/ApiCode";
import type { ApiErrorBody } from "../generated/ApiErrorBody";
import type { SessionView } from "../generated/SessionView";

/** A failed request: the API's error code and message, or `network` when no answer came. */
export class ApiError extends Error {
  readonly code: ApiCode | "network";
  readonly status: number | null;

  constructor(code: ApiCode | "network", message: string, status: number | null) {
    super(message);
    this.name = "ApiError";
    this.code = code;
    this.status = status;
  }
}

/** How the client reaches the server: `fetch`, or a stand-in in tests. */
export type Fetch = (input: string, init: RequestInit) => Promise<Response>;

function isErrorBody(value: unknown): value is ApiErrorBody {
  if (typeof value !== "object" || value === null || !("error" in value)) {
    return false;
  }
  const { error } = value;
  return (
    typeof error === "object" &&
    error !== null &&
    "code" in error &&
    typeof error.code === "string" &&
    "message" in error &&
    typeof error.message === "string"
  );
}

/** GETs `path` and returns its JSON body as `T`, or throws an {@link ApiError}. */
async function get<T>(fetcher: Fetch, path: string, signal?: AbortSignal): Promise<T> {
  let response: Response;
  try {
    response = await fetcher(path, {
      method: "GET",
      headers: { accept: "application/json" },
      ...(signal === undefined ? {} : { signal }),
    });
  } catch (error: unknown) {
    const reason = error instanceof Error ? error.message : String(error);
    throw new ApiError("network", `the server could not be reached: ${reason}`, null);
  }
  const body: unknown = await response.json().catch(() => null);
  if (!response.ok) {
    if (isErrorBody(body)) {
      throw new ApiError(body.error.code, body.error.message, response.status);
    }
    throw new ApiError("internal", `the server answered ${response.status}`, response.status);
  }
  if (body === null) {
    throw new ApiError("internal", "the server's answer was not JSON", response.status);
  }
  // The server is the same build as this app, and its Rust types are the source of `T`.
  // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- the server's answer is a T
  return body as T;
}

/** The API, over `fetcher`. */
export function apiClient(fetcher: Fetch) {
  return {
    /** Who the visitor is to this server: `GET /api/session`. */
    session: (signal?: AbortSignal) => get<SessionView>(fetcher, "/api/session", signal),
  };
}

export type ApiClient = ReturnType<typeof apiClient>;
