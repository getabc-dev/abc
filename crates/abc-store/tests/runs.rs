//! Graph runs through the store: every run event is written with its views in one transaction,
//! read back in order, and replayed into the same views. Real Postgres, no mocks.

#[cfg(test)]
mod tests {
    use abc_core::actor::Actor;
    use abc_core::command::{
        AnswerEscalation, ApplyResult, ExpireLease, GrantLease, ResultFate, RunRejection, StartRun,
        StartSession,
    };
    use abc_core::event::{Event, RunEvent, StreamKind};
    use abc_core::graph::{
        Access, BounceBound, Checker, GateName, GraphBuilder, GraphName, GraphVersion, Step,
        StepId, StepKind, Text,
    };
    use abc_core::id::{FromUuid as _, LeaseId, RunId, SessionId, WorkerId, WorkspaceId};
    use abc_core::policy::DenialReason;
    use abc_core::query::RunEvents;
    use abc_core::run::{Answer, Confidence, Decision, StepResult, Verdict};
    use abc_store::testing::{Fixture, TestDatabase, assert_views_replay};
    use abc_store::{Caller, ExecuteError};
    use semver::Version;

    fn graph() -> GraphVersion {
        let step = |name: &str| Step {
            id: StepId::parse(name).unwrap(),
            kind: StepKind::Agent {
                prompt: Text::parse("build it").unwrap(),
            },
            checker: Checker::Gate {
                check: GateName::parse("tests").unwrap(),
            },
            max_bounces: BounceBound::new(1).unwrap(),
            access: Access::default(),
            back_edge: None,
        };
        GraphBuilder::new(GraphName::parse("build").unwrap())
            .step(step("make"))
            .step(step("ship"))
            .edge(
                StepId::parse("make").unwrap(),
                StepId::parse("ship").unwrap(),
            )
            .build()
            .unwrap()
            .version()
    }

    /// A claimed server with `workers` enrolled workers that each have a session open.
    async fn world(fixture: &Fixture, workspace: WorkspaceId, workers: usize) -> Vec<WorkerId> {
        let mut ids = Vec::new();
        for n in 0..workers {
            let issued = fixture.issue_token(workspace).await;
            let key = fixture.worker_key();
            let enrolled = fixture.enrol(&issued.token, &key).await.unwrap();
            fixture
                .store
                .execute(
                    &Fixture::worker(&key),
                    &fixture.origin(),
                    StartSession {
                        session: SessionId::from_uuid(uuid::Uuid::from_u128(1000 + n as u128)),
                        build: Version::new(0, 1, 0),
                    },
                )
                .await
                .unwrap();
            ids.push(enrolled.worker);
        }
        ids
    }

    async fn start(fixture: &Fixture, workspace: WorkspaceId) -> RunId {
        fixture
            .store
            .execute(
                &Fixture::alice(),
                &fixture.origin(),
                StartRun {
                    workspace,
                    graph: graph(),
                },
            )
            .await
            .unwrap()
    }

    async fn grant(
        fixture: &Fixture,
        run: RunId,
        worker: WorkerId,
    ) -> Result<LeaseId, RunRejection> {
        match fixture
            .store
            .execute(
                &Caller::Server,
                &fixture.origin(),
                GrantLease { run, worker },
            )
            .await
        {
            Ok(lease) => Ok(lease),
            Err(ExecuteError::Rejected(rejection)) => Err(rejection),
            Err(other) => panic!("unexpected: {other:?}"),
        }
    }

    async fn history(fixture: &Fixture, run: RunId) -> Vec<RunEvent> {
        fixture
            .store
            .query(&Caller::Server, RunEvents { run })
            .await
            .unwrap()
            .into_iter()
            .map(|recorded| match recorded.event {
                Event::Run(event) => event,
                other => panic!("{other:?} is not a run event"),
            })
            .collect()
    }

    fn denied<T: std::fmt::Debug, R: std::error::Error>(
        result: Result<T, ExecuteError<R>>,
    ) -> DenialReason {
        match result {
            Err(ExecuteError::Denied(denied)) => denied.reason,
            other => panic!("expected a denial, got {other:?}"),
        }
    }

    fn judged(verdict: Verdict) -> StepResult {
        StepResult::Judged(Decision {
            verdict,
            confidence: Confidence::new(85).unwrap(),
        })
    }

    #[tokio::test]
    async fn a_runs_life_is_written_read_and_replayed() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let (fixture, claimed) = Fixture::claimed(pool.clone()).await;
        let workers = world(&fixture, claimed.workspace, 2).await;
        let run = start(&fixture, claimed.workspace).await;

