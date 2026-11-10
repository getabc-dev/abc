//! Approve-in-browser enrolment over the real socket: a machine asks as a tailnet person from a
//! node the (stand-in) tailscaled names, the owner approves or denies, and the machine learns.

#[cfg(test)]
#[expect(
    dead_code,
    reason = "the shared harness; this file needs only part of it"
)]
mod common;

#[cfg(test)]
mod tests {
    use abc_core::credential::WorkerKey;
    use abc_core::secret::Secret;
    use abc_server::LinkTimings;
    use abc_store::testing::{Fixture, TestDatabase, assert_views_replay};
    use hyper::StatusCode;
    use serde_json::{Value, json};

    use crate::common::{ALICE_MAC, ALICE_OTHER, BOB_LAPTOP, Call, Server, config, socket_dir};

    const OK: StatusCode = StatusCode::OK;
    const NO_CONTENT: StatusCode = StatusCode::NO_CONTENT;

    fn ask_body(key: &Secret<WorkerKey>) -> Value {
        json!({
            "key": key.expose_secret().digest(),
            "machine": { "hostname": "mbp", "os": "macos", "arch": "aarch64" },
            "build": "0.1.0",
        })
    }

    fn requests(workspace: &str) -> String {
        format!("/api/workspaces/{workspace}/enrolment-requests")
    }

    /// Alice's Mac asks to enrol; returns (workspace, key, request id, code).
    async fn asked(server: &Server) -> (String, Secret<WorkerKey>, String, String) {
        let workspace = server.claimed().await;
        let key = Fixture::new(server.pool.clone()).await.worker_key();
        let body = server
            .expect(
                Call::post(&requests(&workspace))
                    .alice()
                    .from_node(ALICE_MAC)
                    .json(&ask_body(&key)),
                OK,
                "alice's Mac asks",
            )
            .await;
        (
            workspace,
            key,
            body["request"].as_str().unwrap().to_owned(),
            body["code"].as_str().unwrap().to_owned(),
        )
    }

    fn status(request: &str) -> String {
        format!("/api/enrolment-requests/{request}")
    }

