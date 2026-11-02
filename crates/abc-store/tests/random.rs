//! Seeded random sequences of commands from random callers, checked against a model. The model
//! keeps every token's and worker's state, predicts each step's exact outcome (success with how
//! many events, or the specific denial, rejection or authentication error) and the test asserts
//! it. A retried enrolment must return its first answer, positions must stay gapless, and the
//! views must equal the replay of the log. The seeds are fixed, so every run is the same (P2).

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use abc_core::command::{
        EndSession, Enrolled, IssueEnrolmentToken, RevokeEnrolmentToken, RevokeTokenRejection,
        RevokeWorker, StartSession, TokenIssued, TokenTtl,
    };
    use abc_core::credential::{EnrolmentToken, WorkerKey};
    use abc_core::event::SessionEndReason;
    use abc_core::id::{FromUuid as _, SessionId, WorkerId, WorkspaceId};
    use abc_core::policy::DenialReason;
    use abc_core::secret::Secret;
    use abc_core::time::{Clock as _, Timestamp};
    use abc_store::testing::{Fixture, TestDatabase, assert_views_replay};
    use abc_store::{AuthError, Caller, ExecuteError, QueryError};
    use rand::rngs::ChaCha20Rng;
    use rand::{RngExt as _, SeedableRng as _};
    use semver::Version;

    /// The steps one run takes.
    const STEPS: usize = 120;

    /// Who acts in a step.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Who {
        Owner,
        Stranger,
        Nobody,
    }

    impl Who {
        fn pick(rng: &mut ChaCha20Rng) -> Self {
            match rng.random_range(0..5) {
                0 => Self::Stranger,
                1 => Self::Nobody,
                _ => Self::Owner,
            }
        }

        fn caller(self) -> Caller {
            match self {
                Self::Owner => Fixture::alice(),
                Self::Stranger => Fixture::tailnet("mallory@example.com"),
                Self::Nobody => Caller::Anonymous,
            }
        }

        /// The denial an owner-only command gets from this caller, if any.
        fn owner_denial(self) -> Option<DenialReason> {
            match self {
                Self::Owner => None,
                Self::Stranger => Some(DenialReason::NotAMember),
                Self::Nobody => Some(DenialReason::TailnetIdentityRequired),
            }
        }
    }

    /// What a step must produce.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Expect {
        /// Success, appending this many events.
        Ok(i64),
        Denied(DenialReason),
        Rejected(String),
        Auth(AuthError),
    }

    /// What a step produced, in the same terms.
    fn observed<T, R: std::error::Error>(
        result: &Result<T, ExecuteError<R>>,
        events: i64,
    ) -> Expect {
        match result {
            Ok(_) => Expect::Ok(events),
            Err(ExecuteError::Denied(denied)) => Expect::Denied(denied.reason),
            Err(ExecuteError::Rejected(rejection)) => Expect::Rejected(rejection.to_string()),
            Err(ExecuteError::Auth(error)) => Expect::Auth(*error),
            Err(ExecuteError::Store(error)) => panic!("the store failed: {error}"),
        }
    }

    struct TokenModel {
        issued: TokenIssued,
        revoked: bool,
        /// The key that redeemed it.
        redeemed_by: Option<usize>,
    }

    struct WorkerModel {
        id: WorkerId,
        revoked: bool,
        open_session: Option<u128>,
    }

    struct World {
        fixture: Fixture,
        workspace: WorkspaceId,
        tokens: Vec<TokenModel>,
        keys: Vec<Secret<WorkerKey>>,
        /// The worker each key enrolled, by key.
        workers: BTreeMap<usize, WorkerModel>,
        /// The first answer to each successful (token, key) enrolment.
        answers: BTreeMap<(usize, usize), Enrolled>,
        sessions: u128,
    }

    impl World {
        fn now(&self) -> Timestamp {
            self.fixture.clock.now()
        }

        /// Takes one random step and checks it against the model.
        async fn step(&mut self, rng: &mut ChaCha20Rng) {
            let before = self.fixture.event_count().await;
            let (expected, result) = match rng.random_range(0..7) {
                0 => self.issue(rng).await,
                1 if !self.tokens.is_empty() => self.revoke_token(rng).await,
                2 if !self.tokens.is_empty() => self.enrol(rng).await,
                3 => self.enrol_unknown().await,
                4 if !self.workers.is_empty() => self.revoke_worker(rng).await,
                5 if !self.keys.is_empty() => self.start(rng).await,
                6 if !self.workers.is_empty() => self.end(rng).await,
                _ => return,
            };
            let events = self.fixture.event_count().await.saturating_sub(before);
            let got = if let Expect::Ok(_) = result {
                Expect::Ok(events)
            } else {
                assert_eq!(events, 0, "a refused step appends nothing: {result:?}");
                result
            };
            assert_eq!(got, expected, "the model predicts every outcome");
        }

        async fn issue(&mut self, rng: &mut ChaCha20Rng) -> (Expect, Expect) {
            let who = Who::pick(rng);
            let command = IssueEnrolmentToken {
                workspace: self.workspace,
                label: None,
                ttl: TokenTtl::default(),
            };
            let result = self
                .fixture
                .store
                .execute(&who.caller(), &self.fixture.origin(), command)
                .await;
            let got = observed(&result, 1);
            if let Ok(issued) = result {
                self.tokens.push(TokenModel {
                    issued,
                    revoked: false,
                    redeemed_by: None,
                });
            }
            (
                who.owner_denial().map_or(Expect::Ok(1), Expect::Denied),
                got,
            )
        }

        async fn revoke_token(&mut self, rng: &mut ChaCha20Rng) -> (Expect, Expect) {
            let who = Who::pick(rng);
            let index = rng.random_range(0..self.tokens.len());
            let token = &self.tokens[index];
            let expected = match (who.owner_denial(), token.redeemed_by, token.revoked) {
                (Some(reason), _, _) => Expect::Denied(reason),
                (None, Some(_), _) => {
                    Expect::Rejected(RevokeTokenRejection::AlreadyRedeemed.to_string())
                }
                (None, None, true) => Expect::Ok(0),
                (None, None, false) => Expect::Ok(1),
            };
            let command = RevokeEnrolmentToken {
                workspace: self.workspace,
                token: token.issued.id,
            };
            let result = self
                .fixture
                .store
                .execute(&who.caller(), &self.fixture.origin(), command)
                .await;
            if result.is_ok() {
                self.tokens[index].revoked = true;
            }
            (expected, observed(&result, 0))
        }

        /// The outcome the model predicts for redeeming token `index` with key `key`.
        fn predict_enrol(&self, index: usize, key: usize) -> Expect {
            let token = &self.tokens[index];
            match token.redeemed_by {
                Some(holder) if holder == key => {
                    if self.workers.get(&key).is_some_and(|worker| worker.revoked) {
                        Expect::Denied(DenialReason::WorkerRevoked)
                    } else {
                        Expect::Ok(0)
                    }
                }
                Some(_) => Expect::Denied(DenialReason::EnrolmentTokenUsed),
                None if token.revoked => Expect::Denied(DenialReason::EnrolmentTokenRevoked),
                None if self.now() >= token.issued.expires_at => {
                    Expect::Denied(DenialReason::EnrolmentTokenExpired)
                }
                None if self.workers.contains_key(&key) => {
                    Expect::Denied(DenialReason::WorkerKeyInUse)
                }
                None => Expect::Ok(2),
            }
        }

        async fn enrol(&mut self, rng: &mut ChaCha20Rng) -> (Expect, Expect) {
            let index = rng.random_range(0..self.tokens.len());
            let key = if self.keys.is_empty() || rng.random_bool(0.4) {
                self.keys.push(self.fixture.worker_key());
                self.keys.len().saturating_sub(1)
            } else {
                rng.random_range(0..self.keys.len())
            };
            let expected = self.predict_enrol(index, key);
            let text = self.tokens[index]
                .issued
                .token
                .expose_secret()
                .as_str()
                .to_owned();
            let result = self
                .fixture
                .enrol(&EnrolmentToken::parse(&text).unwrap(), &self.keys[key])
                .await;
            if let Ok(enrolled) = &result {
                if let Some(first) = self.answers.get(&(index, key)) {
                    assert_eq!(enrolled, first, "a retry returns its first answer");
                } else {
                    self.answers.insert((index, key), enrolled.clone());
                    self.tokens[index].redeemed_by = Some(key);
                    self.workers.insert(
                        key,
                        WorkerModel {
                            id: enrolled.worker,
                            revoked: false,
                            open_session: None,
                        },
                    );
                }
            }
            (expected, observed(&result, 0))
        }

        async fn enrol_unknown(&mut self) -> (Expect, Expect) {
            let unknown = EnrolmentToken::parse(&format!("abc_enr_{}", "0".repeat(52))).unwrap();
            let result = self
                .fixture
                .enrol(&unknown, &self.fixture.worker_key())
                .await;
            (
                Expect::Denied(DenialReason::EnrolmentTokenUnknown),
                observed(&result, 0),
            )
        }

        fn some_worker(&self, rng: &mut ChaCha20Rng) -> usize {
            let keys: Vec<usize> = self.workers.keys().copied().collect();
            keys[rng.random_range(0..keys.len())]
        }

        async fn revoke_worker(&mut self, rng: &mut ChaCha20Rng) -> (Expect, Expect) {
            let who = Who::pick(rng);
            let key = self.some_worker(rng);
            let worker = &self.workers[&key];
            let expected = match (who.owner_denial(), worker.revoked) {
                (Some(reason), _) => Expect::Denied(reason),
                (None, true) => Expect::Ok(0),
                (None, false) => Expect::Ok(1),
            };
            let command = RevokeWorker {
                workspace: self.workspace,
                worker: worker.id,
            };
            let result = self
                .fixture
                .store
                .execute(&who.caller(), &self.fixture.origin(), command)
                .await;
            if result.is_ok()
                && let Some(model) = self.workers.get_mut(&key)
            {
                model.revoked = true;
            }
            (expected, observed(&result, 0))
        }

        async fn start(&mut self, rng: &mut ChaCha20Rng) -> (Expect, Expect) {
            let key = rng.random_range(0..self.keys.len());
            self.sessions = self.sessions.saturating_add(1);
            let expected = match self.workers.get(&key) {
                None => Expect::Auth(AuthError::UnknownWorkerKey),
                Some(worker) if worker.revoked => Expect::Auth(AuthError::WorkerRevoked),
                Some(_) => Expect::Ok(1),
            };
            let command = StartSession {
                session: SessionId::from_uuid(uuid::Uuid::from_u128(self.sessions)),
                build: Version::new(0, 1, 0),
            };
            let result = self
                .fixture
                .store
                .execute(
                    &Fixture::worker(&self.keys[key]),
                    &self.fixture.origin(),
                    command,
                )
                .await;
            if result.is_ok()
                && let Some(worker) = self.workers.get_mut(&key)
            {
                worker.open_session = Some(self.sessions);
            }
            (expected, observed(&result, 0))
        }

        async fn end(&mut self, rng: &mut ChaCha20Rng) -> (Expect, Expect) {
            let key = self.some_worker(rng);
            let session = rng.random_range(0..=self.sessions);
            let worker = &self.workers[&key];
            let expected = if worker.open_session == Some(session) {
                Expect::Ok(1)
            } else {
                Expect::Ok(0)
            };
            let command = EndSession {
                worker: worker.id,
                session: SessionId::from_uuid(uuid::Uuid::from_u128(session)),
                reason: SessionEndReason::Closed,
            };
            let result = self
                .fixture
                .store
                .execute(&Caller::Server, &self.fixture.origin(), command)
                .await;
            if expected == Expect::Ok(1)
                && let Some(model) = self.workers.get_mut(&key)
            {
                model.open_session = None;
            }
            (expected, observed(&result, 0))
        }

        /// The model's workers agree with what each key reads about itself.
        async fn check_workers(&self) {
            for (key, worker) in &self.workers {
                let read = self
                    .fixture
                    .store
                    .query(
                        &Fixture::worker(&self.keys[*key]),
                        abc_core::query::WorkerSelf,
                    )
                    .await;
                match read {
                    Ok(me) => {
                        assert!(!worker.revoked, "a live key belongs to a live worker");
                        assert_eq!(me.worker, worker.id, "the key names its worker");
                    }
                    Err(QueryError::Auth(AuthError::WorkerRevoked)) => {
                        assert!(worker.revoked, "only a revoked worker is refused");
                    }
                    Err(other) => panic!("unexpected: {other:?}"),
                }
            }
        }
    }

    async fn run(seed: u64) {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let (fixture, claimed) = Fixture::claimed(pool.clone()).await;
        let mut world = World {
            fixture,
            workspace: claimed.workspace,
            tokens: Vec::new(),
            keys: Vec::new(),
            workers: BTreeMap::new(),
            answers: BTreeMap::new(),
            sessions: 0,
        };
        let mut rng = ChaCha20Rng::seed_from_u64(seed);
        for _ in 0..STEPS {
            world.step(&mut rng).await;
            if rng.random_bool(0.1) {
                world.fixture.clock.advance(time::Duration::minutes(4));
            }
        }
        world.check_workers().await;
        let positions: Vec<i64> =
            sqlx::query_scalar("SELECT position FROM event ORDER BY position")
                .fetch_all(&pool)
                .await
                .unwrap();
        let count = i64::try_from(positions.len()).unwrap();
        assert_eq!(positions, (1..=count).collect::<Vec<_>>(), "gapless");
        assert!(
            !world.workers.is_empty(),
            "the run enrolled someone (seed {seed})"
        );
        assert_views_replay(&pool).await;
    }

    #[tokio::test]
    async fn random_commands_seed_1() {
        run(1).await;
    }

    #[tokio::test]
    async fn random_commands_seed_2() {
        run(2).await;
    }

    #[tokio::test]
    async fn random_commands_seed_3() {
        run(3).await;
    }
}
