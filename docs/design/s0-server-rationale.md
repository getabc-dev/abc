# S0 PR 3 "Server": design rationale

Status: agreed on 1 Nov 2026 and implemented in PR 3. The sections below are the design as agreed;
"Implementation notes" at the end records where the code differs and why.

## Problem

PR 3 builds the server's spine: Postgres 18 migrations on start, an append-only event log with
views, one permission check, the owner claim, enrolment tokens, the worker WebSocket, SSE for the
web app, and health and readiness. Four constraints interlock, and together they make the shape
non-obvious.

- **D10 + I4 + "every write goes through the event path".** Storage must demand an `Authorized`
  that only abc-core can mint. Every request, reads included, must pass the check. Events and views
  must be written in one transaction. If the check and the write run in different transactions, a
  revoked worker or a spent token slips through the gap between them.
- **D4.** Identity arrives as plain HTTP headers. Tailscale Serve sets `Tailscale-User-Login`,
  strips copies a client sent, and omits it for tagged devices and Funnel. But anyone who reaches
  the backend by another path can set the header.
- **Enrolment over a link that drops.** A laptop can lose the enrolment response. The worker must
  not end up with a spent token and no credential, and the server must never hold a credential it
  could leak (I2).
- **Decision 30.** The same log must carry S1 graph runs (per-stream order, causation, versioned
  payloads, cheap tailing) without being reworked.

Phase A grounding: the only existing code is `abc-protocol`. It defines the `Hello` to
`Welcome`/`Reject` handshake as JSON text frames, ignores unknown fields, and treats unknown
variants as errors. The workspace lints rule out panics, `as` casts, wildcard enum arms, errors
named `Error`, `#[allow]` and `pub` items that are unreachable. Postgres tests use
`#[sqlx::test]` (D7). This PR does not edit `xtask/`, CI or `AGENTS.md`, because s0-gates owns
them right now.

## Usage (caller's view)

### Operator quickstart

```text
# compose: abc + postgres:18 + tailscale/tailscale. abc and the sidecar share ONE volume,
# mounted nowhere else, holding the socket. TS_SERVE_CONFIG proxies / to unix:/run/abc-sidecar/abc.sock
docker compose up -d                                   # abc migrates Postgres, then serves
docker compose exec abc abc server setup-code          # prints a code to THIS terminal only
  Setup code (valid until 14:05 UTC):  7QK2M-9XPD4-WH3TR-8NVCB
# open https://abc.tail1234.ts.net, enter the code: your tailnet login becomes the owner
# Machines -> Add machine -> shown once:  abc worker enrol https://abc.tail1234.ts.net --token abc_enr_…
```

### abc-store, as the server uses it

```rust
let db = abc_store::connect(&database_url).await?;     // opaque Database: no sqlx type escapes
abc_store::migrate(&db).await?;
let store = Store::open(db, StoreOptions::production()).await?;

let alice = Caller::Tailnet(login);                     // built only by the sidecar extractor
let issued = store.execute(&alice, &origin, IssueEnrolmentToken { workspace, label, ttl }).await?;
// issued.token: Secret<EnrolmentToken>, shown once. The log holds its digest.

let enrolled = store.execute(&Caller::Anonymous, &origin, EnrolWorker { token, key, machine, build }).await?;
// The same token with the same key digest again (lost response) -> the same Enrolled, zero events.

let machines = store.query(&alice, ListMachines { workspace }).await?;   // reads pass the same check
let mut feed = store.feed();                                             // wakes when the log head moves
```

### Call site 1: an HTTP handler (abc-server)

```rust
async fn issue_token(
    State(app): State<SidecarState>,
    caller: TailnetCaller,          // compiles only on the sidecar router; Host + CSRF guarded
    origin: RequestOrigin,          // request id = the events' correlation id
    Path(workspace): Path<WorkspaceId>,
    ApiJson(body): ApiJson<IssueTokenRequest>,
) -> Result<Json<TokenIssuedResponse>, ApiError> {
    let command = IssueEnrolmentToken { workspace, label: body.label, ttl: body.ttl };
    Ok(Json(app.store.execute(&caller.into(), &origin.into(), command).await?.into()))
}
```

