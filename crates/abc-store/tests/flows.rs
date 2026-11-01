//! The domain flows through the store: claiming, enrolment, sessions, revocation and reads. Every
//! test ends by checking that the views are exactly what replaying the log produces.

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use abc_core::command::{
        EndSession, IssueEnrolmentToken, RevokeEnrolmentToken, RevokeTokenRejection, RevokeWorker,
        RevokeWorkerRejection, SETUP_CODE_TTL, StartSession, TokenTtl,
    };
    use abc_core::credential::SetupCode;
    use abc_core::event::{Position, SessionEndReason};
    use abc_core::id::{FromUuid as _, SessionId, WorkerId};
    use abc_core::identity::MachineLabel;
    use abc_core::policy::DenialReason;
    use abc_core::query::{
        ListEnrolmentTokens, ListMachines, MachinesSince, OpenSessions, SessionView, TokenStatus,
        WhoAmI, WorkerSelf,
    };
    use abc_core::time::Clock as _;
    use abc_store::testing::{Fixture, assert_views_replay};
    use abc_store::{AuthError, Caller, ExecuteError, QueryError};
    use semver::Version;
    use sqlx::PgPool;

    fn denial<R: std::error::Error>(
        result: Result<impl std::fmt::Debug, ExecuteError<R>>,
    ) -> DenialReason {
        match result {
            Err(ExecuteError::Denied(denied)) => denied.reason,
            other => panic!("expected a denial, got {other:?}"),
        }
    }

    fn query_denial<T: std::fmt::Debug>(result: Result<T, QueryError>) -> DenialReason {
        match result {
            Err(QueryError::Denied(denied)) => denied.reason,
            other => panic!("expected a denial, got {other:?}"),
        }
    }

    fn session(n: u128) -> SessionId {
        SessionId::from_uuid(uuid::Uuid::from_u128(n))
    }

    #[sqlx::test(migrator = "abc_store::MIGRATOR")]
    async fn a_setup_code_claims_once_and_never_after(pool: PgPool) {
        let fixture = Fixture::new(pool.clone()).await;
        assert!(
            !fixture.store.is_claimed().await.unwrap(),
            "unclaimed at first"
        );
        let first = fixture.store.issue_setup_code().await.unwrap();
        let second = fixture.store.issue_setup_code().await.unwrap();
        assert_eq!(
            denial(fixture.claim_with(&Fixture::alice(), &first.code).await),
            DenialReason::SetupCodeMismatch,
            "a newer code supersedes the older"
        );
        let wrong = SetupCode::parse("00000-00000-00000-00000").unwrap();
        assert_eq!(
            denial(fixture.claim_with(&Fixture::alice(), &wrong).await),
            DenialReason::SetupCodeMismatch,
            "a wrong code"
        );
        assert_eq!(
            denial(fixture.claim_with(&Caller::Anonymous, &second.code).await),
            DenialReason::TailnetIdentityRequired,
            "the claim needs a tailnet identity"
        );
        let claimed = fixture
            .claim_with(&Fixture::alice(), &second.code)
            .await
            .unwrap();
        assert!(fixture.store.is_claimed().await.unwrap(), "claimed");
        assert_eq!(
            denial(
                fixture
                    .claim_with(&Fixture::tailnet("bob@example.com"), &second.code)
                    .await
            ),
            DenialReason::ServerAlreadyClaimed,
            "the same code cannot claim twice"
        );
        assert_eq!(
            denial(fixture.claim_with(&Fixture::alice(), &second.code).await),
            DenialReason::ServerAlreadyClaimed,
            "nor can the owner claim again"
        );
        assert_eq!(
            denial(fixture.store.issue_setup_code().await),
            DenialReason::ServerAlreadyClaimed,
            "no code is issued once claimed"
        );
        let reserved: (uuid::Uuid, uuid::Uuid) =
            sqlx::query_as("SELECT owner_id, workspace_id FROM installation")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            reserved,
            (claimed.owner.as_uuid(), claimed.workspace.as_uuid()),
            "the claim created the ids the first code reserved"
        );
        let codes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM setup_code")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(codes, 0, "the spent code is gone from the view");
        assert_views_replay(&pool).await;
    }

    #[sqlx::test(migrator = "abc_store::MIGRATOR")]
    async fn a_setup_code_expires_after_an_hour(pool: PgPool) {
        let fixture = Fixture::new(pool.clone()).await;
        let issued = fixture.store.issue_setup_code().await.unwrap();
        fixture.clock.advance(SETUP_CODE_TTL);
        assert_eq!(
            denial(fixture.claim_with(&Fixture::alice(), &issued.code).await),
            DenialReason::SetupCodeExpired,
            "an hour later"
        );
        assert!(
            fixture.claim(&Fixture::alice()).await.is_ok(),
            "a fresh code works"
        );
        assert_views_replay(&pool).await;
    }

    #[sqlx::test(migrator = "abc_store::MIGRATOR")]
    async fn two_concurrent_claims_make_one_owner(pool: PgPool) {
        let fixture = Arc::new(Fixture::new(pool.clone()).await);
        let issued = fixture.store.issue_setup_code().await.unwrap();
        let display = issued.code.expose_secret().display_form();
        let claims = ["alice@example.com", "bob@example.com"].map(|login| {
            let fixture = Arc::clone(&fixture);
            let code = SetupCode::parse(&display).unwrap();
            tokio::spawn(async move {
                fixture
                    .claim_with(&Fixture::tailnet(login), &code)
                    .await
                    .is_ok()
            })
        });
        let mut owners = 0_i32;
        for claim in claims {
            owners = owners.saturating_add(i32::from(claim.await.unwrap()));
        }
        assert_eq!(owners, 1, "exactly one claim wins");
        let people: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tailnet_identity")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(people, 1, "one person exists");
        assert_views_replay(&pool).await;
    }

    #[sqlx::test(migrator = "abc_store::MIGRATOR")]
    async fn a_lost_enrolment_response_is_retried_safely(pool: PgPool) {
        let (fixture, claimed) = Fixture::claimed(pool.clone()).await;
        let issued = fixture.issue_token(claimed.workspace).await;
        let key = fixture.worker_key();
        let first = fixture.enrol(&issued.token, &key).await.unwrap();
        assert_eq!(
            first.workspace, claimed.workspace,
            "in the token's workspace"
        );
        assert_eq!(first.name.as_str(), "mbp", "named after the host");
        let events = fixture.event_count().await;
        assert_eq!(
            fixture.enrol(&issued.token, &key).await.unwrap(),
            first,
            "the same token and key name the same worker"
        );
        assert_eq!(
            fixture.event_count().await,
            events,
            "and record nothing new"
        );
        assert_eq!(
            denial(fixture.enrol(&issued.token, &fixture.worker_key()).await),
            DenialReason::EnrolmentTokenUsed,
            "a second key is a replay"
        );
        fixture.clock.advance(time::Duration::hours(1));
        assert_eq!(
            fixture.enrol(&issued.token, &key).await.unwrap(),
            first,
            "a retry still gets its answer after expiry"
        );
        assert_views_replay(&pool).await;
    }

    #[sqlx::test(migrator = "abc_store::MIGRATOR")]
    async fn two_concurrent_redemptions_make_one_worker(pool: PgPool) {
        let (fixture, claimed) = Fixture::claimed(pool.clone()).await;
        let fixture = Arc::new(fixture);
        let issued = fixture.issue_token(claimed.workspace).await;
        let text = issued.token.expose_secret().as_str().to_owned();
        let redemptions: Vec<_> = (0..4)
            .map(|_| {
                let fixture = Arc::clone(&fixture);
                let token = abc_core::credential::EnrolmentToken::parse(&text).unwrap();
                let key = fixture.worker_key();
                tokio::spawn(async move { fixture.enrol(&token, &key).await.is_ok() })
            })
            .collect();
        let mut enrolled = 0_i32;
        for redemption in redemptions {
            enrolled = enrolled.saturating_add(i32::from(redemption.await.unwrap()));
        }
        assert_eq!(enrolled, 1, "one machine wins the token");
        let workers: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM worker")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(workers, 1, "one worker exists");
        assert_views_replay(&pool).await;
    }

    #[sqlx::test(migrator = "abc_store::MIGRATOR")]
    async fn expired_revoked_and_unknown_tokens_are_refused(pool: PgPool) {
        let (fixture, claimed) = Fixture::claimed(pool.clone()).await;
        let workspace = claimed.workspace;
        let revoked = fixture.issue_token(workspace).await;
        let revoke = || RevokeEnrolmentToken {
            workspace,
            token: revoked.id,
        };
        fixture
            .store
            .execute(&Fixture::alice(), &fixture.origin(), revoke())
            .await
            .unwrap();
        let events = fixture.event_count().await;
        fixture
            .store
            .execute(&Fixture::alice(), &fixture.origin(), revoke())
            .await
            .unwrap();
        assert_eq!(
            fixture.event_count().await,
            events,
            "revoking twice records nothing"
        );
        assert_eq!(
            denial(fixture.enrol(&revoked.token, &fixture.worker_key()).await),
            DenialReason::EnrolmentTokenRevoked,
            "revoked"
        );
        let expired = fixture.issue_token(workspace).await;
        fixture.clock.advance(time::Duration::minutes(30));
        assert_eq!(
            denial(fixture.enrol(&expired.token, &fixture.worker_key()).await),
            DenialReason::EnrolmentTokenExpired,
            "thirty minutes later"
        );
        let unknown =
            abc_core::credential::EnrolmentToken::parse(&format!("abc_enr_{}", "0".repeat(52)))
                .unwrap();
        assert_eq!(
            denial(fixture.enrol(&unknown, &fixture.worker_key()).await),
            DenialReason::EnrolmentTokenUnknown,
            "never issued"
        );
        assert_views_replay(&pool).await;
    }

    #[sqlx::test(migrator = "abc_store::MIGRATOR")]
    async fn tokens_are_listed_with_their_status_and_revoked_only_while_open(pool: PgPool) {
        let (fixture, claimed) = Fixture::claimed(pool.clone()).await;
        let workspace = claimed.workspace;
        let labelled = fixture
            .store
            .execute(
                &Fixture::alice(),
                &fixture.origin(),
                IssueEnrolmentToken {
                    workspace,
                    label: Some(MachineLabel::parse("work laptop").unwrap()),
                    ttl: TokenTtl::try_from(600).unwrap(),
                },
            )
            .await
            .unwrap();
        fixture.clock.advance(time::Duration::seconds(1));
        let redeemed = fixture.issue_token(workspace).await;
        let enrolled = fixture
            .enrol(&redeemed.token, &fixture.worker_key())
            .await
            .unwrap();
        assert_eq!(
            fixture
                .store
                .execute(
                    &Fixture::alice(),
                    &fixture.origin(),
                    RevokeEnrolmentToken {
                        workspace,
                        token: redeemed.id,
                    },
                )
                .await
                .unwrap_err()
                .to_string(),
            RevokeTokenRejection::AlreadyRedeemed.to_string(),
            "a redeemed token is revoked through its worker"
        );
        let listed = |at: &Fixture| {
            let store = at.store.clone();
            async move {
                store
                    .query(&Fixture::alice(), ListEnrolmentTokens { workspace })
                    .await
                    .unwrap()
            }
        };
        let tokens = listed(&fixture).await;
        assert_eq!(tokens.len(), 2, "both tokens");
        assert_eq!(tokens[0].id, redeemed.id, "newest first");
        assert_eq!(
            tokens[0].status,
            TokenStatus::Redeemed {
                worker: enrolled.worker
            },
            "redeemed by the worker it reserved"
        );
        assert_eq!(tokens[1].status, TokenStatus::Open, "the other is open");
        assert_eq!(
            tokens[1].label.as_ref().map(MachineLabel::as_str),
            Some("work laptop"),
            "with its label"
        );
        assert_eq!(tokens[1].expires_at, labelled.expires_at, "and its expiry");
        fixture.clock.advance(time::Duration::minutes(10));
        assert_eq!(
            listed(&fixture).await[1].status,
            TokenStatus::Expired,
            "until it expires"
        );
        assert_eq!(
            query_denial(
                fixture
                    .store
                    .query(
                        &Fixture::tailnet("mallory@example.com"),
                        ListEnrolmentTokens { workspace }
                    )
                    .await
            ),
            DenialReason::NotAMember,
            "strangers see nothing"
        );
        assert_views_replay(&pool).await;
    }

    #[sqlx::test(migrator = "abc_store::MIGRATOR")]
    async fn revocation_is_seen_by_the_very_next_request(pool: PgPool) {
        let (fixture, claimed) = Fixture::claimed(pool.clone()).await;
        let issued = fixture.issue_token(claimed.workspace).await;
        let key = fixture.worker_key();
        let enrolled = fixture.enrol(&issued.token, &key).await.unwrap();
        let me = fixture
            .store
            .query(&Fixture::worker(&key), WorkerSelf)
            .await
            .unwrap();
        assert_eq!(
            me.worker, enrolled.worker,
            "the key authenticates its worker"
        );
        assert_eq!(me.workspace, claimed.workspace, "in its workspace");
        let revoke = || RevokeWorker {
            workspace: claimed.workspace,
            worker: enrolled.worker,
        };
        fixture
            .store
            .execute(&Fixture::alice(), &fixture.origin(), revoke())
            .await
            .unwrap();
        let events = fixture.event_count().await;
        fixture
            .store
            .execute(&Fixture::alice(), &fixture.origin(), revoke())
            .await
            .unwrap();
        assert_eq!(
            fixture.event_count().await,
            events,
            "revoking twice records nothing"
        );
        let start = fixture
            .store
            .execute(
                &Fixture::worker(&key),
                &fixture.origin(),
                StartSession {
                    session: session(1),
                    build: Version::new(0, 1, 0),
                },
            )
            .await;
        assert!(
            matches!(start, Err(ExecuteError::Auth(AuthError::WorkerRevoked))),
            "a revoked key no longer acts"
        );
        assert!(
            matches!(
                fixture
                    .store
                    .query(&Fixture::worker(&key), WorkerSelf)
                    .await,
                Err(QueryError::Auth(AuthError::WorkerRevoked))
            ),
            "nor reads"
        );
        assert!(
            matches!(
                fixture
                    .store
                    .query(&Fixture::worker(&fixture.worker_key()), WorkerSelf)
                    .await,
                Err(QueryError::Auth(AuthError::UnknownWorkerKey))
            ),
            "an unknown key is not a worker"
        );
        let stranger = fixture
            .store
            .execute(
                &Fixture::alice(),
                &fixture.origin(),
                RevokeWorker {
                    workspace: claimed.workspace,
                    worker: WorkerId::from_uuid(uuid::Uuid::from_u128(9)),
                },
            )
            .await;
        assert!(
            matches!(
                stranger,
                Err(ExecuteError::Rejected(RevokeWorkerRejection::NotFound))
            ),
            "an unknown worker is not found"
        );
        assert_views_replay(&pool).await;
    }

    #[sqlx::test(migrator = "abc_store::MIGRATOR")]
    async fn sessions_make_a_worker_online_and_a_superseded_end_changes_nothing(pool: PgPool) {
        let (fixture, claimed) = Fixture::claimed(pool.clone()).await;
        let issued = fixture.issue_token(claimed.workspace).await;
        let key = fixture.worker_key();
        let enrolled = fixture.enrol(&issued.token, &key).await.unwrap();
        let machines = || {
            let store = fixture.store.clone();
            async move {
                store
                    .query(
                        &Fixture::alice(),
                        ListMachines {
                            workspace: claimed.workspace,
                        },
                    )
                    .await
                    .unwrap()
            }
        };
        let before = machines().await;
        assert_eq!(before.machines.len(), 1, "one machine");
        assert!(!before.machines[0].online, "enrolled, not yet connected");
        assert_eq!(before.machines[0].last_seen_at, None, "never seen");
        let start = |n: u128, build: Version| {
            let store = fixture.store.clone();
            let caller = Fixture::worker(&key);
            let origin = fixture.origin();
            async move {
                store
                    .execute(
                        &caller,
                        &origin,
                        StartSession {
                            session: session(n),
                            build,
                        },
                    )
                    .await
                    .unwrap();
            }
        };
        let end = |n: u128| {
            let store = fixture.store.clone();
            let origin = fixture.origin();
            let worker = enrolled.worker;
            async move {
                store
                    .execute(
                        &Caller::Server,
                        &origin,
                        EndSession {
                            worker,
                            session: session(n),
                            reason: SessionEndReason::Closed,
                        },
                    )
                    .await
                    .unwrap();
            }
        };
        start(1, Version::new(0, 2, 0)).await;
        fixture.clock.advance(time::Duration::seconds(5));
        start(2, Version::new(0, 2, 1)).await;
        let events = fixture.event_count().await;
        end(1).await;
        assert_eq!(
            fixture.event_count().await,
            events,
            "the superseded session's end is a no-op"
        );
        let online = machines().await;
        assert!(
            online.machines[0].online,
            "the newer session keeps it online"
        );
        assert_eq!(online.machines[0].build, "0.2.1", "with the newer build");
        let open = fixture
            .store
            .query(&Caller::Server, OpenSessions)
            .await
            .unwrap();
        assert_eq!(open.len(), 1, "one open session");
        assert_eq!(open[0].session, Some(session(2)), "the newer one");
        end(2).await;
        let offline = machines().await;
        assert!(!offline.machines[0].online, "offline once it ends");
        assert_eq!(
            offline.machines[0].last_seen_at,
            Some(fixture.clock.now()),
            "last seen at the end"
        );
        let quiet = fixture.event_count().await;
        end(2).await;
        assert_eq!(
            fixture.event_count().await,
            quiet,
            "ending twice records nothing"
        );
        assert!(
            fixture
                .store
                .query(&Caller::Server, OpenSessions)
                .await
                .unwrap()
                .is_empty(),
            "nothing open"
        );
        assert_views_replay(&pool).await;
    }

    #[sqlx::test(migrator = "abc_store::MIGRATOR")]
    async fn machines_since_returns_exactly_the_cards_changed_after_a_position(pool: PgPool) {
        let (fixture, claimed) = Fixture::claimed(pool.clone()).await;
        let workspace = claimed.workspace;
        let first = fixture.issue_token(workspace).await;
        let a = fixture
            .enrol(&first.token, &fixture.worker_key())
            .await
            .unwrap();
        fixture.clock.advance(time::Duration::seconds(1));
        let second = fixture.issue_token(workspace).await;
        let key = fixture.worker_key();
        let b = fixture.enrol(&second.token, &key).await.unwrap();
        let snapshot = fixture
            .store
            .query(&Fixture::alice(), ListMachines { workspace })
            .await
            .unwrap();
        assert_eq!(
            snapshot
                .machines
                .iter()
                .map(|card| card.worker)
                .collect::<Vec<_>>(),
            [a.worker, b.worker],
            "in enrolment order"
        );
        assert_eq!(
            snapshot.head.get(),
            fixture.event_count().await,
            "the snapshot names its head"
        );
        let since = |after: Position| {
            let store = fixture.store.clone();
            async move {
                store
                    .query(&Fixture::alice(), MachinesSince { workspace, after })
                    .await
                    .unwrap()
            }
        };
        let quiet = since(snapshot.head).await;
        assert!(quiet.machines.is_empty(), "nothing changed since the head");
        assert_eq!(quiet.head, snapshot.head, "and the head is unchanged");
        fixture
            .store
            .execute(
                &Fixture::worker(&key),
                &fixture.origin(),
                StartSession {
                    session: session(1),
                    build: Version::new(0, 1, 0),
                },
            )
            .await
            .unwrap();
        let delta = since(snapshot.head).await;
        assert_eq!(delta.machines.len(), 1, "only the machine that connected");
        assert_eq!(delta.machines[0].worker, b.worker, "b");
        assert!(delta.machines[0].online, "now online");
        assert_eq!(
            delta.machines[0].changed_at, delta.head,
            "changed by the last event"
        );
        assert_eq!(
            since(Position::ORIGIN).await.machines.len(),
            2,
            "everything since the start"
        );
        assert_views_replay(&pool).await;
    }

    #[sqlx::test(migrator = "abc_store::MIGRATOR")]
    async fn who_am_i_answers_every_kind_of_caller(pool: PgPool) {
        let fixture = Fixture::new(pool.clone()).await;
        let who = |caller: Caller| {
            let store = fixture.store.clone();
            async move { store.query(&caller, WhoAmI).await.unwrap() }
        };
        assert_eq!(
            who(Caller::Anonymous).await,
            SessionView::NoIdentity,
            "no identity"
        );
        assert_eq!(
            who(Caller::Server).await,
            SessionView::NoIdentity,
            "the server is no one here"
        );
        assert!(
            matches!(who(Fixture::alice()).await, SessionView::Unclaimed { login } if login.as_str() == "alice@example.com"),
            "a visitor to an unclaimed server may claim it"
        );
        let claimed = fixture.claim(&Fixture::alice()).await.unwrap();
        assert!(
            matches!(
                who(Fixture::tailnet("bob@example.com")).await,
                SessionView::NotAMember { .. }
            ),
            "bob is not a member"
        );
        let SessionView::Member {
            person,
            display_name,
            workspaces,
        } = who(Fixture::alice()).await
        else {
            panic!("alice is a member");
        };
        assert_eq!(person, claimed.owner, "alice is the owner");
        assert_eq!(display_name.as_str(), "Alice", "by her chosen name");
        assert_eq!(workspaces.len(), 1, "one workspace");
        assert_eq!(workspaces[0].id, claimed.workspace, "the first workspace");
        assert_eq!(workspaces[0].name.as_str(), "Home", "named Home");
        let issued = fixture.issue_token(claimed.workspace).await;
        let key = fixture.worker_key();
        fixture.enrol(&issued.token, &key).await.unwrap();
        assert_eq!(
            who(Fixture::worker(&key)).await,
            SessionView::NoIdentity,
            "a worker is not a person"
        );
        assert_views_replay(&pool).await;
    }

    #[sqlx::test(migrator = "abc_store::MIGRATOR")]
    async fn the_server_caller_can_do_only_server_actions(pool: PgPool) {
        let (fixture, claimed) = Fixture::claimed(pool.clone()).await;
        let workspace = claimed.workspace;
        let issue = |caller: Caller| {
            let store = fixture.store.clone();
            let origin = fixture.origin();
            async move {
                store
                    .execute(
                        &caller,
                        &origin,
                        IssueEnrolmentToken {
                            workspace,
                            label: None,
                            ttl: TokenTtl::default(),
                        },
                    )
                    .await
            }
        };
        assert_eq!(
            denial(issue(Caller::Server).await),
            DenialReason::TailnetIdentityRequired,
            "the server cannot issue tokens"
        );
        assert_eq!(
            query_denial(
                fixture
                    .store
                    .query(&Caller::Server, ListMachines { workspace })
                    .await
            ),
            DenialReason::TailnetIdentityRequired,
            "nor read a workspace"
        );
        assert_eq!(
            query_denial(fixture.store.query(&Fixture::alice(), OpenSessions).await),
            DenialReason::ServerOnly,
            "a person cannot read installation-wide sessions"
        );
        let issued = fixture.issue_token(workspace).await;
        let key = fixture.worker_key();
        let enrolled = fixture.enrol(&issued.token, &key).await.unwrap();
        let end = fixture
            .store
            .execute(
                &Fixture::alice(),
                &fixture.origin(),
                EndSession {
                    worker: enrolled.worker,
                    session: session(1),
                    reason: SessionEndReason::Closed,
                },
            )
            .await;
        assert_eq!(
            denial(end),
            DenialReason::ServerOnly,
            "a person cannot record session ends"
        );
        let start = fixture
            .store
            .execute(
                &Caller::Server,
                &fixture.origin(),
                StartSession {
                    session: session(1),
                    build: Version::new(0, 1, 0),
                },
            )
            .await;
        assert_eq!(
            denial(start),
            DenialReason::WorkerOnly,
            "the server cannot start sessions"
        );
        assert_eq!(
            query_denial(fixture.store.query(&Fixture::alice(), WorkerSelf).await),
            DenialReason::WorkerOnly,
            "nor read a worker's self"
        );
        let unknown = fixture
            .store
            .execute(
                &Caller::Server,
                &fixture.origin(),
                EndSession {
                    worker: WorkerId::from_uuid(uuid::Uuid::from_u128(9)),
                    session: session(1),
                    reason: SessionEndReason::Closed,
                },
            )
            .await;
        assert_eq!(
            denial(unknown),
            DenialReason::UnknownWorker,
            "an unknown worker has no session"
        );
        assert_views_replay(&pool).await;
    }
}