        let first = grant(&fixture, run, workers[0]).await.unwrap();
        let expire = ExpireLease { run, lease: first };
        fixture
            .store
            .execute(&Caller::Server, &fixture.origin(), expire)
            .await
            .unwrap();
        fixture
            .store
            .execute(&Caller::Server, &fixture.origin(), expire)
            .await
            .unwrap();
        let second = grant(&fixture, run, workers[1]).await.unwrap();
        let apply = |lease, result| ApplyResult { run, lease, result };
        let made = fixture
            .store
            .execute(
                &Caller::Server,
                &fixture.origin(),
                apply(second, StepResult::Made),
            )
            .await
            .unwrap();
        assert_eq!(
            made,
            ResultFate::Applied,
            "a live lease's result is applied"
        );
        let late = fixture
            .store
            .execute(
                &Caller::Server,
                &fixture.origin(),
                apply(first, judged(Verdict::Pass)),
            )
            .await
            .unwrap();
        assert_eq!(
            late,
            ResultFate::Discarded,
            "an expired lease's result is not"
        );
        let third = grant(&fixture, run, workers[0]).await.unwrap();
        let sent_again = fixture
            .store
            .execute(
                &Caller::Server,
                &fixture.origin(),
                apply(second, StepResult::Made),
            )
            .await
            .unwrap();
        assert_eq!(
            sent_again,
            ResultFate::Discarded,
            "a result sent twice is applied once"
        );
        fixture
            .store
            .execute(
                &Caller::Server,
                &fixture.origin(),
                apply(third, judged(Verdict::Escalate)),
            )
            .await
            .unwrap();
        fixture
            .store
            .execute(
                &Fixture::alice(),
                &fixture.origin(),
                AnswerEscalation {
                    workspace: claimed.workspace,
                    run,
                    answer: Answer::Accept,
                },
            )
            .await
            .unwrap();

        assert_eq!(
            history(&fixture, run).await,
            [
                RunEvent::Started { graph: graph() },
                RunEvent::LeaseGranted {
                    lease: first,
                    worker: workers[0]
                },
                RunEvent::LeaseExpired { lease: first },
                RunEvent::LeaseGranted {
                    lease: second,
                    worker: workers[1]
                },
                RunEvent::ResultApplied {
                    lease: second,
                    result: StepResult::Made
                },
                RunEvent::LeaseGranted {
                    lease: third,
                    worker: workers[0]
                },
                RunEvent::ResultApplied {
                    lease: third,
                    result: judged(Verdict::Escalate)
                },
                RunEvent::Answered {
                    answer: Answer::Accept
                },
            ],
            "the run's stream is its history, in order, and decodes to what was written"
        );

        let row = sqlx::query!(
            "SELECT graph, epoch, live_lease, last_answer, started_by FROM run WHERE id = $1",
            run.as_uuid()
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            row.graph,
            graph().as_bytes().to_vec(),
            "the view pins the graph"
        );
        assert_eq!(row.epoch, 2, "two results were applied");
        assert_eq!(row.live_lease, None, "no lease is out");
        assert_eq!(row.last_answer.as_deref(), Some("accept"), "the answer");
        assert_eq!(
            row.started_by,
            claimed.owner.as_uuid(),
            "a person started it (I4)"
        );
        let leases = sqlx::query!(
            "SELECT id, ended, epoch FROM run_lease WHERE run_id = $1 ORDER BY granted_at, id",
            run.as_uuid()
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let ended: Vec<(&str, i32)> = leases
            .iter()
            .map(|lease| (lease.ended.as_deref().unwrap(), lease.epoch))
            .collect();
        assert_eq!(
            ended.iter().filter(|(how, _)| *how == "applied").count(),
            2,
            "two leases ended by their result"
        );
        assert_eq!(
            ended.iter().filter(|(how, _)| *how == "expired").count(),
            1,
            "one by expiry"
        );
        assert_views_replay(&pool).await;
    }