Every mutating handler has this shape: extract, build a command, `execute`, convert. Handlers hold
no rules.

### Call site 2: abc-worker (PR 4), which depends on abc-protocol and abc-core only

```rust
// `abc worker enrol <url> --token …`: generate the key and SAVE it before the request
let key = WorkerKey::generate(&OsMint);                  // never leaves this machine
credentials.save(&url, &key)?;                           // 0600, atomic rename
let req = EnrolRequest { token, key: key.expose_secret().digest(), machine, build };
let EnrolResponse { worker, workspace, name } = post_with_retry(ENROL_PATH, &req).await?; // same request on retry
// `abc worker`: on every connect, including after sleep
let ws = dial(CONNECT_PATH, Authorization::bearer(&key)).await?;  // 401 worker_revoked -> stop
ws.send(WorkerMessage::Hello(Hello::new(build))).await?;          // existing handshake
// close 4409 superseded -> stop this process; 4503 -> back off (full jitter, 0.5 s to 30 s)
```

### Call site 3: a store integration test

```rust
#[sqlx::test(migrator = "abc_store::MIGRATOR")]
async fn a_lost_enrolment_response_is_retried_safely(pool: PgPool) {
    let t = Fixture::claimed(pool).await;                       // ManualClock + SeededMint
    let (token, key) = (t.issue_token().await, t.worker_key());
    let first = t.enrol(&token, &key).await.unwrap();
    assert_eq!(t.enrol(&token, &key).await.unwrap(), first, "the same token and key name the same worker");
    assert!(matches!(t.enrol(&token, &t.worker_key()).await.unwrap_err(),
        ExecuteError::Denied(Denied { reason: DenialReason::EnrolmentTokenUsed, .. })), "a second key is a replay");
    t.clock.advance(time::Duration::hours(1));
    assert_eq!(t.enrol(&token, &key).await.unwrap(), first, "a retry still gets its answer after expiry");
    abc_store::testing::assert_views_replay(&t.pool).await;    // views == replay(log)
}
```

## Shape

### Module map

| Crate | Owns | Public surface |
| --- | --- | --- |
| `abc-core` (no IO) | ids, `Secret`, credential formats, `Actor` and `Principal`, events and streams, commands with `prove` and `decide`, queries and read models, **the policy matrix** | `authorize`, `Authorized<R>`, `Command`, `Query`, `Decision`, value types |
| `abc-store` (the only sqlx user) | migrations, the log, views, authentication, the write and read pipelines, `LogFeed` | `connect`, `migrate`, `Store::{open, execute, query, feed, ready, issue_setup_code, is_claimed}`, `Caller`, `Database`, errors |
| `abc-protocol` (grows) | wire contracts: handshake, `enrol`, `link` (path, timeouts, close codes), `api::ApiCode` | message types and constants |
| `abc-server` (axum, no sqlx) | listeners and the trust each carries, HTTP, WebSocket, SSE, in-process session supersession, telemetry | `run(ServerConfig, shutdown)`, `PublicUrl`, `telemetry` |
| `abc` (binary) | CLI and exit codes | `abc server`, `abc server setup-code`, `abc worker` (PR 4) |

A write takes three files to trace: the handler (`abc-server/src/api/mod.rs`), the pipeline
(`abc-store/src/execute.rs`), and the command's rules (`abc-core/src/command/*.rs`). Views are
updated in `abc-store/src/projection.rs`.

### Key types

