//! No worker secret reaches the log (I2), through the subscriber `abc worker` installs, at
//! `trace`, across enrolment, a refused enrolment, a link and a revocation. In a test
//! binary of its own because the subscriber is process-wide.

#[cfg(test)]
#[expect(
    dead_code,
    reason = "the shared harness; this file needs only part of it"
)]
mod common;

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex, PoisonError};

    use abc_core::credential::EnrolmentToken;
    use abc_worker::{IfEnrolled, RunError, Terminal};
    use tracing_subscriber::EnvFilter;

    use crate::common::{Running, World, home};

    /// Everything the subscriber writes.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Capture {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap_or_else(PoisonError::into_inner))
                .into_owned()
        }
    }

    #[tokio::test]
    async fn no_worker_secret_reaches_the_log() {
        let capture = Capture::default();
        let writer = capture.clone();
        tracing::subscriber::set_global_default(abc_platform::telemetry::subscriber(
            EnvFilter::new("trace"),
            move || writer.clone(),
        ))
        .unwrap();
        let world = World::start().await;
        let (_dir, home) = home();
        let env = world.fast_env();
        let text = world.token("laptop").await;
        let used_elsewhere = world.token("other").await;
        let enrolled = abc_worker::enrol(
            &home,
            &env,
            &world.serve.url,
            EnrolmentToken::parse(&text).unwrap(),
            abc_worker::machine(Some("mbp")).unwrap(),
            IfEnrolled::Refuse,
        )
        .await
        .unwrap()
        .enrolled;
        let refused = abc_worker::enrol(
            &home,
            &env,
            &world.serve.url,
            EnrolmentToken::parse(&used_elsewhere).unwrap(),
            abc_worker::machine(Some("mbp")).unwrap(),
            IfEnrolled::Refuse,
        )
        .await;
        assert!(refused.is_err(), "already enrolled");
        let record = std::fs::read_to_string(home.dir().join("worker.toml")).unwrap();
        let key = record
            .lines()
            .find_map(|line| line.strip_prefix("key = \""))
            .and_then(|rest| rest.strip_suffix('"'))
            .unwrap()
            .to_owned();
        assert!(key.starts_with("abc_wk_"), "the key is in the record");

        let running = Running::start(home.clone(), env);
        world.until_starts(1).await;
        world.revoke(&enrolled.worker.to_string()).await;
        let stopped = running.finished().await.unwrap_err();
        assert!(
            matches!(stopped, RunError::Terminal(Terminal::Revoked)),
            "{stopped:?}"
        );

        let logged = capture.text();
        assert!(
            logged.contains("\"message\":\"connected\""),
            "the worker's own log lines are captured"
        );
        assert!(
            logged.contains(&enrolled.worker.to_string()),
            "and name the worker"
        );
        let ended = logged
            .lines()
            .find(|line| line.contains("\"message\":\"the link ended\""))
            .unwrap_or_else(|| panic!("how the link ended is logged: {logged}"));
        assert!(
            ended.contains("Revoked") && ended.contains("\"lived_ms\":"),
            "with how and after how long: {ended}"
        );
        for secret in [text.as_str(), used_elsewhere.as_str(), key.as_str()] {
            assert!(!logged.contains(secret), "a secret reached the log");
        }
        let body = key.strip_prefix("abc_wk_").unwrap();
        assert!(!logged.contains(body), "nor the key without its prefix");
    }
}
