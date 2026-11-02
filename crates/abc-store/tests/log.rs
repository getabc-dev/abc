//! Properties of the event log, the write path and the feed. Each test gets a fresh migrated
//! database (`TestDatabase`); `Fixture` fixes the clock and the mints.

#[cfg(test)]
mod tests {
    use abc_core::query::{InstallationState, InstallationStatus};
    use abc_store::Caller;
    use abc_store::testing::TestDatabase;
    use std::time::Duration;

    use abc_core::command::{IssueEnrolmentToken, RevokeEnrolmentToken, RevokeWorker, TokenTtl};
    use abc_core::event::{Position, StreamKind};
    use abc_core::policy::DenialReason;
    use abc_store::testing::{Fixture, assert_views_replay, database_url, views_replay_diff};
    use abc_store::{DatabaseUrl, ExecuteError, OpenError, Store, StoreOptions};
    use rand::rngs::ChaCha20Rng;
    use rand::{RngExt as _, SeedableRng as _};

    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "writers, a reader and a feed, checked together"
    )]
    async fn positions_stay_gapless_to_every_reader_under_concurrent_writers() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let (fixture, claimed) = Fixture::claimed(pool.clone()).await;
        let fixture = std::sync::Arc::new(fixture);
        let start = fixture.event_count().await;
        let mut feed = fixture.store.feed();
        let heads = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Ok(Some(head)) =
                tokio::time::timeout(Duration::from_secs(2), feed.changed()).await
            {
                seen.push(head);
            }
            seen
        });
        // A reader that, at every instant, must see exactly the positions 1..=max: never a hole that a
        // later commit fills.
        let reader_pool = pool.clone();
        let reader = tokio::spawn(async move {
            let mut checks = 0_u32;
            for _ in 0..200 {
                let (max, count): (Option<i64>, i64) =
                    sqlx::query_as("SELECT MAX(position), COUNT(*) FROM event")
                        .fetch_one(&reader_pool)
                        .await
                        .unwrap();
                assert_eq!(max.unwrap_or(0), count, "a reader never sees a gap");
                checks = checks.saturating_add(1);
                tokio::task::yield_now().await;
            }
            checks
        });
        let mut rng = ChaCha20Rng::seed_from_u64(abc_store::testing::SEED);
        let mut writers = Vec::new();
        for n in 0..32_u32 {
            let fixture = std::sync::Arc::clone(&fixture);
            let workspace = claimed.workspace;
            let as_stranger = rng.random_bool(0.25);
            writers.push(tokio::spawn(async move {
                let caller = if as_stranger {
                    Fixture::tailnet("mallory@example.com")
                } else {
                    Fixture::alice()
                };
                let issued = fixture
                    .store
                    .execute(
                        &caller,
                        &fixture.origin(),
                        IssueEnrolmentToken {
                            workspace,
                            label: None,
                            ttl: TokenTtl::default(),
                        },
                    )
                    .await;
                if let Ok(issued) = &issued
                    && n % 2 == 0
                {
                    fixture
                        .store
                        .execute(
                            &Fixture::alice(),
                            &fixture.origin(),
                            RevokeEnrolmentToken {
                                workspace,
                                token: issued.id,
                            },
                        )
                        .await
                        .unwrap();
                }
                issued.is_ok()
            }));
        }
        let mut succeeded = 0_i64;
        let mut issued_and_revoked = 0_i64;
        for (n, writer) in writers.into_iter().enumerate() {
            if writer.await.unwrap() {
                succeeded = succeeded.saturating_add(1);
                if n % 2 == 0 {
                    issued_and_revoked = issued_and_revoked.saturating_add(1);
                }
            }
        }
        assert!(reader.await.unwrap() > 0, "the reader ran");
        let positions: Vec<i64> =
            sqlx::query_scalar("SELECT position FROM event ORDER BY position")
                .fetch_all(&pool)
                .await
                .unwrap();
        let total = start
            .saturating_add(succeeded)
            .saturating_add(issued_and_revoked);
        assert_eq!(
            positions,
            (1..=total).collect::<Vec<_>>(),
            "1..=n, no gaps from denials"
        );
        let heads = heads.await.unwrap();
        assert!(
            heads.windows(2).all(|pair| pair[0] < pair[1]),
            "the feed only moves forward: {heads:?}"
        );
        assert_eq!(
            heads.last().map(|head| head.get()),
            Some(total),
            "the feed reaches the head"
        );
        assert_views_replay(&pool).await;
    }

    #[tokio::test]
    async fn a_refused_command_writes_nothing_and_leaves_no_gap() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let (fixture, claimed) = Fixture::claimed(pool.clone()).await;
        let before = fixture.event_count().await;
        let refused = fixture
            .store
            .execute(
                &Fixture::tailnet("mallory@example.com"),
                &fixture.origin(),
                IssueEnrolmentToken {
                    workspace: claimed.workspace,
                    label: None,
                    ttl: TokenTtl::default(),
                },
            )
            .await;
        assert!(
            matches!(refused, Err(ExecuteError::Denied(denied)) if denied.reason == DenialReason::NotAMember),
            "a stranger is not a member"
        );
        assert_eq!(fixture.event_count().await, before, "nothing was written");
        fixture.issue_token(claimed.workspace).await;
        let last: i64 = sqlx::query_scalar("SELECT MAX(position) FROM event")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            last,
            before.saturating_add(1),
            "the next event takes the next position"
        );
        assert_views_replay(&pool).await;
    }

    #[tokio::test]
    async fn the_log_rejects_update_delete_and_truncate() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let (_fixture, _) = Fixture::claimed(pool.clone()).await;
        for statement in [
            "UPDATE event SET correlation_id = gen_random_uuid()",
            "DELETE FROM event",
            "TRUNCATE event CASCADE",
        ] {
            let error = sqlx::query(statement).execute(&pool).await.unwrap_err();
            assert!(
                error.to_string().contains("the event log is append-only"),
                "{statement}: {error}"
            );
        }
    }

    #[tokio::test]
    async fn the_head_moves_only_forward_and_only_onto_an_event() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let (fixture, _) = Fixture::claimed(pool.clone()).await;
        let head = fixture.event_count().await;
        let backwards = sqlx::query("UPDATE event_log_head SET position = 1")
            .execute(&pool)
            .await
            .unwrap_err();
        assert!(
            backwards.to_string().contains("moved backwards"),
            "{backwards}"
        );
        let beyond = sqlx::query("UPDATE event_log_head SET position = $1")
            .bind(head.saturating_add(1))
            .execute(&pool)
            .await
            .unwrap_err();
        assert!(
            beyond.to_string().contains("with no event there"),
            "{beyond}"
        );
        sqlx::query("UPDATE event_log_head SET position = $1")
            .bind(head)
            .execute(&pool)
            .await
            .unwrap();
    }

    /// position, type, actor kind, actor id, workspace, stream seq, correlation, causation, id.
    type EventRow = (
        i64,
        String,
        String,
        Option<uuid::Uuid>,
        uuid::Uuid,
        i32,
        uuid::Uuid,
        Option<uuid::Uuid>,
        uuid::Uuid,
    );

    #[tokio::test]
    async fn every_event_records_its_actor_workspace_stream_and_cause() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let (fixture, claimed) = Fixture::claimed(pool.clone()).await;
        let issued = fixture.issue_token(claimed.workspace).await;
        let key = fixture.worker_key();
        let enrolled = fixture.enrol(&issued.token, &key).await.unwrap();
        let rows: Vec<EventRow> = sqlx::query_as(
            "SELECT position, event_type, actor_kind, actor_id, workspace_id, stream_seq, correlation_id,
                    causation_id, id
             FROM event ORDER BY position",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let summary: Vec<(String, String, i32)> = rows
            .iter()
            .map(|row| (row.1.clone(), row.2.clone(), row.5))
            .collect();
        let s = |kind: &str, actor: &str, seq: i32| (kind.to_owned(), actor.to_owned(), seq);
        assert_eq!(
            summary,
            [
                s("installation.setup_code_issued", "server", 1),
                s("person.registered", "person", 1),
                s("workspace.created", "person", 1),
                s("workspace.member_added", "person", 2),
                s("installation.claimed", "person", 2),
                s("enrolment_token.issued", "person", 1),
                s("enrolment_token.redeemed", "worker", 2),
                s("worker.enrolled", "worker", 1),
            ],
            "who did what, numbered within each stream"
        );
        assert!(
            rows.iter().all(|row| row.4 == claimed.workspace.as_uuid()),
            "everything belongs to the workspace (I4)"
        );
        let owner = Some(claimed.owner.as_uuid());
        assert!(
            rows[1..6].iter().all(|row| row.3 == owner),
            "the owner acted"
        );
        let worker = Some(enrolled.worker.as_uuid());
        assert!(
            rows[6..].iter().all(|row| row.3 == worker),
            "the worker enrolled itself"
        );
        let issued_event = rows[5].8;
        assert!(
            rows[6..].iter().all(|row| row.7 == Some(issued_event)),
            "enrolment is caused by the token's issue"
        );
        assert_eq!(rows[1].6, rows[4].6, "one request, one correlation id");
        assert_ne!(rows[5].6, rows[6].6, "two requests, two correlation ids");
        assert_views_replay(&pool).await;
    }

    #[tokio::test]
    async fn the_stream_kind_constraint_matches_the_enum() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let definition: String = sqlx::query_scalar(
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint
             WHERE conrelid = 'event'::regclass AND pg_get_constraintdef(oid) LIKE '%stream_kind%'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let quoted = definition.matches("::text").count();
        assert_eq!(
            quoted,
            StreamKind::ALL.len(),
            "one name per kind: {definition}"
        );
        for kind in StreamKind::ALL {
            assert!(
                definition.contains(&format!("'{}'", kind.as_str())),
                "{kind:?} is allowed: {definition}"
            );
        }
    }

    #[tokio::test]
    async fn the_feed_wakes_on_commits_and_not_on_refusals() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let (fixture, claimed) = Fixture::claimed(pool.clone()).await;
        let head = Position::from_stored(fixture.event_count().await).unwrap();
        let mut early = fixture.store.feed();
        while early.head() < head {
            tokio::time::timeout(Duration::from_secs(5), early.changed())
                .await
                .unwrap();
        }
        let mut feed = fixture.store.feed();
        assert_eq!(feed.head(), head, "a new feed starts at the head");
        let refused = fixture
            .store
            .execute(
                &Fixture::tailnet("mallory@example.com"),
                &fixture.origin(),
                IssueEnrolmentToken {
                    workspace: claimed.workspace,
                    label: None,
                    ttl: TokenTtl::default(),
                },
            )
            .await;
        assert!(refused.is_err(), "mallory is refused");
        assert!(
            tokio::time::timeout(Duration::from_millis(300), feed.changed())
                .await
                .is_err(),
            "a refusal wakes no one"
        );
        fixture.issue_token(claimed.workspace).await;
        let woke = tokio::time::timeout(Duration::from_secs(5), feed.changed())
            .await
            .unwrap();
        assert_eq!(woke, head.next(), "one commit, one step");
        assert_eq!(feed.head(), woke.unwrap(), "head() is the latest");
    }

    #[tokio::test]
    async fn a_lost_listener_connection_loses_no_wake_up() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let (fixture, claimed) = Fixture::claimed(pool.clone()).await;
        let mut feed = fixture.store.feed();
        let killed: i64 = sqlx::query_scalar(
            "SELECT COUNT(pg_terminate_backend(pid)) FROM pg_stat_activity
             WHERE datname = current_database() AND query ILIKE 'LISTEN%'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(killed, 1, "the store's listener was found and cut");
        fixture.issue_token(claimed.workspace).await;
        let expected = Position::from_stored(fixture.event_count().await).unwrap();
        let mut seen = feed.head();
        let deadline = tokio::time::Instant::now()
            .checked_add(Duration::from_secs(20))
            .unwrap();
        while seen < expected {
            seen = tokio::time::timeout_at(deadline, feed.changed())
                .await
                .expect("the feed catches up after reconnecting")
                .unwrap();
        }
        assert_eq!(
            seen, expected,
            "the head written while disconnected is published"
        );
        for _ in 0..100 {
            if fixture.store.ready().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(
            fixture.store.ready().await,
            Ok(()),
            "ready again once reconnected"
        );
    }

    #[tokio::test]
    async fn replay_catches_a_view_written_outside_the_event_path() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let (fixture, claimed) = Fixture::claimed(pool.clone()).await;
        fixture.issue_token(claimed.workspace).await;
        assert_eq!(
            views_replay_diff(&pool).await.unwrap(),
            None,
            "the views match the log"
        );
        sqlx::query("UPDATE workspace SET name = 'Renamed behind the log'")
            .execute(&pool)
            .await
            .unwrap();
        let difference = views_replay_diff(&pool).await.unwrap().unwrap();
        let tampered = pool.clone();
        let check = tokio::spawn(async move { assert_views_replay(&tampered).await }).await;
        assert!(
            check.is_err_and(|error| error.is_panic()),
            "assert_views_replay fails the test"
        );
        assert!(
            difference.starts_with("view workspace differs from its replay"),
            "the divergent view is named: {difference}"
        );
    }

    #[tokio::test]
    async fn a_store_opens_only_on_this_builds_schema() {
        let database = TestDatabase::empty().await;
        let pool = database.pool();
        let clock = abc_store::testing::ManualClock::new();
        let unmigrated = Store::open(
            abc_store::testing::database(pool.clone()),
            abc_store::testing::options(&clock, 1),
        )
        .await;
        assert!(
            matches!(unmigrated, Err(OpenError::NotMigrated)),
            "never migrated"
        );
        sqlx::query("CREATE TABLE _sqlx_migrations (version bigint)")
            .execute(&pool)
            .await
            .unwrap();
        let broken = Store::open(
            abc_store::testing::database(pool.clone()),
            abc_store::testing::options(&clock, 1),
        )
        .await;
        assert!(
            matches!(broken, Err(OpenError::Database(_))),
            "a migration table this build cannot read is a database error, not a fresh database"
        );
        sqlx::query("DROP TABLE _sqlx_migrations")
            .execute(&pool)
            .await
            .unwrap();
        abc_store::migrate(&abc_store::testing::database(pool.clone()))
            .await
            .unwrap();
        abc_store::migrate(&abc_store::testing::database(pool.clone()))
            .await
            .unwrap();
        let store = Store::open(
            abc_store::testing::database(pool.clone()),
            StoreOptions::production(),
        )
        .await
        .unwrap();
        assert!(
            !(store
                .query(&Caller::Server, InstallationStatus)
                .await
                .unwrap()
                == InstallationState::Claimed),
            "a new server is unclaimed"
        );
        assert_eq!(store.ready().await, Ok(()), "and ready");
        assert_eq!(
            format!("{store:?}"),
            "Store { .. }",
            "Debug shows no internals"
        );
        drop(store);
        sqlx::query("INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (99990101000000, 'from the future', true, '\\x00', 0)")
            .execute(&pool)
            .await
            .unwrap();
        let newer = Store::open(
            abc_store::testing::database(pool),
            abc_store::testing::options(&clock, 1),
        )
        .await;
        assert!(
            matches!(newer, Err(OpenError::NotMigrated)),
            "migrated by a newer build"
        );
    }

    #[tokio::test]
    async fn connect_reaches_a_database_and_never_echoes_the_url() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let db = abc_store::connect(&database_url(&pool), Duration::from_secs(5))
            .await
            .unwrap();
        let store = Store::open(db, StoreOptions::production()).await.unwrap();
        assert_eq!(store.ready().await, Ok(()), "connected");
        let invalid = abc_store::connect(
            &DatabaseUrl::new("not a url:hunter2".to_owned()),
            Duration::ZERO,
        )
        .await
        .unwrap_err();
        assert!(matches!(invalid, OpenError::InvalidUrl), "a malformed URL");
        assert!(
            !invalid.to_string().contains("hunter2"),
            "the message never echoes the URL"
        );
        let unreachable = abc_store::connect(
            &DatabaseUrl::new("postgres://abc:hunter2@127.0.0.1:1/abc".to_owned()),
            Duration::from_millis(300),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(unreachable, OpenError::Connect(_)),
            "nothing listens on port 1"
        );
        assert!(
            !unreachable.to_string().contains("hunter2"),
            "never echoes the URL"
        );
        assert_eq!(
            format!(
                "{:?}",
                DatabaseUrl::new("postgres://x:hunter2@h/d".to_owned()).expose_secret()
            ),
            "DatabaseUrl(<redacted>)",
            "nor does Debug"
        );
    }

    #[tokio::test]
    async fn a_test_database_is_dropped_after_its_test_and_old_leftovers_are_swept() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let name: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(
            name.starts_with("abc_test_"),
            "named for abc's tests: {name}"
        );
        sqlx::query("CREATE DATABASE abc_test_1_leftover")
            .execute(&pool)
            .await
            .unwrap();
        let fresh = TestDatabase::empty().await;
        let exists = |wanted: &str| {
            let pool = pool.clone();
            let wanted = wanted.to_owned();
            async move {
                sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)",
                )
                .bind(wanted)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        assert!(
            !exists("abc_test_1_leftover").await,
            "a day-old leftover is swept"
        );
        let fresh_name: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(&fresh.pool())
            .await
            .unwrap();
        assert!(
            exists(&fresh_name).await,
            "the new database exists while held"
        );
        drop(fresh);
        assert!(!exists(&fresh_name).await, "and is dropped with its value");
    }

    #[tokio::test]
    async fn a_store_whose_database_turns_away_connections_is_not_ready() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let admin = TestDatabase::empty().await;
        let fixture = Fixture::new(pool.clone()).await;
        assert_eq!(fixture.store.ready().await, Ok(()), "ready at first");
        let name: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(&pool)
            .await
            .unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "ALTER DATABASE {name:?} ALLOW_CONNECTIONS false"
        )))
        .execute(&admin.pool())
        .await
        .unwrap();
        sqlx::query("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = $1")
            .bind(&name)
            .execute(&admin.pool())
            .await
            .unwrap();
        assert_eq!(
            fixture.store.ready().await,
            Err(abc_store::NotReady::Database),
            "a database that refuses connections is not ready"
        );
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "ALTER DATABASE {name:?} ALLOW_CONNECTIONS true"
        )))
        .execute(&admin.pool())
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn one_process_holds_the_installation_at_a_time() {
        let database = TestDatabase::migrated().await;
        let fixture = Fixture::new(database.pool()).await;
        let held = fixture.store.lock_installation().await.unwrap();
        assert!(held.is_some(), "the first process gets the lock");
        assert!(
            fixture.store.lock_installation().await.unwrap().is_none(),
            "a second is refused"
        );
        assert_eq!(
            format!("{held:?}"),
            "Some(InstallationLock)",
            "Debug names it"
        );
        drop(held);
        let mut again = None;
        for _ in 0..50 {
            again = fixture.store.lock_installation().await.unwrap();
            if again.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(again.is_some(), "released when the holder drops it");
        let first: abc_core::id::SessionId = fixture.store.fresh_id().unwrap();
        let second: abc_core::id::SessionId = fixture.store.fresh_id().unwrap();
        assert_ne!(first, second, "fresh ids are fresh");
    }

    #[tokio::test]
    async fn scoped_feeds_wake_for_their_workspace_and_worker_only() {
        let database = TestDatabase::migrated().await;
        let (fixture, claimed) = Fixture::claimed(database.pool()).await;
        let issued = fixture.issue_token(claimed.workspace).await;
        let key = fixture.worker_key();
        let enrolled = fixture.enrol(&issued.token, &key).await.unwrap();
        let mut caught_up = fixture.store.feed();
        let head = Position::from_stored(fixture.event_count().await).unwrap();
        while caught_up.head() < head {
            tokio::time::timeout(Duration::from_secs(5), caught_up.changed())
                .await
                .unwrap();
        }
        let mut workspace = fixture.store.workspace_feed(claimed.workspace);
        let mut worker = fixture.store.worker_feed(enrolled.worker);
        fixture.issue_token(claimed.workspace).await;
        let woke = tokio::time::timeout(Duration::from_secs(5), workspace.changed())
            .await
            .unwrap();
        assert_eq!(woke, head.next(), "the workspace saw the new token");
        assert!(
            tokio::time::timeout(Duration::from_millis(300), worker.changed())
                .await
                .is_err(),
            "the worker did not: the token is not its stream"
        );
        fixture
            .store
            .execute(
                &Fixture::alice(),
                &fixture.origin(),
                RevokeWorker {
                    workspace: claimed.workspace,
                    worker: enrolled.worker,
                },
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), worker.changed())
                .await
                .is_ok(),
            "its revocation wakes the worker's feed"
        );
    }
}
