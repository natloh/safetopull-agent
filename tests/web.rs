mod support;

use reqwest::{Client, Response, StatusCode};
use serde_json::{json, Value};
use stp_agent::web::{self, INTENT_HEADER};
use support::Lab;

async fn dashboard(lab: &Lab, settings: &[(&str, &str)]) -> String {
    lab.run_nextcloud_29();
    let (app, _) = lab.app(settings).await;
    support::serve(web::router(app)).await
}

async fn error_of(response: Response) -> String {
    response.json::<Value>().await.unwrap()["error"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn serves_the_dashboard_and_a_status_without_environment_values() {
    let lab = Lab::new();
    let base = dashboard(&lab, &[]).await;

    let page = reqwest::get(format!("{base}/")).await.unwrap().text().await.unwrap();
    let status = reqwest::get(format!("{base}/api/status"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let health = reqwest::get(format!("{base}/healthz")).await.unwrap();

    assert!(page.contains("<title>SafeToPull agent</title>"));
    assert_eq!(health.status(), StatusCode::OK);
    assert!(status.contains("\"key\":\"cloud/nextcloud\""));
    assert!(status.contains("\"update_mode\":\"advisory\""));
    assert!(!status.contains("hunter2"));
    assert!(!status.contains("container_id"));
}

#[tokio::test]
async fn actions_need_the_intent_header_and_refuse_other_origins() {
    let lab = Lab::new();
    let base = dashboard(&lab, &[]).await;
    let host = base.trim_start_matches("http://");
    let refresh = || Client::new().post(format!("{base}/api/refresh"));

    assert_eq!(refresh().send().await.unwrap().status(), StatusCode::FORBIDDEN);
    assert_eq!(
        refresh()
            .header(INTENT_HEADER, "1")
            .header("Origin", "http://evil.example")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        refresh()
            .header(INTENT_HEADER, "1")
            .header("Origin", format!("http://{host}"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        refresh().header(INTENT_HEADER, "1").send().await.unwrap().status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn an_api_token_protects_status_and_actions_when_configured() {
    let lab = Lab::new();
    let base = dashboard(&lab, &[("STP_API_TOKEN", "letmein")]).await;
    let status = || Client::new().get(format!("{base}/api/status"));

    assert_eq!(status().send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        status().bearer_auth("wrong!!").send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status().bearer_auth("letmein").send().await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(
        reqwest::get(format!("{base}/healthz")).await.unwrap().status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn the_update_action_is_forbidden_in_advisory_mode() {
    let lab = Lab::new();
    let base = dashboard(&lab, &[]).await;

    let response = Client::new()
        .post(format!("{base}/api/update"))
        .header(INTENT_HEADER, "1")
        .json(&json!({"key": "cloud/nextcloud", "acknowledged": true}))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(error_of(response).await.contains("STP_UPDATE_MODE=apply"));
}

#[tokio::test]
async fn the_update_action_reports_what_it_updated_to_in_apply_mode() {
    let lab = Lab::new();
    lab.publish_nextcloud_latest("sha256:nextcloud30index");
    let base = dashboard(&lab, &[("STP_UPDATE_MODE", "apply")]).await;
    let update = |acknowledged: bool| {
        Client::new()
            .post(format!("{base}/api/update"))
            .header(INTENT_HEADER, "1")
            .json(&json!({"key": "cloud/nextcloud", "acknowledged": acknowledged}))
            .send()
    };

    assert_eq!(update(false).await.unwrap().status(), StatusCode::CONFLICT);

    let updated = update(true).await.unwrap();
    assert_eq!(updated.status(), StatusCode::OK);
    assert_eq!(updated.json::<Value>().await.unwrap()["updated_to"], "v30.0.0");
}

#[tokio::test]
async fn pairing_through_the_dashboard_links_and_validates_its_input() {
    let lab = Lab::new();
    let base = dashboard(&lab, &[]).await;
    let pair = |body: Value| {
        Client::new()
            .post(format!("{base}/api/cloud/pair"))
            .header(INTENT_HEADER, "1")
            .json(&body)
            .send()
    };

    let blank = pair(json!({"code": " ", "name": "nas"})).await.unwrap();
    assert_eq!(blank.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let insecure = pair(json!({"code": "ABCDE-FGHJK", "name": "nas", "cloud_url": "http://example.test"}))
        .await
        .unwrap();
    assert_eq!(insecure.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let linked = pair(json!({"code": "ABCDE-FGHJK", "name": "nas"})).await.unwrap();
    assert_eq!(linked.status(), StatusCode::OK);
    assert_eq!(linked.json::<Value>().await.unwrap()["host_name"], "nas");
    assert_eq!(
        lab.cloud.reports().len(),
        1,
        "the first report is sent as soon as the host is linked"
    );

    let unlinked = Client::new()
        .post(format!("{base}/api/cloud/unlink"))
        .header(INTENT_HEADER, "1")
        .send()
        .await
        .unwrap();
    assert_eq!(unlinked.status(), StatusCode::NO_CONTENT);
    assert!(lab.cloud.unlinked());
}
