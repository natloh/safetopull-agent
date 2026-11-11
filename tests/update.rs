mod support;

use serde_json::json;
use stp_agent::update::UpdateError;
use support::Lab;

const APPLY: [(&str, &str); 1] = [("STP_UPDATE_MODE", "apply")];

fn position(calls: &[String], call: &str) -> usize {
    calls
        .iter()
        .position(|made| made == call)
        .unwrap_or_else(|| panic!("`{call}` was never called: {calls:?}"))
}

#[tokio::test]
async fn pulls_verifies_and_swaps_a_rolling_tag_container() {
    let lab = Lab::new();
    lab.run_nextcloud_29();
    lab.publish_nextcloud_latest("sha256:nextcloud30index");
    let (app, _) = lab.app(&APPLY).await;

    assert_eq!(app.update("cloud/nextcloud", true, None).await.unwrap(), "v30.0.0");

    let calls = lab.docker.calls();
    let order = [
        "tag sha256:nc29 docker.io/library/nextcloud:latest-stp-previous",
        "pull docker.io/library/nextcloud:latest",
        "stop nc1",
        "rename nc1 nextcloud-stp-previous",
        "create nextcloud",
        "start new-container",
        "remove nc1",
    ]
    .map(|call| position(&calls, call));
    assert!(order.is_sorted(), "{calls:?}");
    assert!(!calls.contains(
        &"tag docker.io/library/nextcloud:latest-stp-previous docker.io/library/nextcloud:latest".to_owned()
    ));
}

#[tokio::test]
async fn recreates_the_container_from_its_own_settings_minus_the_old_image_defaults() {
    let lab = Lab::new();
    lab.run_nextcloud_29();
    lab.publish_nextcloud_latest("sha256:nextcloud30index");
    let (app, _) = lab.app(&APPLY).await;

    app.update("cloud/nextcloud", true, None).await.unwrap();

    let spec = &lab.docker.created()[0];
    assert_eq!(spec["Image"], "docker.io/library/nextcloud:latest");
    assert_eq!(spec["Env"], json!(["NEXTCLOUD_ADMIN_PASSWORD=hunter2"]));
    assert_eq!(
        spec["HostConfig"]["Binds"],
        json!(["/srv/data:/data", "anonymous0123:/cache"])
    );
    assert_eq!(
        spec["ExposedPorts"],
        json!({"9000/tcp": {}}),
        "a published port stays, the image's own are dropped"
    );
    assert_eq!(spec["Labels"]["com.docker.compose.service"], "nextcloud");
    assert_eq!(spec["Labels"]["com.docker.compose.image"], "sha256:new");
    assert_eq!(
        spec["NetworkingConfig"]["EndpointsConfig"]["media_default"],
        json!({"Aliases": ["nextcloud"]})
    );
    for inherited in [
        "Cmd",
        "Entrypoint",
        "WorkingDir",
        "User",
        "Hostname",
        "Healthcheck",
        "StopSignal",
        "Volumes",
    ] {
        assert!(
            spec.get(inherited).is_none(),
            "{inherited} must come from the new image"
        );
    }
    assert!(spec["Labels"].get("maintainer").is_none());
}

#[tokio::test]
async fn changes_nothing_when_the_tag_moved_to_a_release_that_was_not_checked() {
    let lab = Lab::new();
    lab.run_nextcloud_29();
    lab.publish_nextcloud_latest("sha256:somethingnewer");
    let (app, _) = lab.app(&APPLY).await;

    let error = app.update("cloud/nextcloud", true, None).await.unwrap_err();

    assert!(matches!(error, UpdateError::UnexpectedVersion { .. }), "{error}");
    let calls = lab.docker.calls();
    assert!(!calls.iter().any(|call| call.starts_with("stop")));
    assert!(
        calls.contains(
            &"tag docker.io/library/nextcloud:latest-stp-previous docker.io/library/nextcloud:latest".to_owned()
        ),
        "the tag must point back at the running image"
    );
}

#[tokio::test]
async fn restores_the_previous_container_when_the_replacement_does_not_start() {
    let lab = Lab::new();
    lab.run_nextcloud_29();
    lab.publish_nextcloud_latest("sha256:nextcloud30index");
    lab.docker.fail_start_of("new-container");
    let (app, _) = lab.app(&APPLY).await;

    let error = app.update("cloud/nextcloud", true, None).await.unwrap_err();

    assert!(matches!(error, UpdateError::RolledBack(_)), "{error}");
    let calls = lab.docker.calls();
    let order = [
        "remove new-container",
        "rename nc1 nextcloud",
        "start nc1",
        "tag docker.io/library/nextcloud:latest-stp-previous docker.io/library/nextcloud:latest",
    ]
    .map(|call| position(&calls, call));
    assert!(order.is_sorted(), "{calls:?}");
    assert!(!calls.contains(&"remove nc1".to_owned()));
}

#[tokio::test]
async fn an_update_with_manual_steps_needs_an_explicit_acknowledgement() {
    let lab = Lab::new();
    lab.run_nextcloud_29();
    let (app, _) = lab.app(&APPLY).await;

    let error = app.update("cloud/nextcloud", false, None).await.unwrap_err();

    assert!(matches!(error, UpdateError::NeedsAcknowledgement), "{error}");
    assert!(!lab.docker.calls().iter().any(|call| call.starts_with("pull")));
}