    #[tokio::test]
    async fn events_record_their_actors_and_stream() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let (fixture, claimed) = Fixture::claimed(pool.clone()).await;
        let workers = world(&fixture, claimed.workspace, 1).await;
        let run = start(&fixture, claimed.workspace).await;
        grant(&fixture, run, workers[0]).await.unwrap();
        let events = fixture
            .store
            .query(&Caller::Server, RunEvents { run })
            .await
            .unwrap();
        assert_eq!(events.len(), 2, "started and granted");
        assert_eq!(
            events.iter().map(|e| e.stream_seq).collect::<Vec<_>>(),
            [1, 2],
            "sequenced in the run's stream"
        );
        assert_eq!(
            events[0].actor,
            Actor::Person(claimed.owner),
            "a person started it"
        );
        assert_eq!(events[1].actor, Actor::Server, "the server leased it");
        for event in &events {
            assert_eq!(event.stream.kind, StreamKind::Run, "on a run stream");
            assert_eq!(event.stream.id, run.as_uuid(), "of this run");
            assert_eq!(event.workspace, claimed.workspace, "in its workspace");
        }
        assert_views_replay(&pool).await;
    }

    #[tokio::test]
    async fn a_run_holds_one_live_lease_and_a_worker_works_on_one_step() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let (fixture, claimed) = Fixture::claimed(pool.clone()).await;
        let workers = world(&fixture, claimed.workspace, 2).await;
        let one = start(&fixture, claimed.workspace).await;
        let two = start(&fixture, claimed.workspace).await;
        grant(&fixture, one, workers[0]).await.unwrap();
        assert_eq!(
            grant(&fixture, one, workers[1]).await,
            Err(RunRejection::RunLeased),
            "a second live lease on a run"
        );
        assert_eq!(
            grant(&fixture, two, workers[0]).await,
            Err(RunRejection::WorkerBusy),
            "a second live lease for a worker"
        );
        assert_eq!(
            grant(
                &fixture,
                two,
                WorkerId::from_uuid(uuid::Uuid::from_u128(77))
            )
            .await,
            Err(RunRejection::UnknownWorker),
            "an unknown worker"
        );
        let before = fixture.event_count().await;
        grant(&fixture, two, workers[1]).await.unwrap();
        assert_eq!(
            fixture.event_count().await,
            before + 1,
            "a refusal wrote nothing"
        );

        let both_live = sqlx::query(
            "INSERT INTO run_lease (id, run_id, worker_id, epoch, granted_at, changed_at)
             SELECT gen_random_uuid(), run_id, worker_id, epoch, granted_at, changed_at
             FROM run_lease WHERE run_id = $1",
        )
        .bind(one.as_uuid())
        .execute(&pool)
        .await;
        assert!(
            both_live.is_err(),
            "the database refuses a second live lease too"
        );
        assert_views_replay(&pool).await;
    }

    #[tokio::test]
    async fn an_offline_worker_gets_no_lease() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let (fixture, claimed) = Fixture::claimed(pool.clone()).await;
        let issued = fixture.issue_token(claimed.workspace).await;
        let enrolled = fixture
            .enrol(&issued.token, &fixture.worker_key())
            .await
            .unwrap();
        let run = start(&fixture, claimed.workspace).await;
        assert_eq!(
            grant(&fixture, run, enrolled.worker).await,
            Err(RunRejection::WorkerOffline),
            "no session is open"
        );
        assert_views_replay(&pool).await;
    }

    #[tokio::test]
    async fn only_the_right_principals_record_runs() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let (fixture, claimed) = Fixture::claimed(pool.clone()).await;
        let workers = world(&fixture, claimed.workspace, 1).await;
        let run = start(&fixture, claimed.workspace).await;
        assert_eq!(
            denied(
                fixture
                    .store
                    .execute(
                        &Fixture::alice(),
                        &fixture.origin(),
                        GrantLease {
                            run,
                            worker: workers[0]
                        },
                    )
                    .await
            ),
            DenialReason::ServerOnly,
            "a person cannot grant leases"
        );
        assert_eq!(
            denied(
                fixture
                    .store
                    .execute(
                        &Caller::Server,
                        &fixture.origin(),
                        GrantLease {
                            run: RunId::from_uuid(uuid::Uuid::from_u128(5)),
                            worker: workers[0],
                        },
                    )
                    .await
            ),
            DenialReason::UnknownRun,
            "no such run"
        );
        let stranger = fixture
            .store
            .execute(
                &Fixture::tailnet("mallory@example.com"),
                &fixture.origin(),
                StartRun {
                    workspace: claimed.workspace,
                    graph: graph(),
                },
            )
            .await;
        assert_eq!(
            denied(stranger),
            DenialReason::NotAMember,
            "strangers start nothing"
        );
        let wrong = fixture
            .store
            .execute(
                &Fixture::alice(),
                &fixture.origin(),
                AnswerEscalation {
                    workspace: claimed.workspace,
                    run: RunId::from_uuid(uuid::Uuid::from_u128(5)),
                    answer: Answer::Cancel,
                },
            )
            .await;
        assert!(
            matches!(wrong, Err(ExecuteError::Rejected(RunRejection::UnknownRun))),
            "answering a run that does not exist"
        );
        assert_views_replay(&pool).await;
    }
}
