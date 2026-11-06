// The typed client for abc's API. Every request and response type comes from Rust
// (src/generated/, written by ts-rs); nothing here declares an API type of its own.

import type { ApiCode } from "../generated/ApiCode";
import type { ApiErrorBody } from "../generated/ApiErrorBody";
import type { ClaimRequest } from "../generated/ClaimRequest";
import type { ClaimResponse } from "../generated/ClaimResponse";
import type { IssueTokenRequest } from "../generated/IssueTokenRequest";
import type { LiveUpdate } from "../generated/LiveUpdate";
import type { Position } from "../generated/Position";
import type { SessionView } from "../generated/SessionView";
import type { TokenIssuedResponse } from "../generated/TokenIssuedResponse";
import type { WorkspaceId } from "../generated/WorkspaceId";
import { readEvents } from "./sse";

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

/** Any failure as an {@link ApiError}: one already, or a network error naming its cause. */
export function asApiError(error: unknown): ApiError {
  if (error instanceof ApiError) {
    return error;
  }
  const reason = error instanceof Error ? error.message : String(error);
  return new ApiError("network", `the server could not be reached: ${reason}`, null);
}

/** Sends a request, turning a failure to send into a network {@link ApiError}. */
async function send(fetcher: Fetch, path: string, init: RequestInit): Promise<Response> {
  try {
    return await fetcher(path, init);
  } catch (error: unknown) {
    throw asApiError(error);
  }
}

/** The {@link ApiError} a non-2xx `response` stands for. */
async function refusal(response: Response): Promise<ApiError> {
  const body: unknown = await response.json().catch(() => null);
  if (isErrorBody(body)) {
    return new ApiError(body.error.code, body.error.message, response.status);
  }
  return new ApiError("internal", `the server answered ${response.status}`, response.status);
}

/** Sends `body`, if any, as JSON to `path` and returns the JSON answer as `T`. */
async function exchange<T>(
  fetcher: Fetch,
  method: "GET" | "POST",
  path: string,
  body: unknown,
  signal: AbortSignal | undefined,
): Promise<T> {
  const response = await send(fetcher, path, {
    method,
    headers:
      body === undefined
        ? { accept: "application/json" }
        : { accept: "application/json", "content-type": "application/json" },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    ...(signal === undefined ? {} : { signal }),
  });
  if (!response.ok) {
    throw await refusal(response);
  }
  const answer: unknown = await response.json().catch(() => null);
  if (answer === null) {
    throw new ApiError("internal", "the server's answer was not JSON", response.status);
  }
  // The server is the same build as this app, and its Rust types are the source of `T`.
  // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- the server's answer is a T
  return answer as T;
}

/**
 * The workspace's live updates, from `GET /api/workspaces/{ws}/live`: a snapshot first, or, when
 * `after` is a position, what changed after it (sent as `Last-Event-ID`). Throws an
 * {@link ApiError} when the server refuses; iterating throws one when the connection fails, and
 * ends when the server ends the stream.
 */
async function live(
  fetcher: Fetch,
  workspace: WorkspaceId,
  after: Position | null,
  signal: AbortSignal,
): Promise<AsyncIterable<LiveUpdate>> {
  const response = await send(fetcher, `/api/workspaces/${encodeURIComponent(workspace)}/live`, {
    method: "GET",
    headers:
      after === null
        ? { accept: "text/event-stream" }
        : { accept: "text/event-stream", "last-event-id": String(after) },
    cache: "no-store",
    signal,
  });
  if (!response.ok) {
    throw await refusal(response);
  }
  if (response.body === null) {
    throw new ApiError("internal", "the server's stream had no body", response.status);
  }
  const body = response.body;
  return {
    async *[Symbol.asyncIterator]() {
      try {
        for await (const event of readEvents(body)) {
          // The server's `LiveUpdate`, serialised from the same Rust type.
          // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- the server sends LiveUpdates
          yield JSON.parse(event.data) as LiveUpdate;
        }
      } catch (error: unknown) {
        throw asApiError(error);
      }
    },
  };
}

/** The API, over `fetcher`. */
export function apiClient(fetcher: Fetch) {
  return {
    /** Who the visitor is to this server: `GET /api/session`. */
    session: (signal?: AbortSignal) =>
      exchange<SessionView>(fetcher, "GET", "/api/session", undefined, signal),
    /** Claims the unclaimed server with its setup code: `POST /api/owner/claim`. */
    claim: (request: ClaimRequest, signal?: AbortSignal) =>
      exchange<ClaimResponse>(fetcher, "POST", "/api/owner/claim", request, signal),
    /** A one-time token that enrols a machine: `POST /api/workspaces/{ws}/enrolment-tokens`. */
    issueEnrolmentToken: (
      workspace: WorkspaceId,
      request: IssueTokenRequest,
      signal?: AbortSignal,
    ) =>
      exchange<TokenIssuedResponse>(
        fetcher,
        "POST",
        `/api/workspaces/${encodeURIComponent(workspace)}/enrolment-tokens`,
        request,
        signal,
      ),
    /** The workspace's live updates; see {@link live}. */
    live: (workspace: WorkspaceId, after: Position | null, signal: AbortSignal) =>
      live(fetcher, workspace, after, signal),
  };
}

export type ApiClient = ReturnType<typeof apiClient>;