#[tokio::test]
async fn advisory_mode_refuses_every_update_without_calling_docker() {
    let lab = Lab::new();
    lab.run_nextcloud_29();
    let (app, _) = lab.app(&[]).await;
    let calls_before = lab.docker.calls().len();

    assert!(matches!(
        app.update("cloud/nextcloud", true, None).await,
        Err(UpdateError::Disabled)
    ));
    assert_eq!(lab.docker.calls().len(), calls_before);
}

#[tokio::test]
async fn refuses_services_it_does_not_track_or_cannot_update_itself() {
    let lab = Lab::new();
    lab.docker.run(support::FakeContainer {
        id: "im1",
        name: "immich-server",
        image: "ghcr.io/immich-app/immich-server:v1.133.0",
        image_id: "sha256:im",
        stack: None,
        env: &[],
        repo_digests: &[],
    });
    let (app, _) = lab.app(&APPLY).await;

    assert!(matches!(
        app.update("redis", true, None).await,
        Err(UpdateError::UnknownService(_))
    ));
    assert!(matches!(
        app.update("immich-server", true, None).await,
        Err(UpdateError::NotUpdatable(_))
    ));
}

#[tokio::test]
async fn refuses_before_pulling_when_the_image_the_container_came_from_is_gone() {
    let lab = Lab::new();
    lab.run_nextcloud_29();
    lab.publish_nextcloud_latest("sha256:nextcloud30index");
    let (app, _) = lab.app(&APPLY).await;
    lab.docker.move_tag_away_from("nc1");

    let error = app.update("cloud/nextcloud", true, None).await.unwrap_err();

    assert!(matches!(error, UpdateError::NotUpdatable(_)), "{error}");
    assert!(
        !app.status().await.snapshot.services[0].can_update,
        "the next scan stops offering the update"
    );
    assert!(!lab
        .docker
        .calls()
        .iter()
        .any(|call| call.starts_with("pull") || call.starts_with("stop")));
}

#[tokio::test]
async fn clears_a_leftover_parked_container_before_stopping_the_running_one() {
    let lab = Lab::new();
    lab.run_nextcloud_29();
    lab.publish_nextcloud_latest("sha256:nextcloud30index");
    let (app, _) = lab.app(&APPLY).await;

    app.update("cloud/nextcloud", true, None).await.unwrap();

    let calls = lab.docker.calls();
    assert!(position(&calls, "remove nextcloud-stp-previous") < position(&calls, "stop nc1"));
}

#[tokio::test]
async fn starts_the_running_container_again_when_it_cannot_be_parked() {
    let lab = Lab::new();
    lab.run_nextcloud_29();
    lab.publish_nextcloud_latest("sha256:nextcloud30index");
    lab.docker.fail("rename nc1 nextcloud-stp-previous");
    let (app, _) = lab.app(&APPLY).await;

    let error = app.update("cloud/nextcloud", true, None).await.unwrap_err();

    assert!(matches!(error, UpdateError::RolledBack(_)), "{error}");
    let calls = lab.docker.calls();
    assert!(position(&calls, "stop nc1") < position(&calls, "start nc1"));
    assert!(!calls.iter().any(|call| call.starts_with("create")));
}

#[tokio::test]
async fn a_replaced_container_that_cannot_be_cleared_does_not_undo_a_successful_update() {
    let lab = Lab::new();
    lab.run_nextcloud_29();
    lab.publish_nextcloud_latest("sha256:nextcloud30index");
    lab.docker.fail("remove nc1");
    let (app, _) = lab.app(&APPLY).await;

    assert_eq!(app.update("cloud/nextcloud", true, None).await.unwrap(), "v30.0.0");

    let calls = lab.docker.calls();
    assert!(calls.contains(&"start new-container".to_owned()));
    assert!(!calls
        .iter()
        .any(|call| call.ends_with("docker.io/library/nextcloud:latest") && call.starts_with("tag docker.io")));
}

#[tokio::test]
async fn says_so_when_the_previous_container_could_not_be_restored() {
    let lab = Lab::new();
    lab.run_nextcloud_29();
    lab.publish_nextcloud_latest("sha256:nextcloud30index");
    lab.docker.fail_start_of("new-container");
    lab.docker.fail("rename nc1 nextcloud");
    let (app, _) = lab.app(&APPLY).await;

    let error = app.update("cloud/nextcloud", true, None).await.unwrap_err();

    assert!(matches!(error, UpdateError::RestoreFailed { .. }), "{error}");
    assert!(
        lab.docker.calls().contains(&"start nc1".to_owned()),
        "every restore step is still attempted"
    );
}

#[tokio::test]
async fn reports_a_pull_that_docker_rejected_inside_its_progress_stream() {
    let lab = Lab::new();
    lab.run_nextcloud_29();
    let (app, _) = lab.app(&APPLY).await;

    let error = app.update("cloud/nextcloud", true, None).await.unwrap_err();

    assert!(error.to_string().contains("manifest unknown"), "{error}");
    assert!(!lab.docker.calls().iter().any(|call| call.starts_with("stop")));
}
