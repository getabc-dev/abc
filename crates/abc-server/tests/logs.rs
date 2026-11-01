//! No secret reaches the log (D12, I2): every flow, failures included, runs under the server's
//! own JSON subscriber at TRACE, and the captured text is searched for each plaintext. It has its
//! own test binary because it installs the global subscriber.

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

    use abc_server::LinkTimings;
    use abc_store::testing::Fixture;
    use hyper::StatusCode;
    use serde_json::json;
    use sqlx::PgPool;
    use tracing_subscriber::EnvFilter;

    use crate::common::{Call, DATABASE_PASSWORD, Server, claim_body, enrol_body};

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

    #[sqlx::test(migrator = "abc_store::MIGRATOR")]
    async fn no_secret_reaches_the_log(pool: PgPool) {
        let capture = Capture::default();
        let writer = capture.clone();
        tracing::subscriber::set_global_default(abc_server::telemetry::subscriber(
            EnvFilter::new("trace"),
            move || writer.clone(),
        ))
        .unwrap();
        let server = Server::start_with(pool.clone(), LinkTimings::default()).await;
        let keys = Fixture::new(pool.clone()).await;
        let code = server.setup_code().await;
        let wrong = "ZZZZZ-ZZZZZ-ZZZZZ-ZZZZZ";
        let mismatch = Call::post("/api/owner/claim")
            .alice()
            .json(&claim_body(wrong));
        server
            .expect(mismatch, StatusCode::FORBIDDEN, "a wrong code")
            .await;
        let claim = Call::post("/api/owner/claim")
            .alice()
            .json(&claim_body(&code));
        let claimed = server.expect(claim, StatusCode::OK, "the right code").await;
        let workspace = claimed["workspace"].as_str().unwrap();
        let (_, token) = server.token(workspace).await;
        let key = keys.worker_key();
        let enrol = || Call::post("/api/enrol").json(&enrol_body(&token, &key));
        let enrolled = server.expect(enrol(), StatusCode::OK, "enrol").await;
        server.expect(enrol(), StatusCode::OK, "a retry").await;
        let stranger = keys.worker_key();
        let replay = Call::post("/api/enrol").json(&enrol_body(&token, &stranger));
        server
            .expect(replay, StatusCode::FORBIDDEN, "another key")
            .await;
        let garbled = format!("{token}x");
        let malformed = Call::post("/api/enrol").json(&enrol_body(&garbled, &key));
        server
            .expect(malformed, StatusCode::BAD_REQUEST, "a malformed token")
            .await;
        let mut link = server.connect(&key).await;
        link.handshake().await;
        let unknown = server
            .try_connect(Some(stranger.expose_secret().as_str()))
            .await;
        assert!(unknown.is_err(), "an unknown key is refused");
        let bearer = format!("{}x", key.expose_secret().as_str());
        assert!(
            server.try_connect(Some(&bearer)).await.is_err(),
            "a malformed key is refused"
        );
        let worker = enrolled["worker"].as_str().unwrap();
        let revoke = Call::delete(&format!("/api/workspaces/{workspace}/workers/{worker}")).alice();
        server
            .expect(revoke, StatusCode::NO_CONTENT, "revoke")
            .await;
        assert_eq!(link.closed().await, 4403, "the session closes");
        server.wait_for_ends(1).await;
        let late = Call::post(&format!("/api/workspaces/{workspace}/enrolment-tokens"))
            .alice()
            .json(&json!({ "ttl": 1 }));
        server
            .expect(late, StatusCode::BAD_REQUEST, "a bad request")
            .await;
        server.stop().await.unwrap();

        let log = capture.text();
        assert!(log.contains("request finished"), "requests were logged");
        assert!(log.contains("worker connected"), "sessions were logged");
        let normalized = code.replace('-', "");
        for (secret, what) in [
            (code.as_str(), "the setup code"),
            (normalized.as_str(), "the setup code without dashes"),
            (wrong, "the wrong setup code"),
            (token.as_str(), "the enrolment token"),
            (key.expose_secret().as_str(), "the worker key"),
            (stranger.expose_secret().as_str(), "the unknown worker key"),
            (DATABASE_PASSWORD, "the database password"),
        ] {
            assert!(!log.contains(secret), "{what} reached the log");
        }
        let body = token.strip_prefix("abc_enr_").unwrap();
        assert!(
            !log.contains(body),
            "no part of the token body reached the log"
        );
    }
}
