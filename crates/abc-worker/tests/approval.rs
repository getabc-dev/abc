//! Enrolment by the owner's approval in the browser: the real worker and the real server, with the
//! owner's decision made over the API as the Approve page makes it.

#[cfg(test)]
#[expect(
    dead_code,
    reason = "the shared harness; this file needs only part of it"
)]
mod common;

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use abc_worker::{EnrolError, IfEnrolled, Waiting};
    use serde_json::json;
    use tokio::sync::mpsc;

    use crate::common::{Running, World, home};

    fn machine() -> abc_core::identity::MachineInfo {
        abc_worker::machine(Some("build-box")).unwrap()
    }

    /// The request id in an approve path: `/machines/requests/{workspace}/{request}`.
    fn request_of(waiting: &Waiting) -> String {
        waiting.approve_path.rsplit('/').next().unwrap().to_owned()
    }

    async fn decide(world: &World, request: &str, what: &str) {
        let path = format!(
            "/api/workspaces/{}/enrolment-requests/{request}/{what}",
            world.workspace
        );
        world.server().alice("POST", &path, Some(json!({}))).await;
    }

    #[tokio::test]
    async fn approving_enrols_the_machine_and_it_shows_online() {
        let world = World::start().await;
        let (_dir, home) = home();
        let env = world.fast_env();
        let (shown, mut seen) = mpsc::unbounded_channel();
        let url = world.serve.url.clone();
        let enrolling = {
            let home = home.clone();
            tokio::spawn(async move {
                abc_worker::enrol_by_approval(
                    &home,
                    &env,
                    &url,
                    None,
                    machine(),
                    IfEnrolled::Refuse,
                    |waiting| {
                        shown.send(waiting.clone()).unwrap();
                    },
                )
                .await
            })
        };
        let waiting = seen.recv().await.unwrap();
        assert_eq!(waiting.code.display_form().len(), 9, "XXXX-XXXX");
        assert_eq!(waiting.workspace.as_str(), "Home", "the workspace it joins");
        let request = request_of(&waiting);
        let card = world
            .server()
            .alice(
                "GET",
                &format!(
                    "/api/workspaces/{}/enrolment-requests/{request}",
                    world.workspace
                ),
                None,
            )
            .await;
        assert_eq!(
            card["code"],
            waiting.code.display_form(),
            "the page shows the terminal's code"
        );
        assert_eq!(card["hostname"], "build-box", "and the machine");
        decide(&world, &request, "approve").await;
        let enrolment = enrolling.await.unwrap().unwrap();
        assert_eq!(enrolment.enrolled.name.as_str(), "build-box", "named");
        let record = home.dir().join("worker.toml");
        assert_eq!(
            std::fs::metadata(&record).unwrap().permissions().mode() & 0o777,
            0o600,
            "its credential is private to its user"
        );
        let text = std::fs::read_to_string(&record).unwrap();
        assert!(!text.contains("pending"), "nothing is left pending: {text}");
        let running = Running::start(home, world.fast_env());
        world.until_starts(1).await;
        running.stop().await.unwrap();
    }

    /// Starts setup in the background; the receiver gets what it shows while it waits.
    fn setup(
        world: &World,
        home: &abc_worker::WorkerHome,
        if_enrolled: IfEnrolled,
    ) -> (
        tokio::task::JoinHandle<Result<abc_worker::Enrolment, EnrolError>>,
        mpsc::UnboundedReceiver<Waiting>,
    ) {
        let (shown, seen) = mpsc::unbounded_channel();
        let env = world.fast_env();
        let url = world.serve.url.clone();
        let home = home.clone();
        let task = tokio::spawn(async move {
            abc_worker::enrol_by_approval(&home, &env, &url, None, machine(), if_enrolled, |w| {
                shown.send(w.clone()).unwrap();
            })
            .await
        });
        (task, seen)
    }

    #[tokio::test]
    async fn a_denied_request_ends_setup_and_the_next_run_asks_afresh() {
        let world = World::start().await;
        let (_dir, home) = home();
        let (first, mut first_seen) = setup(&world, &home, IfEnrolled::Refuse);
        let waiting = first_seen.recv().await.unwrap();
        decide(&world, &request_of(&waiting), "deny").await;
        let error = first.await.unwrap().unwrap_err();
        assert!(matches!(error, EnrolError::Denied), "{error}");
        assert!(
            error.to_string().contains("denied"),
            "a clear message: {error}"
        );
        let (second, mut second_seen) = setup(&world, &home, IfEnrolled::Refuse);
        let again = second_seen.recv().await.unwrap();
        assert_ne!(
            request_of(&again),
            request_of(&waiting),
            "a denied request is not asked again"
        );
        second.abort();
    }

    #[tokio::test]
    async fn setup_run_again_while_the_request_waits_is_the_same_request() {
        let world = World::start().await;
        let (_dir, home) = home();
        let (first, mut first_seen) = setup(&world, &home, IfEnrolled::Refuse);
        let waiting = first_seen.recv().await.unwrap();
        first.abort();
        let _gone = first.await;
        let (second, mut second_seen) = setup(&world, &home, IfEnrolled::Refuse);
        let again = second_seen.recv().await.unwrap();
        assert_eq!(again.code, waiting.code, "the same code");
        assert_eq!(request_of(&again), request_of(&waiting), "the same request");
        decide(&world, &request_of(&again), "approve").await;
        second.await.unwrap().unwrap();
        let (third, _seen) = setup(&world, &home, IfEnrolled::Refuse);
        assert!(
            matches!(third.await.unwrap(), Err(EnrolError::AlreadyEnrolled(_))),
            "an enrolled machine does not ask again without --replace"
        );
    }

    #[tokio::test]
    async fn a_machine_with_no_tailnet_person_is_told_how_to_enrol() {
        let world = World::start().await;
        let (_dir, home) = home();
        *world.serve.login.lock().unwrap() = None;
        let (anonymous, _anonymous_seen) = setup(&world, &home, IfEnrolled::Refuse);
        let error = anonymous.await.unwrap().unwrap_err();
        assert!(matches!(error, EnrolError::NoTailnetIdentity), "{error}");
        assert!(
            error.to_string().contains("--token-file"),
            "points at the token flow: {error}"
        );
        *world.serve.login.lock().unwrap() = Some("bob@example.com");
        let (stranger, _stranger_seen) = setup(&world, &home, IfEnrolled::Refuse);
        let stranger_error = stranger.await.unwrap().unwrap_err();
        assert!(
            matches!(&stranger_error, EnrolError::NotAMember(login) if login == "bob@example.com"),
            "{stranger_error}"
        );
    }

    #[tokio::test]
    async fn a_machine_off_the_tailnet_is_told_so_and_how_to_fix_it() {
        let world = World::start().await;
        let (_dir, home) = home();
        let nowhere = abc_protocol::url::PublicUrl::parse("https://localhost:1").unwrap();
        let error = abc_worker::enrol_by_approval(
            &home,
            &world.fast_env(),
            &nowhere,
            None,
            machine(),
            IfEnrolled::Refuse,
            |_| {},
        )
        .await
        .unwrap_err();
        let text = error.to_string();
        assert!(matches!(error, EnrolError::NotOnTailnet { .. }), "{text}");
        for part in ["https://localhost:1", "own Tailscale", "tailscale status"] {
            assert!(text.contains(part), "{part:?} in {text}");
        }
    }

    #[tokio::test]
    async fn a_workspace_that_is_not_the_persons_is_refused_and_the_right_one_is_found_by_name() {
        let world = World::start().await;
        let (_dir, home) = home();
        let url = world.serve.url.clone();
        let error = abc_worker::enrol_by_approval(
            &home,
            &world.fast_env(),
            &url,
            Some("Elsewhere"),
            machine(),
            IfEnrolled::Refuse,
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(matches!(error, EnrolError::NoSuchWorkspace), "{error}");
        let (shown, mut seen) = mpsc::unbounded_channel();
        let env = world.fast_env();
        let named = {
            let home = home.clone();
            tokio::spawn(async move {
                abc_worker::enrol_by_approval(
                    &home,
                    &env,
                    &url,
                    Some("hOmE"),
                    machine(),
                    IfEnrolled::Refuse,
                    |waiting| {
                        shown.send(waiting.clone()).unwrap();
                    },
                )
                .await
            })
        };
        let waiting = seen.recv().await.unwrap();
        assert_eq!(waiting.workspace.as_str(), "Home", "matched without case");
        named.abort();
    }
}