| Type or signature | What it guarantees |
| --- | --- |
| `Store::execute<C: Executable>(&self, &Caller, &Origin, C) -> Result<C::Outcome, ExecuteError<C::Rejection>>` | The only write. One transaction: lock the head, authenticate, load facts, `authorize`, `decide`, append, project, notify, commit. |
| `abc_core::policy::authorize<R: Request>(&Principal, R, &R::Facts, Timestamp) -> Result<Authorized<R>, Denied>` | The one permission check, for reads and writes. It is the only constructor of `Authorized`. |
| `Authorized<R>`: private fields, not `Clone` or `Serialize` | Proof that this exact request passed. It carries the permit (actor and workspace) and the capability proof. |
| `DecisionBuilder::new(&Authorized<C>)` then `Decision<O>` | A decision cannot exist without an `Authorized`. Every event is stamped with the permit's actor and workspace (I4). `WriteTx::append` (crate-private) takes only a `Decision`. |
| `Caller::{Tailnet(TailnetLogin), Worker(Secret<WorkerKey>), Anonymous, Server}` | What transport proved, untrusted. It is resolved to a `Principal` inside the transaction, so revocation is seen by the very next request. |
| `Event` (one enum per stream) + `Stream` trait | A worker event cannot go onto a token's stream (type error). Payloads are internal; the web app gets read models. |
| `TailnetCaller: FromRequestParts<SidecarState>` (and nothing else) | A handler that reads identity compiles only on the sidecar router. `SidecarApp` is served only by `serve_sidecar(SidecarSocket, ..)`. |
| `Secret<T: Zeroize>` | No `Display`, `Serialize` or `Clone`; redacted `Debug`; zeroized on drop. Leaving it takes `expose_secret` or `serialize_exposed`, and both are greppable. |
| `ApiCode` (abc-protocol) | One list of stable error codes for the worker and the web. Mapped from every typed error by exhaustive matches. |

### Event log and views (DDL: `crates/abc-store/migrations/0001_event_log.sql`)

`event(position PK, id, recorded_at, workspace_id, stream_kind, stream_id, stream_seq, event_type
GENERATED FROM payload->>'type', schema_version, actor_kind, actor_id, correlation_id,
causation_id -> event(id), payload jsonb)`. It has `UNIQUE(stream_kind, stream_id, stream_seq)`,
indexes on `(workspace_id, position)` and `(correlation_id, position)`, and triggers that reject
UPDATE, DELETE and TRUNCATE.

- **Single writer, gapless positions.** Every write's first statement locks the one
  `event_log_head` row, so positions are global, gapless and handed out in commit order. A consumer
  that has read up to N has seen everything at or below N, forever. Tailing is just
  `position > cursor`: no `xid8` snapshot filtering, no reorder buffer. A trigger refuses to move
  the head backwards or past the last event. READ COMMITTED is deliberate: each statement after the
  lock sees every earlier commit.
- **Views in the same transaction.** `projection::apply(&Recorded)` is one exhaustive match, so a
  new event fails the build until it is projected. Projections take time and ids from the event,
  never from `now()` or random functions. Every view row carries `changed_at` (the position of the
  last event that touched it), so "what changed since N" is an indexed range scan.
  `assert_views_replay` truncates every view table (found from `information_schema`, not from a
  list kept by hand), replays the log, and compares. Any write that skips the event path fails it.
- **Decisions read views,** not folded streams. That allows lookups by digest or login, with no
  snapshots. The replay test keeps the views honest.
- **Tailing.** `pg_notify('abc_log', head)` is delivered at commit. One `PgListener` per process
  feeds a `watch<Position>` (`LogFeed`). Subscribers wake and read from their own cursor. Bursts
  coalesce, and a slow reader cannot overflow anything. After a listener reconnect it republishes
  the head from the database, so a lost notification delays a read and never loses one.
- **S1 without rework.** A graph run is a `stream_kind = 'run'` stream whose `stream_seq` orders
  its steps. `correlation_id` is the run id, and `causation_id` links checker to maker and step to
  trigger. Reactions go through `execute` with `Origin { causation }`. S1 adds a unique
  `(stream, causation_id, event_type)` index to make reactions idempotent, plus a consumer-cursor
  table. Agent transcripts stay out of the log, so the single-writer lock sees only coarse step
  transitions.

### How `Authorized` is constructed

```text
Caller --authenticate (in txn)--> Principal --C::load (in txn)--> Facts
   --authorize: R::prove(facts, now) -> Proof; PolicyAction::permit(standing) -> Permit--> Authorized<C>
   --C::decide (pure)--> Decision --WriteTx::append--> events + views, same txn
```

- `prove` verifies any bearer capability against facts loaded in the same transaction: the setup
  code digest and expiry, or the token's state, expiry and redeeming key. It returns a typed proof
  (`TokenProof { record, replay }`).
- `permit` is **the** matrix: one exhaustive match for writes and one for reads, in
  `abc-core/src/policy.rs`. That file is all of abc's access policy.
