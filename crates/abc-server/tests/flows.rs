//! End-to-end flows: a real server on a fresh Postgres database each, reached over its real Unix
//! socket by clients that play the Tailscale sidecar.

#[cfg(test)]
mod common;

#[cfg(test)]
mod tests {
    use abc_store::testing::TestDatabase;
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::Duration;

    use abc_core::command::StartSession;
    use abc_core::id::{FromUuid as _, SessionId};
    use abc_protocol::{
        Hello, ProtocolVersion, RejectReason, ServerMessage, VersionRange, WorkerMessage,
    };
    use abc_server::{Limits, LinkTimings, ServeError};
    use abc_store::testing::{Fixture, assert_views_replay};
    use hyper::StatusCode;
    use semver::Version;
    use serde_json::json;

    use crate::common::{Call, HOST, Server, claim_body, config, enrol_body, socket_dir};

    const BAD: StatusCode = StatusCode::BAD_REQUEST;
    const FORBIDDEN: StatusCode = StatusCode::FORBIDDEN;
    const OK: StatusCode = StatusCode::OK;

    #[tokio::test]
    async fn the_owner_claims_with_the_setup_code_once() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let server = Server::start(pool.clone()).await;
        let anonymous = server.expect(Call::get("/api/session"), OK, "anyone").await;
        assert_eq!(
            anonymous["state"], "no_identity",
            "a tagged device has no identity"
        );
        let visitor = server
            .expect(Call::get("/api/session").alice(), OK, "alice")
            .await;
        assert_eq!(visitor["state"], "unclaimed", "alice may claim");
        assert_eq!(visitor["login"], "alice@example.com", "as herself");
        let code = server.setup_code().await;
        let wrong = Call::post("/api/owner/claim")
            .alice()
            .json(&claim_body("00000-00000-00000-00000"));
        server
            .refused(wrong, FORBIDDEN, "setup_code_mismatch", "a wrong code")
            .await;
        let response = server
            .raw(
                Call::post("/api/owner/claim")
                    .alice()
                    .json(&claim_body(&code.to_lowercase())),
            )
            .await;
        assert_eq!(response.status(), OK, "the right code, typed loosely");
        assert!(
            response.headers().contains_key("x-request-id"),
            "responses name their request"
        );
        let again = Call::post("/api/owner/claim")
            .login("bob@example.com")
            .json(&claim_body(&code));
        server
            .refused(
                again,
                StatusCode::CONFLICT,
                "server_already_claimed",
                "a second claim",
            )
            .await;
        let member = server
            .expect(Call::get("/api/session").alice(), OK, "alice")
            .await;
        assert_eq!(member["state"], "member", "{member}");
        assert_eq!(
            member["workspaces"][0]["name"], "Home",
            "with her workspace"
        );
        let bob = server
            .expect(
                Call::get("/api/session").login("bob@example.com"),
                OK,
                "bob",
            )
            .await;
        assert_eq!(bob["state"], "not_a_member", "bob is not");
        server.stop().await.unwrap();
        assert_views_replay(&pool).await;
    }

    #[tokio::test]
    async fn identity_is_refused_from_other_hosts_and_sites() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let server = Server::start(pool.clone()).await;
        let workspace = server.claimed().await;
        let tokens = format!("/api/workspaces/{workspace}/enrolment-tokens");
        let rebound = Call::get("/api/session")
            .alice()
            .header("host", "evil.example");
        server
            .refused(rebound, FORBIDDEN, "cross_site_request", "DNS rebinding")
            .await;
        let shouted = Call::get("/api/session")
            .alice()
            .header("host", "abc.TEST.ts.net");
        server
            .expect(shouted, OK, "the host compares case-insensitively")
            .await;
        let count = Fixture::new(pool.clone()).await.event_count().await;
        let site = Call::post(&tokens)
            .alice()
            .header("sec-fetch-site", "cross-site")
            .json(&json!({}));
        server
            .refused(site, FORBIDDEN, "cross_site_request", "another site")
            .await;
        let origin = Call::post(&tokens)
            .alice()
            .header("origin", "https://evil.example")
            .json(&json!({}));
        server
            .refused(origin, FORBIDDEN, "cross_site_request", "another origin")
            .await;
        assert_eq!(
            Fixture::new(pool.clone()).await.event_count().await,
            count,
            "nothing written"
        );
        let own = Call::post(&tokens)
            .alice()
            .header("sec-fetch-site", "same-origin")
            .header("origin", &format!("https://{HOST}"))
            .json(&json!({}));
        server
            .expect(own, OK, "the web app itself is welcome")
            .await;
        let tagged = Call::post(&tokens).json(&json!({}));
        server
            .refused(
                tagged,
                StatusCode::UNAUTHORIZED,
                "tailnet_identity_required",
                "tagged",
            )
            .await;
        let twice = Call::get(&tokens).alice().login("bob@example.com");
        server
            .refused(twice, BAD, "malformed_request", "two logins")
            .await;
        let bad = Call::get(&tokens).login("alice smith");
        server
            .refused(bad, BAD, "malformed_request", "a malformed login")
            .await;
        let stranger = Call::get(&tokens).login("mallory@example.com");
        server
            .refused(stranger, FORBIDDEN, "not_a_member", "a stranger")
            .await;
        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn malformed_requests_get_the_api_error_format() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let server = Server::start(pool.clone()).await;
        let workspace = server.claimed().await;
        let tokens = format!("/api/workspaces/{workspace}/enrolment-tokens");
        let plain = Call::post(&tokens).alice().body_text("text/plain", "{}");
        server
            .refused(plain, BAD, "malformed_request", "not JSON by content type")
            .await;
        let broken = Call::post(&tokens)
            .alice()
            .body_text("application/json", "{");
        let syntax = server
            .refused(broken, BAD, "malformed_request", "not JSON")
            .await;
        assert_eq!(
            syntax["error"]["message"], "the request body is not valid JSON for this request",
            "a syntax error has no path"
        );
        let short = Call::post(&tokens)
            .alice()
            .json(&json!({ "ttl": 5, "label": "hunter2-label" }));
        let field = server
            .refused(short, BAD, "malformed_request", "a ttl out of range")
            .await;
        assert_eq!(
            field["error"]["message"], "the request body is invalid at `ttl`",
            "names the field"
        );
        let trailing = Call::post(&tokens)
            .alice()
            .body_text("application/json; charset=utf-8", "{} []");
        server
            .refused(trailing, BAD, "malformed_request", "trailing data")
            .await;
        let token = Call::post("/api/enrol").json(&json!({
            "token": "abc_enr_hunter2", "key": "00", "machine": {}, "build": "0.1.0"
        }));
        let secret = server
            .refused(token, BAD, "malformed_request", "a malformed token")
            .await;
        assert!(
            !secret.to_string().contains("hunter2"),
            "input is never echoed: {secret}"
        );
        let path = Call::get("/api/workspaces/not-a-uuid/enrolment-tokens").alice();
        server
            .refused(path, BAD, "malformed_request", "a bad id in the path")
            .await;
        let padded = format!("{{{}\"label\": null}}", " ".repeat(32 * 1024));
        let roomy = Call::post(&tokens)
            .alice()
            .body_text("application/json", &padded);
        server
            .expect(roomy, OK, "a 32 KiB body is within the limit")
            .await;
        let big = Call::post("/api/enrol").body_text("application/json", &"x".repeat(70 * 1024));
        server
            .refused(big, BAD, "malformed_request", "a body over 64 KiB")
            .await;
        let nowhere = Call::get("/api/nowhere");
        server
            .refused(nowhere, StatusCode::NOT_FOUND, "not_found", "no route")
            .await;
        let method = Call::delete("/api/session");
        server
            .refused(
                method,
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "wrong method",
            )
            .await;
        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn the_ops_listener_serves_probes_and_never_identity() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let server = Server::start(pool.clone()).await;
        assert_eq!(
            server.ops_get("/healthz").await,
            (OK, json!({ "status": "alive" })),
            "alive"
        );
        assert_eq!(
            server.ops_get("/readyz").await,
            (OK, json!({ "status": "ready" })),
            "ready"
        );
        assert_eq!(
            server.ops_get("/api/session").await.0,
            StatusCode::NOT_FOUND,
            "no API on the ops listener, identity header or not"
        );
        let ops = server.ops;
        server.stop().await.unwrap();
        assert!(
            tokio::net::TcpStream::connect(ops).await.is_err(),
            "the ops listener stops with the server"
        );
    }

    #[tokio::test]
    async fn enrolment_over_http_survives_a_lost_response_and_refuses_replay() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let server = Server::start(pool.clone()).await;
        let keys = Fixture::new(pool.clone()).await;
        let workspace = server.claimed().await;
        let tokens = format!("/api/workspaces/{workspace}/enrolment-tokens");
        let labelled = Call::post(&tokens)
            .alice()
            .json(&json!({ "label": "work laptop", "ttl": 600 }));
        let issued = server.expect(labelled, OK, "issue").await;
        let token = issued["token"].as_str().unwrap().to_owned();
        assert!(token.starts_with("abc_enr_"), "the token is shown once");
        let listed = server.expect(Call::get(&tokens).alice(), OK, "list").await;
        assert_eq!(listed[0]["status"]["status"], "open", "{listed}");
        assert!(
            !listed.to_string().contains(&token),
            "listing never shows the secret"
        );
        let key = keys.worker_key();
        let enrol = || Call::post("/api/enrol").json(&enrol_body(&token, &key));
        let first = server.expect(enrol(), OK, "enrol").await;
        assert_eq!(first["name"], "work laptop", "named by the label");
        let again = server.expect(enrol().alice(), OK, "a retry").await;
        assert_eq!(
            again, first,
            "a retry gets the same answer, identity ignored"
        );
        let other = Call::post("/api/enrol").json(&enrol_body(&token, &keys.worker_key()));
        server
            .refused(other, FORBIDDEN, "enrolment_token_used", "another key")
            .await;
        let token_id = listed[0]["id"].as_str().unwrap();
        let redeemed = Call::delete(&format!("{tokens}/{token_id}")).alice();
        server
            .refused(
                redeemed,
                StatusCode::CONFLICT,
                "enrolment_token_already_redeemed",
                "revoke the worker instead",
            )
            .await;
        let unknown =
            Call::delete(&format!("{tokens}/00000000-0000-7000-8000-000000000000")).alice();
        server
            .refused(unknown, StatusCode::NOT_FOUND, "not_found", "unknown token")
            .await;
        let (open_id, open_token) = server.token(&workspace).await;
        let revoke = Call::delete(&format!("{tokens}/{open_id}")).alice();
        server
            .expect(revoke, StatusCode::NO_CONTENT, "an open token is revoked")
            .await;
        let late = Call::post("/api/enrol").json(&enrol_body(&open_token, &keys.worker_key()));
        server
            .refused(late, FORBIDDEN, "enrolment_token_revoked", "revoked")
            .await;
        let worker = first["worker"].as_str().unwrap();
        let retire = Call::delete(&format!("/api/workspaces/{workspace}/workers/{worker}")).alice();
        server
            .expect(retire, StatusCode::NO_CONTENT, "the worker is revoked")
            .await;
        server.stop().await.unwrap();
        assert_views_replay(&pool).await;
    }

    #[tokio::test]
    async fn a_connected_worker_shows_online_over_sse_and_a_resume_sends_only_changes() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let server = Server::start(pool.clone()).await;
        let keys = Fixture::new(pool.clone()).await;
        let workspace = server.claimed().await;
        let (_, token) = server.token(&workspace).await;
        let (key, worker) = server.enrol(&token, &keys).await;
        let mut live = server.live(&workspace, None).await;
        let snapshot = live.next().await.unwrap();
        assert_eq!(
            snapshot.name, "snapshot",
            "a fresh stream starts with a snapshot"
        );
        assert_eq!(
            snapshot.data["machines"][0]["worker"],
            worker.as_str(),
            "{snapshot:?}"
        );
        assert_eq!(
            snapshot.data["machines"][0]["online"], false,
            "not connected yet"
        );
        assert_eq!(
            snapshot.id,
            snapshot.data["head"].to_string(),
            "the SSE id is the head"
        );
        let mut link = server.connect(&key).await;
        link.handshake().await;
        let (_, online) = live.machines().await;
        assert_eq!(
            online[0]["online"], true,
            "online once the handshake completes"
        );
        link.socket.close(None).await.unwrap();
        let (last_id, offline) = live.machines().await;
        assert_eq!(offline[0]["online"], false, "offline once it closes");
        assert_eq!(
            server.wait_for_ends(1).await,
            ["closed"],
            "the end is recorded"
        );
        let mut resumed = server.live(&workspace, Some(&snapshot.id)).await;
        let (resumed_id, changed) = resumed.machines().await;
        assert_eq!(resumed_id, last_id, "a resume catches up to the head");
        assert_eq!(changed.len(), 1, "with only the changed card");
        let path = format!("/api/workspaces/{workspace}/live");
        let malformed = Call::get(&path).alice().header("last-event-id", "-1");
        server
            .refused(malformed, BAD, "malformed_request", "a bad Last-Event-ID")
            .await;
        let stranger = Call::get(&path).login("mallory@example.com");
        server
            .refused(
                stranger,
                FORBIDDEN,
                "not_a_member",
                "refused before streaming",
            )
            .await;
        server.stop().await.unwrap();
        assert_views_replay(&pool).await;
    }

    #[tokio::test]
    async fn the_handshake_refuses_other_versions_silence_and_noise() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let timings = LinkTimings {
            handshake: Duration::from_millis(300),
            ..LinkTimings::default()
        };
        let server = Server::start_with(pool.clone(), timings).await;
        let keys = Fixture::new(pool.clone()).await;
        let workspace = server.claimed().await;
        let (_, token) = server.token(&workspace).await;
        let (key, _) = server.enrol(&token, &keys).await;
        let mut old = server.connect(&key).await;
        let hello = WorkerMessage::Hello(Hello {
            protocol: VersionRange::single(ProtocolVersion::new(99)),
            build: Version::new(9, 0, 0),
        });
        old.send_text(&hello.encode().unwrap()).await;
        assert!(
            matches!(
                old.message().await,
                ServerMessage::Reject(reject)
                    if matches!(reject.reason, RejectReason::UnsupportedVersion { .. })
            ),
            "an unsupported version is rejected"
        );
        assert_eq!(old.closed().await, 1000, "and closed normally");
        let mut silent = server.connect(&key).await;
        assert_eq!(silent.closed().await, 4408, "no hello in time");
        let mut noisy = server.connect(&key).await;
        noisy.send_text("{\"type\":\"goodbye\"}").await;
        assert_eq!(noisy.closed().await, 4400, "not a hello");
        let mut binary = server.connect(&key).await;
        let frame = tokio_tungstenite::tungstenite::Message::Binary(vec![1, 2].into());
        futures::SinkExt::send(&mut binary.socket, frame)
            .await
            .unwrap();
        assert_eq!(binary.closed().await, 4400, "binary before hello");
        let mut twice = server.connect(&key).await;
        twice.handshake().await;
        twice.hello().await;
        assert_eq!(twice.closed().await, 4400, "a second hello");
        assert_eq!(
            server.wait_for_ends(1).await,
            ["protocol_error"],
            "only a started session ends in the log"
        );
        server.stop().await.unwrap();
        assert_views_replay(&pool).await;
    }

    #[tokio::test]
    async fn a_reconnect_supersedes_the_old_session_and_stays_online() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let server = Server::start(pool.clone()).await;
        let keys = Fixture::new(pool.clone()).await;
        let workspace = server.claimed().await;
        let (_, token) = server.token(&workspace).await;
        let (key, _) = server.enrol(&token, &keys).await;
        let mut first = server.connect(&key).await;
        first.handshake().await;
        let mut second = server.connect(&key).await;
        second.handshake().await;
        assert_eq!(
            first.closed().await,
            4409,
            "the older session is superseded"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            server.session_ends().await.is_empty(),
            "the superseded session's end records nothing: the newer one is open"
        );
        let mut live = server.live(&workspace, None).await;
        let snapshot = live.next().await.unwrap();
        assert_eq!(
            snapshot.data["machines"][0]["online"], true,
            "the newer session is online"
        );
        server.stop().await.unwrap();
        assert_views_replay(&pool).await;
    }

    #[tokio::test]
    async fn simultaneous_reconnects_leave_exactly_the_attached_session_online() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let server = Server::start(pool.clone()).await;
        let keys = Fixture::new(pool.clone()).await;
        let workspace = server.claimed().await;
        let (_, token) = server.token(&workspace).await;
        let (key, _) = server.enrol(&token, &keys).await;
        let mut links = Vec::new();
        for _ in 0..3 {
            links.push(server.connect(&key).await);
        }
        let handshakes = links.into_iter().map(|mut link| {
            tokio::spawn(async move {
                link.handshake().await;
                link
            })
        });
        let mut connected = Vec::new();
        for handshake in handshakes {
            connected.push(handshake.await.unwrap());
        }
        let mut superseded = 0_u32;
        let mut survivors = Vec::new();
        for mut link in connected {
            let next = tokio::time::timeout(Duration::from_millis(500), link.closed()).await;
            match next {
                Ok(4409) => superseded = superseded.saturating_add(1),
                Ok(code) => panic!("unexpected close {code}"),
                Err(_) => survivors.push(link),
            }
        }
        assert_eq!(superseded, 2, "all but one were superseded");
        assert_eq!(survivors.len(), 1, "one stays");
        let mut live = server.live(&workspace, None).await;
        let snapshot = live.next().await.unwrap();
        assert_eq!(
            snapshot.data["machines"][0]["online"], true,
            "the survivor is online"
        );
        let mut survivor = survivors.pop().unwrap();
        futures::SinkExt::close(&mut survivor.socket).await.unwrap();
        let (_, offline) = live.machines().await;
        assert_eq!(
            offline[0]["online"], false,
            "the survivor's end clears it: the open session was the attached one"
        );
        server.stop().await.unwrap();
        assert_views_replay(&pool).await;
    }

    #[tokio::test]
    async fn a_viewer_who_loses_access_gets_access_ended() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let server = Server::start(pool.clone()).await;
        let keys = Fixture::new(pool.clone()).await;
        let workspace = server.claimed().await;
        let (_, token) = server.token(&workspace).await;
        let mut live = server.live(&workspace, None).await;
        live.next().await.unwrap();
        // No command removes a member yet; take the membership away behind the log to stand in
        // for one. (The views no longer match the log, so this test checks no replay.)
        sqlx::query("DELETE FROM membership")
            .execute(&pool)
            .await
            .unwrap();
        server.enrol(&token, &keys).await;
        let ended = live.next().await.unwrap();
        assert_eq!(ended.name, "access_ended", "{ended:?}");
        assert!(live.next().await.is_none(), "and the stream ends");
        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn revoking_a_worker_closes_its_session_and_blocks_reconnecting() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let server = Server::start(pool.clone()).await;
        let keys = Fixture::new(pool.clone()).await;
        let workspace = server.claimed().await;
        let (_, token) = server.token(&workspace).await;
        let (key, worker) = server.enrol(&token, &keys).await;
        let mut link = server.connect(&key).await;
        link.handshake().await;
        let revoke = Call::delete(&format!("/api/workspaces/{workspace}/workers/{worker}")).alice();
        server
            .expect(revoke, StatusCode::NO_CONTENT, "revoked")
            .await;
        assert_eq!(
            link.closed().await,
            4403,
            "the live session closes as revoked"
        );
        assert_eq!(
            server.wait_for_ends(1).await,
            ["revoked"],
            "its end is recorded"
        );
        let unknown = keys.worker_key();
        for (bearer, code, why) in [
            (
                Some(key.expose_secret().as_str()),
                "worker_revoked",
                "refused before the upgrade",
            ),
            (
                Some(unknown.expose_secret().as_str()),
                "invalid_worker_key",
                "an unknown key",
            ),
            (None, "invalid_worker_key", "no key"),
            (
                Some("abc_wk_hunter2"),
                "invalid_worker_key",
                "a malformed key",
            ),
        ] {
            let (status, body) = server.try_connect(bearer).await.err().unwrap();
            assert_eq!(
                (status, body["error"]["code"].as_str()),
                (401, Some(code)),
                "{why}"
            );
            assert!(!body.to_string().contains("hunter2"), "never echoed");
        }
        server.stop().await.unwrap();
        assert_views_replay(&pool).await;
    }

    #[tokio::test]
    async fn the_connect_route_needs_an_upgrade() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let server = Server::start(pool.clone()).await;
        let keys = Fixture::new(pool.clone()).await;
        let workspace = server.claimed().await;
        let (_, token) = server.token(&workspace).await;
        let (key, _) = server.enrol(&token, &keys).await;
        let plain = Call::get(abc_protocol::link::CONNECT_PATH).header(
            "authorization",
            &format!("Bearer {}", key.expose_secret().as_str()),
        );
        server
            .refused(plain, BAD, "malformed_request", "no upgrade")
            .await;
        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn a_silent_worker_times_out() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let timings = LinkTimings {
            handshake: Duration::from_secs(5),
            ping: Duration::from_millis(100),
            idle: Duration::from_millis(300),
        };
        let server = Server::start_with(pool.clone(), timings).await;
        let keys = Fixture::new(pool.clone()).await;
        let workspace = server.claimed().await;
        let (_, token) = server.token(&workspace).await;
        let (key, _) = server.enrol(&token, &keys).await;
        let mut link = server.connect(&key).await;
        link.handshake().await;
        // Not reading the socket sends no pongs back, like a laptop that went to sleep.
        assert_eq!(
            server.wait_for_ends(1).await,
            ["timed_out"],
            "the session times out"
        );
        drop(link);
        server.stop().await.unwrap();
        assert_views_replay(&pool).await;
    }

    #[tokio::test]
    async fn a_worker_answering_pings_stays_connected() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let timings = LinkTimings {
            handshake: Duration::from_secs(5),
            ping: Duration::from_millis(50),
            idle: Duration::from_millis(400),
        };
        let server = Server::start_with(pool.clone(), timings).await;
        let keys = Fixture::new(pool.clone()).await;
        let workspace = server.claimed().await;
        let (_, token) = server.token(&workspace).await;
        let (key, _) = server.enrol(&token, &keys).await;
        let mut link = server.connect(&key).await;
        link.handshake().await;
        // Reading answers the server's pings.
        let deadline = tokio::time::Instant::now()
            .checked_add(Duration::from_secs(1))
            .unwrap();
        let mut pings = 0_u32;
        while let Ok(frame) =
            tokio::time::timeout_at(deadline, futures::StreamExt::next(&mut link.socket)).await
        {
            let frame = frame.unwrap().unwrap();
            assert!(frame.is_ping(), "nothing but pings arrives: {frame:?}");
            pings = pings.saturating_add(1);
        }
        assert!(pings > 1, "the server pinged throughout");
        assert!(
            server.session_ends().await.is_empty(),
            "connected past the idle window"
        );
        drop(link);
        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn a_restart_ends_the_sessions_the_previous_process_left_open() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let fixture = Fixture::new(pool.clone()).await;
        let claimed = fixture.claim(&Fixture::alice()).await.unwrap();
        let issued = fixture.issue_token(claimed.workspace).await;
        let key = fixture.worker_key();
        fixture.enrol(&issued.token, &key).await.unwrap();
        let start = StartSession {
            session: SessionId::from_uuid(uuid::Uuid::from_u128(1)),
            build: Version::new(0, 1, 0),
        };
        fixture
            .store
            .execute(&Fixture::worker(&key), &fixture.origin(), start)
            .await
            .unwrap();
        let server = Server::start(pool.clone()).await;
        assert_eq!(
            server.session_ends().await,
            ["server_restart"],
            "the boot sweep closed it"
        );
        let actor: String = sqlx::query_scalar(
            "SELECT actor_kind FROM event WHERE event_type = 'worker.session_ended'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(actor, "server", "recorded by the server");
        server.stop().await.unwrap();
        assert_views_replay(&pool).await;
    }

    #[tokio::test]
    async fn shutting_down_closes_sessions_and_streams_and_records_the_ends() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let server = Server::start(pool.clone()).await;
        let keys = Fixture::new(pool.clone()).await;
        let workspace = server.claimed().await;
        let (_, token) = server.token(&workspace).await;
        let (key, _) = server.enrol(&token, &keys).await;
        let mut live = server.live(&workspace, None).await;
        live.next().await.unwrap();
        let mut link = server.connect(&key).await;
        link.handshake().await;
        live.machines().await;
        let closing = tokio::spawn(async move { link.closed().await });
        server.stop().await.unwrap();
        assert_eq!(
            closing.await.unwrap(),
            4503,
            "the worker hears the server is shutting down"
        );
        assert!(live.next().await.is_none(), "the live stream ends");
        let reasons: Vec<String> = sqlx::query_scalar(
            "SELECT payload->>'reason' FROM event WHERE event_type = 'worker.session_ended'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            reasons,
            ["server_shutdown"],
            "recorded before the process exits"
        );
        assert_views_replay(&pool).await;
    }

    #[tokio::test]
    async fn the_server_refuses_a_socket_others_could_reach() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let open = socket_dir();
        std::fs::set_permissions(open.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let settings = config(&pool, open.path().join("abc.sock"), LinkTimings::default());
        let refused = abc_server::run(settings, async {}).await.unwrap_err();
        assert!(
            matches!(refused, ServeError::SocketDirTooOpen { mode: 0o755, .. }),
            "a world-readable directory: {refused:?}"
        );
        assert!(
            refused.to_string().contains("0750 or 0770"),
            "the message says what to do"
        );
        let dir = socket_dir();
        let in_the_way = dir.path().join("abc.sock");
        std::fs::write(&in_the_way, "not a socket").unwrap();
        let blocked = abc_server::run(config(&pool, in_the_way, LinkTimings::default()), async {});
        assert!(
            matches!(blocked.await, Err(ServeError::SocketPath { .. })),
            "a file in the way"
        );
        let nowhere = config(
            &pool,
            dir.path().join("none/abc.sock"),
            LinkTimings::default(),
        );
        assert!(
            matches!(
                abc_server::run(nowhere, async {}).await,
                Err(ServeError::SocketPath { .. })
            ),
            "no directory"
        );
        let stale = dir.path().join("stale.sock");
        drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
        let restarted = abc_server::run(config(&pool, stale, LinkTimings::default()), async {});
        assert!(
            restarted.await.is_ok(),
            "a stale socket from a previous run is replaced"
        );
    }

    #[tokio::test]
    async fn the_server_reports_an_unreachable_database() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let dir = socket_dir();
        let mut settings = config(&pool, dir.path().join("abc.sock"), LinkTimings::default());
        settings.database_url =
            abc_store::DatabaseUrl::new("postgres://abc@127.0.0.1:1/abc".to_owned());
        settings.database_wait = Duration::from_millis(200);
        let result = abc_server::run(settings, async {}).await;
        assert!(
            matches!(
                result,
                Err(ServeError::Store(abc_store::OpenError::Connect(_)))
            ),
            "{result:?}"
        );
    }

    /// Waits until `count` backends of `pool`'s database wait for a lock.
    async fn waiting_for_locks(pool: &sqlx::PgPool, count: i64) {
        for _ in 0..400 {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_stat_activity
                 WHERE datname = current_database() AND wait_event_type = 'Lock'",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            if waiting >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{count} backends never waited for the log head");
    }

    #[tokio::test]
    async fn a_revocation_right_after_a_session_starts_still_closes_it() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let server = Server::start(pool.clone()).await;
        let keys = Fixture::new(pool.clone()).await;
        let workspace = server.claimed().await;
        let (_, token) = server.token(&workspace).await;
        let (key, worker) = server.enrol(&token, &keys).await;
        let mut link = server.connect(&key).await;
        // Hold the log head so the session's start and the revocation queue behind it, in that
        // order: the revocation commits right after the start, before the session's loop runs.
        let mut gate = pool.begin().await.unwrap();
        sqlx::query("SELECT position FROM event_log_head FOR UPDATE")
            .execute(&mut *gate)
            .await
            .unwrap();
        link.hello().await;
        waiting_for_locks(&pool, 1).await;
        let revoke = Call::delete(&format!("/api/workspaces/{workspace}/workers/{worker}")).alice();
        let socket = server.socket.clone();
        let revoking = tokio::spawn(async move { crate::common::send_on(&socket, revoke).await.0 });
        waiting_for_locks(&pool, 2).await;
        gate.commit().await.unwrap();
        assert_eq!(revoking.await.unwrap(), StatusCode::NO_CONTENT, "revoked");
        assert!(
            matches!(link.message().await, ServerMessage::Welcome(_)),
            "the session started"
        );
        let closed = tokio::time::timeout(Duration::from_secs(5), link.closed()).await;
        assert_eq!(
            closed.ok(),
            Some(4403),
            "and closes as revoked, long before its next ping"
        );
        server.stop().await.unwrap();
        assert_views_replay(&pool).await;
    }

    #[tokio::test]
    async fn every_ping_re_checks_revocation_even_without_a_wake_up() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let timings = LinkTimings {
            handshake: Duration::from_secs(5),
            ping: Duration::from_millis(100),
            idle: Duration::from_secs(30),
        };
        let server = Server::start_with(pool.clone(), timings).await;
        let keys = Fixture::new(pool.clone()).await;
        let workspace = server.claimed().await;
        let (_, token) = server.token(&workspace).await;
        let (key, _) = server.enrol(&token, &keys).await;
        let mut link = server.connect(&key).await;
        link.handshake().await;
        // Revoke behind the log's back: no notification, so only the ping's check can notice.
        // (The views no longer match the log, so this test checks no replay.)
        sqlx::query("UPDATE worker SET revoked_at = now()")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            link.closed().await,
            4403,
            "the next ping finds the worker revoked"
        );
        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn a_reused_key_conflicts_and_a_revoked_workers_token_stops_answering() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let server = Server::start(pool.clone()).await;
        let keys = Fixture::new(pool.clone()).await;
        let workspace = server.claimed().await;
        let (_, first) = server.token(&workspace).await;
        let (_, second) = server.token(&workspace).await;
        let (key, worker) = server.enrol(&first, &keys).await;
        let reuse = Call::post("/api/enrol").json(&enrol_body(&second, &key));
        server
            .refused(
                reuse,
                StatusCode::CONFLICT,
                "worker_key_in_use",
                "one key, one worker",
            )
            .await;
        let revoke = Call::delete(&format!("/api/workspaces/{workspace}/workers/{worker}")).alice();
        server
            .expect(revoke, StatusCode::NO_CONTENT, "revoked")
            .await;
        let retry = Call::post("/api/enrol").json(&enrol_body(&first, &key));
        server
            .refused(
                retry,
                StatusCode::UNAUTHORIZED,
                "worker_revoked",
                "no answer for a revoked worker",
            )
            .await;
        server.stop().await.unwrap();
        assert_views_replay(&pool).await;
    }

    #[tokio::test]
    async fn an_issued_token_is_never_cached() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let server = Server::start(pool.clone()).await;
        let workspace = server.claimed().await;
        let issue = Call::post(&format!("/api/workspaces/{workspace}/enrolment-tokens"))
            .alice()
            .json(&json!({}));
        let response = server.raw(issue).await;
        assert_eq!(response.status(), OK, "issued");
        assert_eq!(
            response
                .headers()
                .get("cache-control")
                .map(hyper::header::HeaderValue::as_bytes),
            Some(&b"no-store"[..]),
            "the one response with a secret says no-store"
        );
        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn enrolment_attempts_are_limited_per_source() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let limits = Limits {
            enrolments_per_minute: 2,
            ..Limits::default()
        };
        let server = Server::start_limited(pool.clone(), LinkTimings::default(), limits).await;
        let keys = Fixture::new(pool.clone()).await;
        let token = format!("abc_enr_{}", "0".repeat(52));
        let attempt = |forwarded: &str| {
            Call::post("/api/enrol")
                .header("x-forwarded-for", forwarded)
                .json(&enrol_body(&token, &keys.worker_key()))
        };
        for _ in 0..2 {
            let unknown = attempt("10.0.0.9, 100.64.0.1");
            server
                .refused(
                    unknown,
                    StatusCode::NOT_FOUND,
                    "enrolment_token_unknown",
                    "unknown",
                )
                .await;
        }
        let spoofed = attempt("10.0.0.10, 100.64.0.1");
        server
            .refused(
                spoofed,
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "the address Serve added counts",
            )
            .await;
        let other = attempt("100.64.0.2");
        server
            .refused(
                other,
                StatusCode::NOT_FOUND,
                "enrolment_token_unknown",
                "another machine has its own quota",
            )
            .await;
        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn a_worker_is_limited_in_sockets_and_session_starts() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let limits = Limits {
            sockets_per_worker: 2,
            session_starts_per_minute: 3,
            ..Limits::default()
        };
        let server = Server::start_limited(pool.clone(), LinkTimings::default(), limits).await;
        let keys = Fixture::new(pool.clone()).await;
        let workspace = server.claimed().await;
        let (_, token) = server.token(&workspace).await;
        let (key, _) = server.enrol(&token, &keys).await;
        let bearer = key.expose_secret().as_str().to_owned();
        let first = server.connect(&key).await;
        let second = server.connect(&key).await;
        let (status, body) = server.try_connect(Some(&bearer)).await.err().unwrap();
        assert_eq!(
            (status, body["error"]["code"].as_str()),
            (429, Some("rate_limited")),
            "a third socket"
        );
        drop((first, second));
        let mut third = None;
        for _ in 0..100 {
            if let Ok(link) = server.try_connect(Some(&bearer)).await {
                third = Some(link);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(third.is_some(), "closed sockets free their slots");
        drop(third);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let (fourth, refusal) = server.try_connect(Some(&bearer)).await.err().unwrap();
        assert_eq!(
            (fourth, refusal["error"]["code"].as_str()),
            (429, Some("rate_limited")),
            "a fourth start in a minute is refused"
        );
        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn an_oversized_message_closes_the_link() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let server = Server::start(pool.clone()).await;
        let keys = Fixture::new(pool.clone()).await;
        let workspace = server.claimed().await;
        let (_, token) = server.token(&workspace).await;
        let (key, _) = server.enrol(&token, &keys).await;
        let mut link = server.connect(&key).await;
        link.handshake().await;
        let big = tokio_tungstenite::tungstenite::Message::Text("x".repeat(70 * 1024).into());
        // The server may cut the connection before the whole frame is written.
        let _sent = futures::SinkExt::send(&mut link.socket, big).await;
        assert_ne!(
            link.closed().await,
            4403,
            "the server drops a frame over 64 KiB"
        );
        assert_eq!(
            server.wait_for_ends(1).await,
            ["closed"],
            "and the session ends"
        );
        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn one_server_runs_per_database_and_a_live_socket_is_left_alone() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let server = Server::start(pool.clone()).await;
        let dir = socket_dir();
        let second = abc_server::run(
            config(&pool, dir.path().join("abc.sock"), LinkTimings::default()),
            async {},
        )
        .await;
        assert!(
            matches!(second, Err(ServeError::AlreadyRunning)),
            "{second:?}"
        );
        let other = TestDatabase::migrated().await;
        let live = abc_server::run(
            config(&other.pool(), server.socket.clone(), LinkTimings::default()),
            async {},
        )
        .await;
        assert!(
            matches!(&live, Err(ServeError::SocketPath { reason, .. }) if reason.contains("listening")),
            "another server's socket is not deleted: {live:?}"
        );
        server
            .expect(
                Call::get("/api/session"),
                OK,
                "the first server still serves",
            )
            .await;
        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn a_socket_path_that_cannot_be_inspected_is_reported_as_such() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let dir = socket_dir();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        let result = abc_server::run(
            config(&pool, dir.path().join("abc.sock"), LinkTimings::default()),
            async {},
        )
        .await;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            matches!(result, Err(ServeError::SocketAccess { .. })),
            "permission denied is not \"does not exist\": {result:?}"
        );
    }
}
