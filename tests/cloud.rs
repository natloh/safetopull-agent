mod support;

use stp_agent::cloud::CloudError;
use support::{Lab, PAIRING_CODE};

const REMOTE_APPLY: [(&str, &str); 2] = [("STP_UPDATE_MODE", "apply"), ("STP_REMOTE_ACTIONS", "true")];

#[tokio::test]
async fn an_unlinked_agent_never_contacts_the_account_service() {
    let lab = Lab::new();
    lab.run_nextcloud_29();
    let (app, _) = lab.app(&[]).await;

    app.cycle().await;

    assert!(lab.cloud.reports().is_empty());
    assert!(app
        .status()
        .await
        .outbound
        .iter()
        .all(|record| record.purpose.starts_with("app ")));
}

#[tokio::test]
async fn a_linked_agent_reports_exactly_the_contract_fields_and_no_secrets() {
    let lab = Lab::new();
    lab.run_nextcloud_29();
    let (app, cloud_url) = lab.app(&[]).await;

    let link = app.cloud.pair(&cloud_url, PAIRING_CODE, "nas").await.unwrap();
    app.cycle().await;

    assert_eq!((link.host_id, link.host_name.as_str()), (7, "nas"));

    let report = &lab.cloud.reports()[0];
    let mut top_level: Vec<&str> = report.as_object().unwrap().keys().map(String::as_str).collect();
    let mut service: Vec<&str> = report["services"][0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    top_level.sort_unstable();
    service.sort_unstable();

    assert_eq!(
        top_level,
        [
            "agent_version",
            "arch",
            "remote_actions_enabled",
            "services",
            "update_mode"
        ]
    );
    assert_eq!(
        service,
        [
            "app",
            "blocking_count",
            "can_update",
            "current_version",
            "image",
            "key",
            "name",
            "stack",
            "summary",
            "target_version",
            "verdict",
            "warning_count"
        ]
    );
    assert_eq!(report["services"][0]["verdict"], "yellow");
    assert_eq!(report["update_mode"], "advisory");
    assert!(!report.to_string().contains("hunter2"));
    assert!(!report.to_string().contains("NEXTCLOUD_ADMIN_PASSWORD"));
}

#[tokio::test]
async fn the_link_survives_a_restart() {
    let lab = Lab::new();
    let (app, cloud_url) = lab.app(&[]).await;
    app.cloud.pair(&cloud_url, PAIRING_CODE, "nas").await.unwrap();

    let (restarted, _) = lab.app(&[]).await;

    assert_eq!(restarted.cloud.status().await.unwrap().host_name, "nas");
}

#[tokio::test]
async fn a_wrong_code_or_an_insecure_address_does_not_link() {
    let lab = Lab::new();
    let (app, cloud_url) = lab.app(&[]).await;

    let wrong_code = app.cloud.pair(&cloud_url, "WRONG", "nas").await.unwrap_err();
    assert_eq!(wrong_code.to_string(), "This pairing code is invalid or has expired.");

    let insecure = app
        .cloud
        .pair("http://account.example.test", PAIRING_CODE, "nas")
        .await
        .unwrap_err();
    assert!(matches!(insecure, CloudError::InsecureUrl(_)));

    assert_eq!(app.cloud.status().await, None);
}

#[tokio::test]
async fn a_link_revoked_from_the_account_makes_the_agent_forget_its_token() {
    let lab = Lab::new();
    let (app, cloud_url) = lab.app(&[]).await;
    app.cloud.pair(&cloud_url, PAIRING_CODE, "nas").await.unwrap();

    lab.cloud.revoke();
    app.cycle().await;

    assert_eq!(app.cloud.status().await, None);
    assert!(!lab.data.path().join("cloud-link.json").exists());
}

#[tokio::test]
async fn unlinking_tells_the_account_and_stops_reporting() {
    let lab = Lab::new();
    let (app, cloud_url) = lab.app(&[]).await;
    app.cloud.pair(&cloud_url, PAIRING_CODE, "nas").await.unwrap();

    app.cloud.unlink().await.unwrap();
    app.cycle().await;

    assert!(lab.cloud.unlinked());
    assert!(lab.cloud.reports().is_empty());
}

#[tokio::test]
async fn a_remote_update_is_refused_unless_this_host_enabled_remote_actions() {
    let lab = Lab::new();
    lab.run_nextcloud_29();
    lab.publish_nextcloud_latest("sha256:nextcloud30index");
    let (app, cloud_url) = lab.app(&[("STP_UPDATE_MODE", "apply")]).await;
    app.cloud.pair(&cloud_url, PAIRING_CODE, "nas").await.unwrap();

    lab.cloud.queue_update(11, "cloud/nextcloud", "v30.0.0");
    app.cycle().await;

    let (id, result) = &lab.cloud.results()[0];
    assert_eq!((*id, result["status"].as_str()), (11, Some("rejected")));
    assert!(!lab.docker.calls().iter().any(|call| call.starts_with("pull")));
}

#[tokio::test]
async fn a_remote_update_runs_when_enabled_and_reports_its_outcome() {
    let lab = Lab::new();
    lab.run_nextcloud_29();
    lab.publish_nextcloud_latest("sha256:nextcloud30index");
    let (app, cloud_url) = lab.app(&REMOTE_APPLY).await;
    app.cloud.pair(&cloud_url, PAIRING_CODE, "nas").await.unwrap();

    lab.cloud.queue_update(12, "cloud/nextcloud", "v30.0.0");
    lab.cloud.queue_update(13, "cloud/nextcloud", "v31.0.0");
    app.cycle().await;

    let results = lab.cloud.results();
    assert_eq!(results[0].1["status"], "succeeded");
    assert_eq!(results[0].1["message"], "Updated to v30.0.0.");
    assert_eq!(
        results[1].1["status"], "rejected",
        "a target the agent did not check must not run"
    );
    assert!(lab.docker.calls().contains(&"remove nc1".to_owned()));
}