- Forging a `Principal` or `Facts` elsewhere buys nothing. abc-store's public API takes a
  `Caller`, never a `Principal` or an `Authorized`. abc-server and abc do not depend on sqlx, and
  the pool is opaque, so nothing outside abc-store can reach a view.
- **Time of check equals time of use by construction.** Principal, facts, decision and append all
  run under the head lock, so no other write can commit in between. There is no epoch to compare
  and no retry loop.
- `Caller::Server` (the installation itself) is permitted exactly three actions:
  `IssueSetupCode` (while unclaimed), `RecordAsServer` (session ends) and `ViewAsServer` (the boot
  sweep's input). A handler that misuses it gains nothing else.

| Action | Who | Actor recorded |
| --- | --- | --- |
| `IssueSetupCode` | `Server` (the host CLI), unclaimed only | `Server` |
| `ClaimServer` | `Visitor` (tailnet login, no person yet) + valid, unexpired setup code | the reserved `Person` |
| `ManageEnrolment`, `ManageWorkers` | `Person` with role `Owner` in the workspace | that person |
| `EnrolWorker` | anyone presenting an open token (or the same key again) | the token's reserved `Worker` |
| `ActAsWorker` (session start) | the `Worker` itself, in its own workspace | that worker |
| `RecordAsServer` (session end) | `Server` | `Server` |
| read `ViewSession` / `ViewWorkspace` / `ViewSelfAsWorker` / `ViewAsServer` | anyone / a member / the worker / `Server` | n/a |

Health and readiness are the only routes outside the check. They serve no data and live on the ops
listener.

### Sidecar trust (D4): a property of the listener, not of the request

1. **The only listener that carries identity is a Unix socket** in a directory shared only with
   the `tailscale/tailscale` container. `SidecarSocket::bind` refuses a directory with any "other"
   permission bit, removes a stale socket file, and sets the socket to 0660. No TCP listener ever
   carries identity, so a stray `ports:` line or a `0.0.0.0` bind exposes health checks at most.
   The server checks this boundary itself at start, with an error message telling a stranger what
   to fix (P4).
2. **Identity handlers cannot be mounted elsewhere.** `TailnetCaller` implements
   `FromRequestParts<SidecarState>` only, so it does not compile on the ops router. `SidecarApp` is
   a newtype that only `serve_sidecar` takes. (`Router::with_state` erases the state type, which is
   why the newtype exists.) `clippy.toml` gains `disallowed-methods = ["axum::serve"]` so nothing
   can serve a router directly.
3. **Ambient authority is fenced in.** Serve attaches identity to whatever the user's browser
   sends. The extractor refuses requests whose `Host` is not `ABC_PUBLIC_URL`'s host (DNS
   rebinding). For unsafe methods it also refuses requests whose `Sec-Fetch-Site` is not
   `same-origin` or whose `Origin` is not the public origin (CSRF). `ApiJson` requires
   `application/json`, which forces a preflight, and the server answers no preflights.
4. **No identity means no identity.** A missing header (tagged device, Funnel) is
   `tailnet_identity_required`, and a duplicate or malformed one is `malformed_request`. Workers
   authenticate by bearer key only and ignore `Tailscale-*`.
5. The public webhook path (S7) will be a third listener with its own router and no identity.

### Owner claim (D4, I2, D12)

`abc server setup-code` runs in a shell on the host. It calls `Store::issue_setup_code`, which
executes `IssueSetupCode` as `Caller::Server`. The command mints a 100-bit code and records
`installation.setup_code_issued { digest, owner, workspace, expires_at }`. The owner and
workspace ids are reserved by the first code and carried over by every later one. The CLI prints
the plaintext with `writeln!(stdout)` to that terminal. The running server never holds a plaintext
code, so it cannot log one.

`POST /api/owner/claim` needs two factors: the sidecar-vouched login (who) and the code
(operator consent). The `installation` view gives single use: a second claim finds it claimed
whatever code it presents. A newer code supersedes older ones. Codes expire after an hour.

### Enrolment token lifecycle

