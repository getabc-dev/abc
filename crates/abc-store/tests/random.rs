//! Seeded random sequences of commands from random callers. Whatever happens, a refused command
//! appends nothing, a retried enrolment returns its first answer, positions stay gapless, and the
//! views equal the replay of the log. The seeds are fixed, so every run is the same (P2).

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use abc_core::command::{
        EndSession, Enrolled, IssueEnrolmentToken, RevokeEnrolmentToken, RevokeWorker,
        StartSession, TokenIssued, TokenTtl,
    };
    use abc_core::credential::{EnrolmentToken, WorkerKey};
    use abc_core::event::SessionEndReason;
    use abc_core::id::{FromUuid as _, SessionId, WorkerId, WorkspaceId};
    use abc_core::secret::Secret;
    use abc_store::Caller;
    use abc_store::testing::{Fixture, assert_views_replay};
    use rand::rngs::ChaCha20Rng;
    use rand::{RngExt as _, SeedableRng as _};
    use semver::Version;
    use sqlx::PgPool;

    /// The steps one run takes.
    const STEPS: usize = 80;

    /// Alice most of the time; sometimes a stranger or no one.
    fn caller(rng: &mut ChaCha20Rng) -> Caller {
        match rng.random_range(0..4) {
            0 => Fixture::tailnet("mallory@example.com"),
            1 => Caller::Anonymous,
            _ => Fixture::alice(),
        }
    }

    struct World {
        fixture: Fixture,
        workspace: WorkspaceId,
        tokens: Vec<TokenIssued>,
        keys: Vec<Secret<WorkerKey>>,
        /// (token index, key index) -> the first answer.
        enrolled: BTreeMap<(usize, usize), Enrolled>,
        sessions: u128,
    }

    impl World {
        fn session(n: u128) -> SessionId {
            SessionId::from_uuid(uuid::Uuid::from_u128(n))
        }

        fn some_worker(&self, rng: &mut ChaCha20Rng) -> Option<WorkerId> {
            let index = rng.random_range(0..self.enrolled.len().max(1));
            self.enrolled
                .values()
                .nth(index)
                .map(|enrolled| enrolled.worker)
        }

        /// Takes one random step and reports whether it changed the log.
        async fn step(&mut self, rng: &mut ChaCha20Rng) -> bool {
            match rng.random_range(0..6) {
                0 => self.issue(rng).await,
                1 if !self.tokens.is_empty() => self.revoke_token(rng).await,
                2 if !self.tokens.is_empty() => self.enrol(rng).await,
                3 => match self.some_worker(rng) {
                    Some(worker) => self.revoke_worker(rng, worker).await,
                    None => false,
                },
                4 if !self.keys.is_empty() => self.start(rng).await,
                5 if self.sessions > 0 => match self.some_worker(rng) {
                    Some(worker) => self.end(rng, worker).await,
                    None => false,
                },
                _ => false,
            }
        }

        async fn issue(&mut self, rng: &mut ChaCha20Rng) -> bool {
            let command = IssueEnrolmentToken {
                workspace: self.workspace,
                label: None,
                ttl: TokenTtl::default(),
            };
            let issued = self
                .fixture
                .store
                .execute(&caller(rng), &self.fixture.origin(), command)
                .await;
            issued.map(|issued| self.tokens.push(issued)).is_ok()
        }

        async fn revoke_token(&mut self, rng: &mut ChaCha20Rng) -> bool {
            let token = self.tokens[rng.random_range(0..self.tokens.len())].id;
            let command = RevokeEnrolmentToken {
                workspace: self.workspace,
                token,
            };
            let before = self.fixture.event_count().await;
            let result = self
                .fixture
                .store
                .execute(&caller(rng), &self.fixture.origin(), command)
                .await;
            result.is_ok() && self.fixture.event_count().await > before
        }

        async fn enrol(&mut self, rng: &mut ChaCha20Rng) -> bool {
            let token = rng.random_range(0..self.tokens.len());
            let key = if self.keys.is_empty() || rng.random_bool(0.5) {
                self.keys.push(self.fixture.worker_key());
                self.keys.len().saturating_sub(1)
            } else {
                rng.random_range(0..self.keys.len())
            };
            let text = EnrolmentToken::parse(self.tokens[token].token.expose_secret().as_str());
            let result = self.fixture.enrol(&text.unwrap(), &self.keys[key]).await;
            let Ok(enrolled) = result else {
                return false;
            };
            if let Some(first) = self.enrolled.get(&(token, key)) {
                assert_eq!(
                    &enrolled, first,
                    "a retried enrolment returns its first answer"
                );
                return false;
            }
            self.enrolled.insert((token, key), enrolled);
            true
        }

        async fn revoke_worker(&mut self, rng: &mut ChaCha20Rng, worker: WorkerId) -> bool {
            let command = RevokeWorker {
                workspace: self.workspace,
                worker,
            };
            let before = self.fixture.event_count().await;
            let result = self
                .fixture
                .store
                .execute(&caller(rng), &self.fixture.origin(), command)
                .await;
            result.is_ok() && self.fixture.event_count().await > before
        }

        async fn start(&mut self, rng: &mut ChaCha20Rng) -> bool {
            let key = &self.keys[rng.random_range(0..self.keys.len())];
            self.sessions = self.sessions.saturating_add(1);
            let command = StartSession {
                session: Self::session(self.sessions),
                build: Version::new(0, 1, 0),
            };
            self.fixture
                .store
                .execute(&Fixture::worker(key), &self.fixture.origin(), command)
                .await
                .is_ok()
        }

        async fn end(&mut self, rng: &mut ChaCha20Rng, worker: WorkerId) -> bool {
            let command = EndSession {
                worker,
                session: Self::session(rng.random_range(1..=self.sessions)),
                reason: SessionEndReason::Closed,
            };
            let before = self.fixture.event_count().await;
            self.fixture
                .store
                .execute(&Caller::Server, &self.fixture.origin(), command)
                .await
                .unwrap();
            self.fixture.event_count().await > before
        }
    }

    async fn run(pool: PgPool, seed: u64) {
        let (fixture, claimed) = Fixture::claimed(pool.clone()).await;
        let mut world = World {
            fixture,
            workspace: claimed.workspace,
            tokens: Vec::new(),
            keys: Vec::new(),
            enrolled: BTreeMap::new(),
            sessions: 0,
        };
        let mut rng = ChaCha20Rng::seed_from_u64(seed);
        for _ in 0..STEPS {
            let before = world.fixture.event_count().await;
            let changed = world.step(&mut rng).await;
            let after = world.fixture.event_count().await;
            assert_eq!(
                after > before,
                changed,
                "only a step that succeeded appended events"
            );
            if rng.random_bool(0.2) {
                world.fixture.clock.advance(time::Duration::minutes(2));
            }
        }
        let positions: Vec<i64> =
            sqlx::query_scalar("SELECT position FROM event ORDER BY position")
                .fetch_all(&pool)
                .await
                .unwrap();
        let count = i64::try_from(positions.len()).unwrap();
        assert_eq!(positions, (1..=count).collect::<Vec<_>>(), "gapless");
        assert!(
            !world.enrolled.is_empty(),
            "the run enrolled someone (seed {seed})"
        );
        assert_views_replay(&pool).await;
    }

    #[sqlx::test(migrator = "abc_store::MIGRATOR")]
    async fn random_commands_seed_1(pool: PgPool) {
        run(pool, 1).await;
    }

    #[sqlx::test(migrator = "abc_store::MIGRATOR")]
    async fn random_commands_seed_2(pool: PgPool) {
        run(pool, 2).await;
    }

    #[sqlx::test(migrator = "abc_store::MIGRATOR")]
    async fn random_commands_seed_3(pool: PgPool) {
        run(pool, 3).await;
    }
}