    #[tokio::test]
    async fn approving_in_the_browser_enrols_the_machine_that_asked() {
        let database = TestDatabase::migrated().await;
        let server = Server::start(database.pool().clone()).await;
        let (workspace, key, request, code) = asked(&server).await;
        assert_eq!(code.len(), 9, "XXXX-XXXX");
        let waiting = server
            .expect(
                Call::get(&status(&request)).alice().from_node(ALICE_MAC),
                OK,
                "the Mac polls",
            )
            .await;
        assert_eq!(waiting["state"], "pending", "waiting");
        assert_eq!(waiting["code"], code, "the same code");
        let card = server
            .expect(
                Call::get(&format!("{}/{request}", requests(&workspace))).alice(),
                OK,
                "the Approve page",
            )
            .await;
        assert_eq!(card["code"], code, "the page shows the terminal's code");
        assert_eq!(card["login"], "alice@example.com", "who asked");
        assert_eq!(card["node"], "alice-mbp.tail1234.ts.net", "from which node");
        assert_eq!(card["hostname"], "mbp", "the machine");
        assert_eq!(card["status"]["status"], "pending", "pending");
        let mut live = server.live(&workspace, None).await;
        let snapshot = live.next().await.unwrap();
        assert_eq!(
            snapshot.data["requests"][0]["id"], request,
            "Machines shows the notice"
        );
        let approve = format!("{}/{request}/approve", requests(&workspace));
        server
            .expect(
                Call::post(&approve).alice().json(&json!({})),
                NO_CONTENT,
                "approve",
            )
            .await;
        let done = server
            .expect(
                Call::get(&status(&request)).alice().from_node(ALICE_MAC),
                OK,
                "polls again",
            )
            .await;
        assert_eq!(done["state"], "approved", "approved");
        assert_eq!(done["workspace"], workspace, "its workspace");
        assert_eq!(done["name"], "mbp", "its name");
        server
            .expect(
                Call::post(&approve).alice().json(&json!({})),
                NO_CONTENT,
                "again",
            )
            .await;
        let mut link = server.connect(&key).await;
        link.hello().await;
        let (_, changed) = loop {
            let event = live.next().await.unwrap();
            if event.name == "machines" && !event.data["changed"].as_array().unwrap().is_empty() {
                break (event.id, event.data["changed"].as_array().unwrap().clone());
            }
        };
        assert_eq!(
            changed[0]["name"], "mbp",
            "the card appears, enrolled with its own key"
        );
        server
            .refused(
                Call::post(&format!("{}/{request}/deny", requests(&workspace)))
                    .alice()
                    .json(&json!({})),
                StatusCode::CONFLICT,
                "enrolment_request_approved",
                "an approved request cannot be denied",
            )
            .await;
        assert_views_replay(&server.pool).await;
        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn a_denied_request_ends_the_machines_wait_and_cannot_be_approved() {
        let database = TestDatabase::migrated().await;
        let server = Server::start(database.pool().clone()).await;
        let (workspace, _key, request, _code) = asked(&server).await;
        let deny = format!("{}/{request}/deny", requests(&workspace));
        server
            .expect(
                Call::post(&deny).alice().json(&json!({})),
                NO_CONTENT,
                "deny",
            )
            .await;
        let state = server
            .expect(
                Call::get(&status(&request)).alice().from_node(ALICE_MAC),
                OK,
                "poll",
            )
            .await;
        assert_eq!(state["state"], "denied", "denied");
        server
            .refused(
                Call::post(&format!("{}/{request}/approve", requests(&workspace)))
                    .alice()
                    .json(&json!({})),
                StatusCode::CONFLICT,
                "enrolment_request_denied",
                "approve after deny",
            )
            .await;
        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn only_the_requesting_login_and_node_learn_where_a_request_stands() {
        let database = TestDatabase::migrated().await;
        let server = Server::start(database.pool().clone()).await;
        let (_workspace, _key, request, _code) = asked(&server).await;
        for (who, call) in [
            (
                "another node of the same person",
                Call::get(&status(&request)).alice().from_node(ALICE_OTHER),
            ),
            (
                "another person",
                Call::get(&status(&request))
                    .login("bob@example.com")
                    .from_node(BOB_LAPTOP),
            ),
            (
                "the right login from an address with another's node",
                Call::get(&status(&request))
                    .login("bob@example.com")
                    .from_node(ALICE_MAC),
            ),
        ] {
            server
                .refused(
                    call,
                    if who.starts_with("the right") {
                        StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        StatusCode::NOT_FOUND
                    },
                    if who.starts_with("the right") {
                        "tailnet_node_unknown"
                    } else {
                        "enrolment_request_unknown"
                    },
                    who,
                )
                .await;
        }
        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn a_request_needs_a_member_on_a_node_the_tailnet_names() {
        let database = TestDatabase::migrated().await;
        let server = Server::start(database.pool().clone()).await;
        let workspace = server.claimed().await;
        let key = Fixture::new(server.pool.clone()).await.worker_key();
        let ask = |call: Call| call.json(&ask_body(&key));
        server
            .refused(
                ask(Call::post(&requests(&workspace))
                    .login("bob@example.com")
                    .from_node(BOB_LAPTOP)),
                StatusCode::FORBIDDEN,
                "not_a_member",
                "bob is not a member",
            )
            .await;
        server
            .refused(
                ask(Call::post(&requests(&workspace))
                    .alice()
                    .from_node("100.64.9.9")),
                StatusCode::SERVICE_UNAVAILABLE,
                "tailnet_node_unknown",
                "an address no node owns",
            )
            .await;
        server
            .refused(
                ask(Call::post(&requests(&workspace)).alice()),
                StatusCode::SERVICE_UNAVAILABLE,
                "tailnet_node_unknown",
                "no forwarded address",
            )
            .await;
        server
            .refused(
                ask(Call::post(&requests(&workspace)).from_node(ALICE_MAC)),
                StatusCode::UNAUTHORIZED,
                "tailnet_identity_required",
                "no tailnet person",
            )
            .await;
        server
            .refused(
                ask(Call::post(&requests(&workspace))
                    .alice()
                    .from_node(ALICE_MAC)
                    .header("origin", "https://evil.example")),
                StatusCode::FORBIDDEN,
                "cross_site_request",
                "another origin",
            )
            .await;
        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn only_an_owner_decides_and_a_repeat_ask_is_the_same_request() {
        let database = TestDatabase::migrated().await;
        let server = Server::start(database.pool().clone()).await;
        let (workspace, key, request, code) = asked(&server).await;
        let again = server
            .expect(
                Call::post(&requests(&workspace))
                    .alice()
                    .from_node(ALICE_MAC)
                    .json(&ask_body(&key)),
                OK,
                "the answer was lost",
            )
            .await;
        assert_eq!(again["request"], request, "the same request");
        assert_eq!(again["code"], code, "the same code");
        let events: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM event WHERE event_type = 'enrolment_request.requested'",
        )
        .fetch_one(&server.pool)
        .await
        .unwrap();
        assert_eq!(events, 1, "recorded once");
        let approve = format!("{}/{request}/approve", requests(&workspace));
        server
            .refused(
                Call::post(&approve)
                    .login("bob@example.com")
                    .json(&json!({})),
                StatusCode::FORBIDDEN,
                "not_a_member",
                "a stranger cannot approve",
            )
            .await;
        server
            .refused(
                Call::post(&approve)
                    .alice()
                    .header("sec-fetch-site", "cross-site")
                    .json(&json!({})),
                StatusCode::FORBIDDEN,
                "cross_site_request",
                "a phishing page cannot approve for the owner",
            )
            .await;
        server
            .refused(
                Call::post(&format!(
                    "{}/00000000-0000-4000-8000-000000000000/approve",
                    requests(&workspace)
                ))
                .alice()
                .json(&json!({})),
                StatusCode::NOT_FOUND,
                "enrolment_request_unknown",
                "no such request",
            )
            .await;
        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn a_server_without_its_tailscaled_socket_asks_for_a_token_instead() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool().clone();
        let dir = socket_dir();
        let socket = dir.path().join("abc.sock");
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let config = config(&pool, socket.clone(), LinkTimings::default());
        let task = tokio::spawn(abc_server::run(config, async move {
            let _stopped = stopped.await;
        }));
        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let (code, body) = crate::common::send_on(
            &socket,
            Call::post(&requests("00000000-0000-4000-8000-000000000000"))
                .alice()
                .from_node(ALICE_MAC)
                .json(&json!({})),
        )
        .await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(body["error"]["code"], "approval_unavailable", "{body}");
        stop.send(()).unwrap();
        task.await.unwrap().unwrap();
    }
}
