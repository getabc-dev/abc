//! The real server and the real worker together: enrolment, the link and its handshake, presence
//! on the Machines screen, reconnecting after a restart, a sleep or a silent link, and every way
//! the server tells a worker to stop.

#[cfg(test)]
mod common;

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use abc_core::credential::EnrolmentToken;
    use abc_core::identity::DisplayName;
    use abc_core::secret::Secret;
    use abc_worker::{EnrolError, IfEnrolled, RunError, Terminal, WallClock as _};

    use crate::common::{LINK, Running, World, home};

    fn token(text: &str) -> Secret<EnrolmentToken> {
        EnrolmentToken::parse(text).unwrap()
    }

    fn machine() -> abc_core::identity::MachineInfo {
        abc_worker::machine(Some("build-box")).unwrap()
    }

    #[tokio::test]
    async fn a_worker_enrols_connects_and_shows_online() {
        let world = World::start().await;
        let (_dir, home) = home();
        let env = world.fast_env();
        let text = world.token("laptop").await;
        let enrolment = abc_worker::enrol(
            &home,
            &env,
            &world.serve.url,
            token(&text),
            machine(),
            IfEnrolled::Refuse,
        )
        .await
        .unwrap();
        assert_eq!(
            enrolment.enrolled.name,
            DisplayName::parse("laptop").unwrap(),
            "the token's label names the worker"
        );
        assert!(!enrolment.again, "a first enrolment");
        let events = world.events().await;
        let again = abc_worker::enrol(
            &home,
            &env,
            &world.serve.url,
            token(&text),
            machine(),
            IfEnrolled::Refuse,
        )
        .await
        .unwrap();
        assert_eq!(again.enrolled, enrolment.enrolled, "the same answer again");
        assert!(again.again, "and it says so");
        assert_eq!(world.events().await, events, "recording nothing new");

        let running = Running::start(home.clone(), env);
        world.until_starts(1).await;
        let worker = enrolment.enrolled.worker.to_string();
        let (actor, build): (String, String) = sqlx::query_as(
            "SELECT actor_id::text, payload->>'build' FROM event
             WHERE event_type = 'worker.session_started'",
        )
        .fetch_one(&world.pool)
        .await
        .unwrap();
        assert_eq!(actor, worker, "the worker itself started the session (I4)");
        assert_eq!(build, "0.1.0", "with the release it sent in hello");

        assert!(
            matches!(home.dir().join("worker.toml").try_exists(), Ok(true)),
            "the key is kept"
        );
        running.stop().await.unwrap();
        assert_eq!(
            world.until_ends(1).await,
            ["closed"],
            "a stopping worker closes its session cleanly"
        );
        let online: bool = sqlx::query_scalar("SELECT open_session IS NOT NULL FROM worker")
            .fetch_one(&world.pool)
            .await
            .unwrap();
        assert!(!online, "and is offline");
    }

    #[tokio::test]
    async fn the_machines_screen_sees_the_worker_come_and_go() {
        let world = World::start().await;
        let (_dir, home) = home();
        let env = world.fast_env();
        let text = world.token("desk").await;
        abc_worker::enrol(
            &home,
            &env,
            &world.serve.url,
            token(&text),
            machine(),
            IfEnrolled::Refuse,
        )
        .await
        .unwrap();
        let running = Running::start(home, env);
        world.until_starts(1).await;
        let online = world.server().snapshot(&world.workspace).await;
        let card = &online["machines"][0];
        assert_eq!(card["online"], true, "online: {online}");
        assert_eq!(card["name"], "desk", "named by the token's label");
        assert_eq!(card["hostname"], "build-box", "with the hostname it gave");
        assert_eq!(
            card["os"],
            abc_worker::machine(None).unwrap().os.as_str(),
            "its OS"
        );
        assert_eq!(
            card["arch"],
            abc_worker::machine(None).unwrap().arch.as_str(),
            "its architecture"
        );
        assert_eq!(card["build"], "0.1.0", "and its release");
        assert_eq!(card["revoked"], false, "in good standing");
        running.stop().await.unwrap();
        world.until_ends(1).await;
        let offline = world.server().snapshot(&world.workspace).await;
        assert_eq!(
            offline["machines"][0]["online"], false,
            "offline: {offline}"
        );
        assert!(
            offline["machines"][0]["last_seen_at"].is_string(),
            "and when it was last seen: {offline}"
        );
    }

    #[tokio::test]
    async fn the_worker_dials_again_after_the_server_restarts() {
        let mut world = World::start().await;
        let (_dir, home) = home();
        let env = world.fast_env();
        let text = world.token("laptop").await;
        abc_worker::enrol(
            &home,
            &env,
            &world.serve.url,
            token(&text),
            machine(),
            IfEnrolled::Refuse,
        )
        .await
        .unwrap();
        let running = Running::start(home, env);
        world.until_starts(1).await;
        world.stop_server().await;
        assert_eq!(
            world.ends().await,
            ["server_shutdown"],
            "the server closed the session as it stopped"
        );
        // While the server is down, Serve answers 502 and the worker keeps backing off.
        tokio::time::sleep(Duration::from_millis(300)).await;
        world.restart_server(LINK).await;
        world.until_starts(2).await;
        running.stop().await.unwrap();
    }

    #[tokio::test]
    async fn after_a_sleep_the_worker_dials_again_at_once() {
        let world = World::start().await;
        let (_dir, home) = home();
        let (env, wall) = world.sleepy_env();
        let text = world.token("laptop").await;
        abc_worker::enrol(
            &home,
            &env,
            &world.serve.url,
            token(&text),
            machine(),
            IfEnrolled::Refuse,
        )
        .await
        .unwrap();
        let running = Running::start(home, env);
        world.until_starts(1).await;
        // The server records the session on `Hello`, a moment before the worker reads `Welcome`
        // and takes its first clock reading; a jump before that reading is not a sleep it can
        // see. So the clock jumps an hour at a time until the worker dials again.
        let mut jumps = 0;
        while world.starts().await < 2 {
            assert!(jumps < 40, "the worker never noticed the sleep");
            let before = wall.now();
            wall.sleep(Duration::from_secs(3600));
            assert!(
                wall.now().duration_since(before).unwrap() >= Duration::from_secs(3600),
                "the wall clock jumped an hour"
            );
            jumps += 1;
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let ends = world.until_ends(1).await;
        assert!(
            matches!(
                ends.first().map(String::as_str),
                Some("closed" | "superseded")
            ),
            "the session from before the sleep ended: {ends:?}"
        );
        running.stop().await.unwrap();
    }

    #[tokio::test]
    async fn a_silent_link_is_dropped_and_dialled_again() {
        // A server that never pings: to the worker, a link that has gone quiet.
        let quiet = abc_server::LinkTimings {
            ping: Duration::from_secs(3600),
            ..LINK
        };
        let world = World::start_with(quiet).await;
        let (_dir, home) = home();
        let env = world.fast_env();
        let text = world.token("laptop").await;
        abc_worker::enrol(
            &home,
            &env,
            &world.serve.url,
            token(&text),
            machine(),
            IfEnrolled::Refuse,
        )
        .await
        .unwrap();
        let running = Running::start(home, env);
        world.until_starts(2).await;
        running.stop().await.unwrap();
    }

    #[tokio::test]
    async fn a_revoked_worker_stops_and_stays_stopped() {
        let world = World::start().await;
        let (_dir, home) = home();
        let env = world.fast_env();
        let text = world.token("laptop").await;
        let enrolled = abc_worker::enrol(
            &home,
            &env,
            &world.serve.url,
            token(&text),
            machine(),
            IfEnrolled::Refuse,
        )
        .await
        .unwrap()
        .enrolled;
        let running = Running::start(home.clone(), env);
        world.until_starts(1).await;
        world.revoke(&enrolled.worker.to_string()).await;
        let stopped = running.finished().await.unwrap_err();
        assert!(
            matches!(stopped, RunError::Terminal(Terminal::Revoked)),
            "revoked while connected: {stopped:?}"
        );
        assert_eq!(stopped.exit_code(), 3, "a person has to act");
        assert_eq!(world.until_ends(1).await, ["revoked"], "the log says why");

        let again = Running::start(home.clone(), world.fast_env())
            .finished()
            .await
            .unwrap_err();
        assert!(
            matches!(again, RunError::Terminal(Terminal::Revoked)),
            "refused before the upgrade: {again:?}"
        );
        let retry = abc_worker::enrol(
            &home,
            &world.fast_env(),
            &world.serve.url,
            token(&text),
            machine(),
            IfEnrolled::Refuse,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(retry, EnrolError::Revoked),
            "enrolling again with the old token says so: {retry:?}"
        );
    }

    #[tokio::test]
    async fn a_copied_home_supersedes_the_older_worker_which_stops() {
        let world = World::start().await;
        let (_dir, home) = home();
        let env = world.fast_env();
        let text = world.token("laptop").await;
        abc_worker::enrol(
            &home,
            &env,
            &world.serve.url,
            token(&text),
            machine(),
            IfEnrolled::Refuse,
        )
        .await
        .unwrap();
        let (_copy_dir, copy) = crate::common::home();
        std::fs::create_dir_all(copy.dir()).unwrap();
        std::fs::set_permissions(
            copy.dir(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        std::fs::copy(
            home.dir().join("worker.toml"),
            copy.dir().join("worker.toml"),
        )
        .unwrap();
        let first = Running::start(home, env);
        world.until_starts(1).await;
        let second = Running::start(copy, world.fast_env());
        let stopped = first.finished().await.unwrap_err();
        assert!(
            matches!(stopped, RunError::Terminal(Terminal::Superseded)),
            "the older worker stops: {stopped:?}"
        );
        assert_eq!(world.starts().await, 2, "the copy holds the newer session");
        assert!(
            world.ends().await.is_empty(),
            "a superseded session's end records nothing: the newer one is open"
        );
        second.stop().await.unwrap();
    }

    #[tokio::test]
    async fn a_key_the_server_does_not_know_stops_the_worker() {
        let world = World::start().await;
        let (_dir, home) = home();
        let env = world.fast_env();
        let text = world.token("laptop").await;
        abc_worker::enrol(
            &home,
            &env,
            &world.serve.url,
            token(&text),
            machine(),
            IfEnrolled::Refuse,
        )
        .await
        .unwrap();
        // Another server's world: the same URL, a fresh database.
        let other = World::start().await;
        let record = std::fs::read_to_string(home.dir().join("worker.toml")).unwrap();
        let moved = record.replace(world.serve.url.origin(), other.serve.url.origin());
        std::fs::write(home.dir().join("worker.toml"), moved).unwrap();
        let stopped = Running::start(home, other.fast_env())
            .finished()
            .await
            .unwrap_err();
        assert!(
            matches!(stopped, RunError::Terminal(Terminal::InvalidKey)),
            "{stopped:?}"
        );
        assert!(
            stopped.to_string().contains("abc worker enrol --replace"),
            "and says how to fix it: {stopped}"
        );
    }

    #[tokio::test]
    async fn the_key_goes_only_to_a_server_whose_certificate_verifies() {
        let world = World::start().await;
        let (_dir, home) = home();
        let env = world.fast_env();
        let text = world.token("laptop").await;
        abc_worker::enrol(
            &home,
            &env,
            &world.serve.url,
            token(&text),
            machine(),
            IfEnrolled::Refuse,
        )
        .await
        .unwrap();
        let requests = world
            .serve
            .requests
            .load(std::sync::atomic::Ordering::SeqCst);
        // A worker that trusts another CA: Serve's certificate does not verify.
        let stranger = crate::common::Pki::new();
        let untrusting = || {
            abc_worker::testing::env(
                abc_worker::testing::trust(stranger.root.clone()).unwrap(),
                abc_worker::testing::fast(),
                Arc::new(abc_worker::SystemClock),
            )
        };
        let running = Running::start(home.clone(), untrusting());
        tokio::time::sleep(Duration::from_millis(500)).await;
        running
            .stop()
            .await
            .expect("it keeps retrying until asked to stop");
        assert_eq!(
            world
                .serve
                .requests
                .load(std::sync::atomic::Ordering::SeqCst),
            requests,
            "no request, and so no key, reached the server"
        );
        assert_eq!(world.starts().await, 0, "and no session started");
        let refused = abc_worker::enrol(
            &crate::common::home().1,
            &untrusting(),
            &world.serve.url,
            token(&world.token("other").await),
            machine(),
            IfEnrolled::Refuse,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                &refused,
                EnrolError::Unreachable(abc_worker::DialError::Tls { .. })
            ),
            "enrolment refuses the certificate too: {refused:?}"
        );
    }

    #[tokio::test]
    async fn enrolment_refusals_say_what_to_do() {
        let world = World::start().await;
        let (_dir, home) = home();
        let env = world.fast_env();
        let text = world.token("laptop").await;
        abc_worker::enrol(
            &home,
            &env,
            &world.serve.url,
            token(&text),
            machine(),
            IfEnrolled::Refuse,
        )
        .await
        .unwrap();
        let other = world.token("other").await;
        let refused = abc_worker::enrol(
            &home,
            &env,
            &world.serve.url,
            token(&other),
            machine(),
            IfEnrolled::Refuse,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&refused, EnrolError::AlreadyEnrolled(name) if name.as_str() == "laptop"),
            "another token for an enrolled home: {refused:?}"
        );
        let replaced = abc_worker::enrol(
            &home,
            &env,
            &world.serve.url,
            token(&other),
            machine(),
            IfEnrolled::Replace,
        )
        .await
        .unwrap();
        assert_eq!(
            replaced.enrolled.name.as_str(),
            "other",
            "replaced by a new worker"
        );

        let (_elsewhere_dir, elsewhere) = crate::common::home();
        let used = abc_worker::enrol(
            &elsewhere,
            &env,
            &world.serve.url,
            token(&other),
            machine(),
            IfEnrolled::Refuse,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(used, EnrolError::TokenUsed),
            "a used token: {used:?}"
        );
        let unknown = abc_worker::enrol(
            &elsewhere,
            &env,
            &world.serve.url,
            token(&format!("abc_enr_{}", "0".repeat(52))),
            machine(),
            IfEnrolled::Refuse,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(unknown, EnrolError::TokenUnknown),
            "an unknown token: {unknown:?}"
        );
    }
}