| Step | Mechanism |
| --- | --- |
| Issue | Owner only. `abc_enr_` + 52 base32 characters (256 bits). Reserves a `WorkerId`. The event holds the SHA-256 digest only. |
| Display once | The plaintext exists only in `TokenIssued`, serialized through `serialize_exposed` in the one response. A retried issue mints a new token; the orphan expires (30 min default, 5 min to 24 h). |
| Redeem | The worker generates `abc_wk_…` and saves it **before** sending `{ token, key_digest, machine, build }`. Under the lock: open, unexpired, unrevoked -> `token.redeemed` + `worker.enrolled` (actor: the reserved worker; causation: the token's `issued` event). |
| Lost response | Same token and same key digest -> `TokenProof { replay: true }` -> the same `Enrolled`, zero events. Replay is checked **before** expiry, so a late retry still gets its answer. |
| Replay | Same token, different key -> `enrolment_token_used`. |
| Expiry | Derived from `expires_at` at check time. No sweeper, no status column. |
| Revocation | `RevokeEnrolmentToken` (idempotent; refuses a redeemed token, for which `RevokeWorker` is the tool). |
| Ongoing credential | `Authorization: Bearer abc_wk_…` on every WebSocket upgrade, looked up by digest in the request's transaction. Revocation: `401 worker_revoked` before any upgrade; live sessions re-check on `LogFeed` wake and close with 4403. |

### Sessions, presence and SSE

- `StartSession` (actor: the worker) sets the `worker` view's `open_session`. "Online" means
  `open_session IS NOT NULL`, derived from the log and nowhere else.
- `EndSession` (actor: `Server`, which observed the close) clears `open_session` only when it names
  that session. A superseded session's late end, a repeat, or a session the boot sweep already
  closed all append nothing. A revoked worker's last session still ends in the log.
- **Boot sweep.** Before the sidecar socket is bound, `run` ends every open session with
  `ServerRestart`. No socket can be live yet, so every open session is stale, and "online" is never
  wrong for longer than the server was down.
- **Newest wins** in-process: `Sessions::attach` kicks an older socket for the same worker (close
  4409), because a laptop waking from sleep usually reconnects before the server notices the dead
  socket.
- **SSE** (`GET /api/workspaces/{ws}/live`) sends read models (`MachineCard`, `online`
  included), never raw events. The `id:` of each message is its `head`. A fresh connection gets
  `Snapshot`. `EventSource`'s automatic reconnect sends `Last-Event-ID`, and the server answers with
  `MachinesSince { after }`, which is complete for any N thanks to `changed_at` and gapless
  positions. Every batch is re-authorized, and on denial the stream sends `AccessEnded` and closes.

### Errors, secrets, logs, generated types, tests

- **Errors (D11).** abc-core: `Denied { action, reason: DenialReason }`, plus a `Rejection` type
  per command. abc-store: `ExecuteError<R>`, `QueryError`, `AuthError`, `StoreError`, `OpenError`.
  abc-server: `ApiError`, `ServeError`. Each maps exhaustively to one `(StatusCode, ApiCode)`;
  bodies are `{"error":{"code","message"}}`, and messages never echo input.
- **Logs (D12).** One JSON subscriber builder serves both the binary and the tests. Spans record
  `request_id`, method, `MatchedPath`, status, latency, actor and workspace. They never record
  headers, raw URIs or bodies. `no_secret_reaches_the_log` captures TRACE output across every
  flow, failures included, and searches it for each plaintext.
- **Generated types.** Web-facing types derive `TS` and `JsonSchema`. `WebApi` is the single
  generation root. `generated_bindings_are_current` compares the output with the committed files,
  and `ABC_UPDATE_BINDINGS=1` rewrites them.
- **Tests (D7).** `#[sqlx::test(migrator = "abc_store::MIGRATOR")]` gives a fresh migrated
  database per test. abc-store's `testing` feature provides `ManualClock`, `SeededMint`, `Fixture`
  and `assert_views_replay`. abc-server tests play the sidecar: they serve `SidecarApp` on a real
  Unix socket in a temp dir, set the identity headers themselves, and drive WebSocket and SSE with
  real clients.

### Interface depth

abc-store's whole write and read surface is `execute` and `query` (plus `feed`). Behind them sit
the lock protocol, authentication, fact loading, the check, sequencing, projection, notification
and digest handling. Adding a command takes a struct with `Request` and `Command` impls, event
variants, an `Executable` loader (the trait bound on `execute` fails until it exists), projection
arms (an exhaustive match fails until they exist) and a handler.

### What this design deliberately does not do

It has no async projector, no sweeper jobs, no identity on TCP, no dev identity bypass, no
server-generated worker credential, no stored responses for idempotency, no secrets at rest (only
digests), no in-memory domain state, and it never sends raw events to the browser.

## Synthesis decision

Two candidates ran independently on the same brief: one on Opus (xhigh) and one on Sonnet. The
skill's default second runner is a non-Claude model, which is not available here. The cross-judge
was **skipped**: arena needs a judge from a different model family than the parent, and every
available model is Claude. I scored both candidates myself against the rubric: D10 and TOCTOU;
whether the log carries S1; whether the sidecar trust can be bypassed; the enrolment lifecycle;
interface depth; testability.

**Base: the Opus candidate.** Its single `Store::execute` loop authenticates, loads facts, checks,
decides and appends under one lock, so TOCTOU is impossible by construction. Its public store
surface is smallest. abc-server has no sqlx. The identity extractor is typed to the sidecar router,
so misuse fails to compile. Tokens reserve their worker id, which gives idempotent redemption and
correct attribution.

**Grafted from the Sonnet candidate:**

1. **The CLI mints the setup code, and its digest goes through the log.** The base held the digest
   in memory and wrote the plaintext to a 0600 file, rotated on restart. The graft leaves no
   plaintext at rest and none in the server process, keeps no in-memory domain state, and survives
   restarts.
2. **Presence derived from the log.** `open_session` in the view, session ends recorded by
   `Server`, and the boot sweep. The base kept online-ness in process memory and merged it at the
   SSE boundary. That meant two sources, and a revoked worker's last session end went unrecorded.
3. **SSE `id:` = head with `Last-Event-ID` resume.** The base re-snapshotted on every reconnect.
4. **A `Host` check against `ABC_PUBLIC_URL`** (DNS rebinding), added to the base's CSRF guard.

**Considered and rejected from the Sonnet candidate:**

- `Gate::run`, with an `acl_epoch` comparison and up to three retries, closes TOCTOU by retry
  instead of by construction. It also exposes a store surface of 13 methods.
- A gapless sequence per workspace takes a lock per workspace instead of one global lock. It was
  rejected because consumers would need a cursor per workspace (the S1 runner, any
  cross-workspace view). It is open question 2.
- Verifiers in a table beside the log are a second write path outside the events. Digests of
  256-bit secrets are not secrets.
- The `Facts` port trait gives the same forgeability as plain data, with more surface.
- A runtime `ConnectInfo<FromSidecar>` check catches mistakes later than a state-typed extractor,
  which fails to compile.

**Convergence** (strong agreement, shipped as is): Unix-socket-only identity; a worker-generated
key with idempotency keyed on its digest; a CSRF guard; views in the same transaction plus a
replay test; gapless commit-ordered positions; digest-only storage; `Worker` as an actor; no dev
identity bypass; the TOCTOU concern itself.

**Verification:** `RUSTFLAGS=--cap-lints=warn cargo check --workspace --all-targets` passes, so
the type sketch is coherent. With lints at full strength, 13 deny-level errors remain, all from
placeholder bodies (`let _ =` on values with destructors, and the like). Clippy was not run. No
test runs.

## Tradeoffs accepted

- We accept **serialized writes per installation** (one head lock) in exchange for gapless,
  commit-ordered positions, trivial tailing, and a check-then-write with no retries. At about 1 to
  5 ms per command that is hundreds of writes per second. Agent steps are coarse.
- We accept **one server process per installation** (in-process session supersession and the
  boot sweep assume it) in exchange for having no distributed presence protocol. High availability
  is a later decision.
- We accept that **decisions read views** in exchange for lookups by any key and no snapshots. The
  replay test is the guard.
- We accept **one fact loader per command in abc-store** in exchange for a pure abc-core. A missing
  loader fails a trait bound.
- We accept that **issuing a token is not idempotent** (a retry mints a new one) in exchange for
  never storing a plaintext. Orphans expire and are listed.
- We accept **a wider abc-protocol** (`ApiCode`, `enrol`, `link`) that depends on abc-core, in
  exchange for one list of codes and domain types at the wire.
- We accept **a `Server` actor** (and a `Server` caller) in exchange for recording what the
  installation itself does: session ends it saw, the boot sweep, setup codes. The matrix limits it
  to those actions.
- We accept **`ABC_PUBLIC_URL` as required configuration** in exchange for the Host check and a
  correct enrol command in the UI.

## Alternatives considered

- **Check in the handler, pass `Authorized` to one store method per mutation.** Each mutation gets
  its own store method, so the surface is wide. Facts are read outside the write transaction (or
  re-checked with an epoch and retries), and reads are easy to forget. It lost on interface depth
  and on correctness by construction.
- **Concurrent appenders with optimistic concurrency per stream.** Positions commit out of order,
  so every consumer needs `pg_snapshot_xmin` filtering and two-part cursors, and decisions need
  retry loops. That machinery spreads across every consumer, for throughput abc does not need.
- **Pure event sourcing, folding streams for each decision.** It needs a fold per command,
  snapshots for long streams, and lookup views anyway. Views plus the replay test give the same
  "the log is the truth" guarantee with less surface.
- **An async projector.** It breaks "views in the same transaction" and read-your-writes.
- **Loopback TCP in a shared network namespace.** It matches Tailscale's guidance, but one bind or
  port-mapping mistake by a stranger opens header forgery. A Unix socket turns the boundary into a
  filesystem permission the server checks at start.
- **A server-generated worker credential in the enrolment response.** A lost response burns the
  token. Making it idempotent would mean storing the plaintext so it can be replayed.

## Open questions and risks

Each has my recommended answer.

1. **Does I4's "a person or an agent" admit `Worker` (a machine acting under its own credential)
   and `Server` (the installation itself) as actors?** *Recommended: yes. Amend I4's wording in the
   brief, and in AGENTS.md once s0-gates lands. `Agent` joins in S1.*
2. **Is one global write lock acceptable for teams, or should positions be sequenced per
   workspace now?** *Recommended: global. Revisit only with measured contention. Sharding later
   changes the cursor model, so decide before S1 builds on it.*
3. **A dev mode without Tailscale?** *Recommended: no. Tests and Playwright play the sidecar over
   the real socket, and developers run `tailscale serve` against it. A bypass flag would be a
   second identity path.*
4. **Do the server sides of the owner claim and SSE land in PR 3 or PR 5?** *Recommended: PR 3.
   Enrolment tokens need an owner, and the integration tests need the whole flow. PR 5 builds the
   screens.*
5. **Is `abc server setup-code` the only way to mint a setup code (plaintext on the operator's
   terminal, digest in the log)?** *Recommended: yes. Shell access to the host is the root of
   trust, and an environment variable would leak into compose files and `docker inspect`.*
6. **Is `ABC_PUBLIC_URL` required configuration?** *Recommended: yes. The Host check and the
   enrol command both need it, and the server refuses to start without a valid https origin.*
7. **Generated TypeScript before `web/` exists?** *Recommended: PR 3 commits
   `web/src/generated/` with the drift test, so PR 5 imports finished types.*
8. **sqlx offline metadata and pinning `sqlx-cli`** (in `xtask/tools.toml`, owned by s0-gates).
   *Recommended: commit `crates/abc-store/.sqlx/` in PR 3, and add the pin and a
   `cargo sqlx prepare --check` gate in a follow-up after s0-gates merges.*
9. **cargo-deny `multiple-versions = "deny"`.** Resolving this sketch already gives duplicates
   (`sha2` 0.10 and 0.11 via sqlx, `getrandom` 0.2, 0.3 and 0.4, `rand` 0.9 and 0.10, `hashbrown`,
   `base64`). *Recommended: align our pins with sqlx's where possible, then add dated `skip`
   entries naming sqlx as the holder.*
10. **Does Tailscale Serve proxy WebSocket upgrades and unbuffered SSE to a `unix:` target in the
    pinned `tailscale/tailscale` image?** *Recommended: spike this first (P1). If it fails, stop
    and decide. Do not quietly fall back to loopback.*
11. **Add `disallowed-methods = ["axum::serve"]` to `clippy.toml`** (a gate file under D5)?
    *Recommended: yes, in this PR, with the review label.*

## Next implementation step

Spike open question 10. Then write `0001_event_log.sql` and abc-store's `WriteTx::{begin, append}`
with the head lock, plus `assert_views_replay`, and make the first `#[sqlx::test]` prove gapless,
commit-ordered positions under concurrent writers.

## Agreed (1 Nov 2026)

The user chose these answers to the open questions:

1. **Actors.** I4 is to be amended so that an actor is a person, an agent, a worker or the server. The amendment lands once s0/gates has merged, because AGENTS.md is a gate file.
2. **Write order.** There is one global, gapless order: a single writer behind the head-row lock. Per-workspace sequences are not built.
3. **Dev mode.** There is none. Identity only ever comes from the sidecar socket, and tests play the sidecar on that real socket.
4. **First owner.** The only way to get a setup code is `abc server setup-code`. It prints the code to that terminal and stores only the code's digest. The code is never written to disk or to logs.

The orchestrator settled the rest by default:

- The server sides of the owner claim and of SSE land in PR 3.
- `ABC_PUBLIC_URL` is required.
- The generated TypeScript is committed under `web/src/generated/`, with a drift test.
- `.sqlx/` is committed in PR 3. The `sqlx-cli` pin follows once s0/gates has merged.
- Duplicate crate versions: our pins are aligned with sqlx's, and any remaining duplicates get skip entries in deny.toml, each with a date.
- `axum::serve` is added to the disallowed methods. That change goes through the gate-file review.
- The Tailscale `unix:` passthrough for WebSocket and SSE is checked before anything else in PR 3. If it fails, PR 3 stops for a decision; it does not fall back to loopback.

## Implementation notes (PR 3)

Where the code differs from the sketch above, and why:

- **No `OwnerOnly`.** With `Owner` as the only role, the matrix could never produce it, so the
  denial and its `ApiCode` are gone (S4). They return with the second role.
- **`method_not_allowed`.** A route hit with the wrong method answers 405 with this code, in the
  API's error format.
- **Session admission.** A session records `StartSession` first and attaches second, and starts
  are admitted one at a time in the process. Attaching first, as sketched, let the kicked older
  session's `EndSession` commit before the newer start, and two simultaneous reconnects could
  leave the view naming a closed session. Now the session recorded last is always the one
  attached, and a kicked session's end always records nothing.
- **`AccessEnded { head }`.** The message carries the head so its SSE `id:` is a position like
  every other message's.
- **Socket directory check.** `SidecarSocket::bind` refuses a directory with any "other"
  permission bit, a missing directory and a non-socket file in the way. It does not compare the
  directory's owner with the process's uid: the standard library has no `getuid`, and the mode
  check is the boundary that matters.
- **`Tailscale-User-Name` is not read.** The claim form sends the display name the owner typed.
- **Middleware.** The request id (a `UUIDv7`, the events' correlation id), the request span and
  the 30-second timeout on non-streaming routes are small axum middleware in `api`, so every
  refusal keeps the API's error format; tower-http is not used.
- **Configuration.** `ServerConfig` also carries `database_wait` (`ABC_DATABASE_WAIT`, default
  60 seconds) and `link` (the worker link's handshake, ping and idle timings, the protocol's own
  unless a test shortens them).
- **Test databases.** Tests take a fresh database from `abc_store::testing::TestDatabase`
  instead of `#[sqlx::test]`, which names each database after its test alone: two runs of one
  test at once (cargo-mutants' parallel jobs, or two checkouts sharing the machine's warm
  container) dropped each other's database. `TestDatabase` names each one uniquely, drops it when
  the test ends, and sweeps leftovers older than a day.
- **Open question 8.** s0/gates had merged, so the `sqlx-cli` pin (`cargo-sqlx` in
  `xtask/tools.toml`) and the query cache check (`cargo xtask check` runs `sqlx prepare --check`
  for every crate with a `.sqlx/` against a fresh migrated database) land in this PR.
- **Open question 10.** The Tailscale `unix:` passthrough check needs a tailnet auth key. Its
  harness is opt-in (see `crates/abc-server/tests/tailscale.rs`); until it has run, the
  passthrough of WebSocket upgrades and unbuffered SSE through the pinned image is unverified.
