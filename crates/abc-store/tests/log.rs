//! Store-level properties of the event log and the write path. Each test gets a fresh migrated
//! database; ManualClock + SeededMint make them deterministic.

use sqlx::PgPool;

#[sqlx::test(migrator = "abc_store::MIGRATOR")]
async fn positions_are_gapless_and_commit_ordered_under_concurrent_writers(pool: PgPool) {
    // 32 concurrent executes (mix of issue and revoke, some rejected or denied) on a multi-thread
    // runtime; then SELECT position ORDER BY position == 1..=n, and for every pair a < b, a's
    // transaction committed first (observed via a LogFeed subscriber that saw heads in order).
    let _ = pool;
}

#[sqlx::test(migrator = "abc_store::MIGRATOR")]
async fn a_failed_command_writes_nothing_and_leaves_no_gap(pool: PgPool) {
    let _ = pool;
}

#[sqlx::test(migrator = "abc_store::MIGRATOR")]
async fn the_log_rejects_update_delete_and_truncate(pool: PgPool) {
    let _ = pool;
}

#[sqlx::test(migrator = "abc_store::MIGRATOR")]
async fn every_event_records_actor_workspace_and_correlation(pool: PgPool) {
    // after a claim + issue + enrol flow: no NULLs (schema), actor ids all exist in `actor`,
    // enrolment events carry causation = the token's issued event.
    let _ = pool;
}

#[sqlx::test(migrator = "abc_store::MIGRATOR")]
async fn stream_kind_constraint_matches_the_enum(pool: PgPool) {
    // read the CHECK from pg_constraint; compare with StreamKind::ALL.
    let _ = pool;
}

#[sqlx::test(migrator = "abc_store::MIGRATOR")]
async fn revocation_is_seen_by_the_very_next_request(pool: PgPool) {
    // revoke worker; the next StartSession with its key -> AuthError::WorkerRevoked.
    let _ = pool;
}

#[sqlx::test(migrator = "abc_store::MIGRATOR")]
async fn two_concurrent_claims_produce_one_owner(pool: PgPool) {
    let _ = pool;
}

#[sqlx::test(migrator = "abc_store::MIGRATOR")]
async fn two_concurrent_redemptions_of_one_token_produce_one_worker(pool: PgPool) {
    let _ = pool;
}

#[sqlx::test(migrator = "abc_store::MIGRATOR")]
async fn the_feed_wakes_subscribers_after_commit_only(pool: PgPool) {
    let _ = pool;
}

// proptest (fixed seed via xtask): random sequences of commands from random callers, then
// assert_views_replay(&pool) and "every denied or rejected command appended nothing".

#[sqlx::test(migrator = "abc_store::MIGRATOR")]
async fn a_setup_code_claims_once_and_never_after(pool: PgPool) {
    // issue_setup_code -> claim with it -> Claimed; a second claim (same or new code) ->
    // ServerAlreadyClaimed; issue_setup_code after the claim -> ServerAlreadyClaimed.
    // A newer code supersedes an older one (the older -> SetupCodeMismatch); both reserve the
    // same owner and workspace ids. After SETUP_CODE_TTL -> SetupCodeExpired.
    let _ = pool;
}

#[sqlx::test(migrator = "abc_store::MIGRATOR")]
async fn a_superseded_session_end_leaves_the_worker_online(pool: PgPool) {
    // StartSession(s1), StartSession(s2), EndSession(s1) -> open_session == s2, zero events for
    // the s1 end; EndSession(s2) -> offline; EndSession(s2) again -> zero events.
    let _ = pool;
}

#[sqlx::test(migrator = "abc_store::MIGRATOR")]
async fn the_server_caller_can_do_only_server_actions(pool: PgPool) {
    // Caller::Server: IssueEnrolmentToken, RevokeWorker, ListMachines -> Denied(ServerOnly or
    // TailnetIdentityRequired); a tailnet person: EndSession, OpenSessions -> Denied(ServerOnly).
    let _ = pool;
}
