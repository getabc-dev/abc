//! End-to-end flows against real Postgres (`#[sqlx::test]`, a fresh migrated database each) and a
//! real Unix socket: the test plays the Tailscale sidecar by setting the identity headers itself.
//! Sketch of the suite; harness bodies omitted.

use abc_protocol::api::ApiCode;
use sqlx::PgPool;

/// Serves `SidecarApp` on a socket in a temp dir with a ManualClock and SeededMint, and offers
/// `as_tailnet(login)` / `anonymous()` / `worker(key)` clients over hyper and tokio-tungstenite.
struct Harness;

#[sqlx::test(migrator = "abc_store::MIGRATOR")]
async fn owner_claims_with_the_setup_code_once(pool: PgPool) {
    // code from store.issue_setup_code() (what the CLI calls); claim as alice -> 200; claim again
    // as alice or bob with the same code -> 409 server_already_claimed; GET /api/session as alice
    // -> Member.
    let _ = (pool, ApiCode::ServerAlreadyClaimed, Harness);
}

#[sqlx::test(migrator = "abc_store::MIGRATOR")]
async fn a_wrong_setup_code_is_refused_and_leaves_the_server_unclaimed(pool: PgPool) {
    let _ = pool;
}

#[sqlx::test(migrator = "abc_store::MIGRATOR")]
async fn identity_headers_are_ignored_off_the_sidecar_socket(pool: PgPool) {
    // ops listener: GET /api/session with Tailscale-User-Login -> 404 (no such route there).
    // The real proof is structural (SidecarApp, FromRequestParts<SidecarState>); this pins it.
    let _ = pool;
}

#[sqlx::test(migrator = "abc_store::MIGRATOR")]
async fn cross_site_posts_cannot_use_tailnet_identity(pool: PgPool) {
    // POST issue-token with Sec-Fetch-Site: cross-site -> 403 cross_site_request; nothing appended.
    let _ = pool;
}

#[sqlx::test(migrator = "abc_store::MIGRATOR")]
async fn enrolment_survives_a_lost_response_and_rejects_replay(pool: PgPool) {
    // issue -> enrol(k1) -> enrol(k1) returns the same EnrolResponse with the event count unchanged;
    // enrol(k2) -> 403 enrolment_token_used; clock.advance(31 min) then enrol(k1) still returns the
    // same EnrolResponse (replay is checked before expiry).
    let _ = pool;
}

#[sqlx::test(migrator = "abc_store::MIGRATOR")]
async fn expired_and_revoked_tokens_are_refused(pool: PgPool) {
    let _ = pool;
}

#[sqlx::test(migrator = "abc_store::MIGRATOR")]
async fn worker_handshake_records_a_session_and_shows_online_over_sse(pool: PgPool) {
    // SSE as alice -> Snapshot (offline) -> worker connects, Hello -> Welcome -> SSE gets
    // Machines{online: true} -> worker closes -> Machines{online: false}. Reconnect the SSE with
    // Last-Event-ID = the first head -> exactly the cards changed since, no snapshot.
    let _ = pool;
}

#[sqlx::test(migrator = "abc_store::MIGRATOR")]
async fn an_unsupported_protocol_version_is_rejected(pool: PgPool) {
    let _ = pool;
}

#[sqlx::test(migrator = "abc_store::MIGRATOR")]
async fn a_reconnect_supersedes_the_old_session(pool: PgPool) {
    // session A open; session B with the same key -> A closes 4409; A's EndSession is a no-op, so
    // the card stays online with open_session = B.
    let _ = pool;
}

#[sqlx::test(migrator = "abc_store::MIGRATOR")]
async fn revoking_a_worker_closes_its_session_and_blocks_reconnect(pool: PgPool) {
    // revoke -> live socket closes 4403 -> reconnect -> 401 worker_revoked before upgrade.
    let _ = pool;
}

#[sqlx::test(migrator = "abc_store::MIGRATOR")]
async fn a_restart_ends_sessions_the_previous_process_left_open(pool: PgPool) {
    // worker connects; drop the server without draining (abort); start a new server on the same
    // database -> before the socket is bound, the worker's card shows online: false with a
    // session_ended { reason: server_restart } recorded by actor server.
    let _ = pool;
}

#[sqlx::test(migrator = "abc_store::MIGRATOR")]
async fn a_request_naming_another_host_cannot_use_tailnet_identity(pool: PgPool) {
    // GET /api/session with Host: evil.example (DNS rebinding) and the identity header ->
    // 403 cross_site_request.
    let _ = pool;
}

#[sqlx::test(migrator = "abc_store::MIGRATOR")]
async fn sse_ends_when_access_is_lost(pool: PgPool) {
    // (exercised with a store-level fixture that removes membership, once teams exist; today:
    // a non-member visitor's SSE request is refused with 403 not_a_member before streaming.)
    let _ = pool;
}

#[sqlx::test(migrator = "abc_store::MIGRATOR")]
async fn no_secret_reaches_the_log(pool: PgPool) {
    // telemetry::subscriber(EnvFilter::new("trace"), buffer) as the default for this test's
    // runtime (current-thread, so spawned server tasks log into it too).
    // Run: claim (wrong code, right code), issue token, enrol (ok, replay, other key, malformed
    // token), connect (good key, unknown key, malformed bearer), revoke.
    // Assert: the captured text contains none of the setup code (with and without dashes), the
    // token, the keys, or the database password.
    let _ = pool;
}

#[test]
fn generated_bindings_are_current() {
    // Render ts-rs declarations for WebApi and its dependencies, and schemars' schema_for!(WebApi);
    // compare with web/src/generated/*.ts and web-api.schema.json; ABC_UPDATE_BINDINGS=1 rewrites.
}
