#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

pub fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

pub async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{address}")
}

#[derive(Default)]
pub struct DatasetState {
    pub files: HashMap<String, String>,
    pub requests: Vec<(String, Option<String>)>,
    pub failing: bool,
}

#[derive(Clone, Default)]
pub struct FakeDataset(pub Arc<Mutex<DatasetState>>);

impl FakeDataset {
    pub fn with_fixtures() -> Self {
        let dataset = Self::default();
        dataset.put("v1/index.json", &fixture("index.json"));
        dataset.put("v1/apps/immich.json", &fixture("immich.json"));
        dataset.put("v1/apps/nextcloud.json", &fixture("nextcloud.json"));
        dataset
    }

    pub fn put(&self, path: &str, body: &str) {
        self.0.lock().unwrap().files.insert(path.to_owned(), body.to_owned());
    }

    pub fn fail(&self, failing: bool) {
        self.0.lock().unwrap().failing = failing;
    }

    pub fn requests(&self) -> Vec<(String, Option<String>)> {
        self.0.lock().unwrap().requests.clone()
    }

    pub async fn start(&self) -> String {
        serve(
            Router::new()
                .route("/{*path}", get(dataset_file))
                .with_state(self.clone()),
        )
        .await
    }
}

fn etag_of(body: &str) -> String {
    format!(
        "\"{}-{}\"",
        body.len(),
        body.bytes()
            .fold(0u64, |sum, byte| sum.wrapping_mul(31).wrapping_add(byte.into()))
    )
}

async fn dataset_file(State(dataset): State<FakeDataset>, Path(path): Path<String>, headers: HeaderMap) -> Response {
    let mut state = dataset.0.lock().unwrap();
    let presented = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    state.requests.push((path.clone(), presented.clone()));

    if state.failing {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }

    match state.files.get(&path) {
        None => StatusCode::NOT_FOUND.into_response(),
        Some(body) if presented.as_deref() == Some(etag_of(body).as_str()) => StatusCode::NOT_MODIFIED.into_response(),
        Some(body) => ([(header::ETAG, etag_of(body))], body.clone()).into_response(),
    }
}

#[derive(Default)]
pub struct DockerState {
    pub containers: Vec<Value>,
    pub inspections: HashMap<String, Value>,
    pub images: HashMap<String, Value>,
    pub calls: Vec<String>,
    pub created: Vec<Value>,
    pub failures: Vec<String>,
}

#[derive(Clone, Default)]
pub struct FakeDocker(pub Arc<Mutex<DockerState>>);

pub struct FakeContainer<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub image: &'a str,
    pub image_id: &'a str,
    pub stack: Option<&'a str>,
    pub env: &'a [&'a str],
    pub repo_digests: &'a [&'a str],
}

impl FakeDocker {
    pub fn run(&self, container: FakeContainer) {
        let mut labels = json!({"maintainer": "someone@example.test", "traefik.secret": "do-not-read"});
        if let Some(stack) = container.stack {
            labels["com.docker.compose.project"] = json!(stack);
            labels["com.docker.compose.service"] = json!(container.name);
            labels["com.docker.compose.image"] = json!(container.image_id);
        }

        let mut state = self.0.lock().unwrap();
        state.containers.push(json!({
            "Id": container.id,
            "Names": [format!("/{}", container.name)],
            "Image": container.image,
            "ImageID": container.image_id,
            "Labels": labels,
            "State": "running",
        }));
        state.inspections.insert(
            container.id.to_owned(),
            json!({
                "Id": container.id,
                "Name": format!("/{}", container.name),
                "Image": container.image_id,
                "Config": {
                    "Hostname": &container.id[..container.id.len().min(12)],
                    "Image": container.image,
                    "Env": container.env,
                    "Cmd": ["start.sh"],
                    "Entrypoint": ["tini", "--"],
                    "WorkingDir": "/usr/src/app",
                    "User": "",
                    "Healthcheck": {"Test": ["CMD", "old-check"]},
                    "StopSignal": "SIGTERM",
                    "ExposedPorts": {"2283/tcp": {}, "9000/tcp": {}},
                    "Volumes": {"/cache": {}},
                    "Labels": labels,
                },
                "HostConfig": {
                    "Binds": ["/srv/data:/data"],
                    "PortBindings": {"9000/tcp": [{"HostPort": "9000"}]},
                    "RestartPolicy": {"Name": "unless-stopped"},
                },
                "Mounts": [
                    {"Type": "bind", "Source": "/srv/data", "Destination": "/data", "RW": true},
                    {"Type": "volume", "Name": "anonymous0123", "Destination": "/cache", "RW": true},
                ],
                "NetworkSettings": {"Networks": {"media_default": {"Aliases": [container.name], "IPAddress": "172.18.0.2"}}},
            }),
        );
        state.images.insert(
            container.image_id.to_owned(),
            json!({
                "Id": container.image_id,
                "RepoDigests": container.repo_digests,
                "Config": {
                    "Env": ["PATH=/usr/local/bin", "IMMICH_BUILD=old"],
                    "Cmd": ["start.sh"],
                    "Entrypoint": ["tini", "--"],
                    "WorkingDir": "/usr/src/app",
                    "User": "",
                    "Healthcheck": {"Test": ["CMD", "old-check"]},
                    "StopSignal": "SIGTERM",
                    "ExposedPorts": {"2283/tcp": {}},
                    "Volumes": {"/cache": {}},
                    "Labels": {"maintainer": "someone@example.test"},
                },
            }),
        );
    }

    /// What a real engine reports after the tag moved on: the container is listed by image ID and
    /// (with the containerd image store) the old image can no longer be inspected.
    pub fn move_tag_away_from(&self, container_id: &str) {
        let mut state = self.0.lock().unwrap();
        let listed = state
            .containers
            .iter_mut()
            .find(|container| container["Id"] == container_id)
            .unwrap();
        let image_id = listed["ImageID"].as_str().unwrap().to_owned();
        listed["Image"] = json!(image_id);
        state.images.remove(&image_id);
    }

    pub fn publish_image(&self, reference: &str, image: Value) {
        self.0.lock().unwrap().images.insert(reference.to_owned(), image);
    }

    pub fn fail_start_of(&self, container_id: &str) {
        self.fail(&format!("start {container_id}"));
    }

    /// Makes the engine answer 500 to one call, written as it appears in `calls()`.
    pub fn fail(&self, call: &str) {
        self.0.lock().unwrap().failures.push(call.to_owned());
    }

    pub fn calls(&self) -> Vec<String> {
        self.0.lock().unwrap().calls.clone()
    }

    pub fn created(&self) -> Vec<Value> {
        self.0.lock().unwrap().created.clone()
    }

    pub async fn start(&self) -> String {
        let router = Router::new()
            .route("/containers/json", get(list_containers))
            .route("/containers/create", post(create_container))
            .route("/containers/{id}/json", get(inspect_container))
            .route("/containers/{id}/{action}", post(container_action))
            .route("/containers/{id}", delete(remove_container))
            .route("/images/create", post(pull_image))
            .route("/images/{*reference}", get(inspect_image).post(tag_image))
            .with_state(self.clone());

        serve(router).await.replacen("http://", "tcp://", 1)
    }
}

fn refused(state: &DockerState, call: &str) -> Option<Response> {
    state.failures.iter().any(|failing| failing == call).then(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"message": format!("engine refused to {call}")})),
        )
            .into_response()
    })
}

async fn list_containers(State(docker): State<FakeDocker>) -> Json<Value> {
    let mut state = docker.0.lock().unwrap();
    state.calls.push("list".to_owned());
    Json(Value::Array(state.containers.clone()))
}

async fn inspect_container(State(docker): State<FakeDocker>, Path(id): Path<String>) -> Response {
    let mut state = docker.0.lock().unwrap();
    let call = format!("inspect {id}");
    state.calls.push(call.clone());

    if let Some(refusal) = refused(&state, &call) {
        return refusal;
    }
    match state.inspections.get(&id) {
        Some(inspection) => Json(inspection.clone()).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"message": format!("No such container: {id}")})),
        )
            .into_response(),
    }
}

async fn tag_image(
    State(docker): State<FakeDocker>,
    Path(source): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> StatusCode {
    let source = source.trim_end_matches("/tag");
    let target = format!("{}:{}", query["repo"], query["tag"]);
    let mut state = docker.0.lock().unwrap();
    state.calls.push(format!("tag {source} {target}"));
    if let Some(image) = state.images.get(source).cloned() {
        state.images.insert(target, image);
    }
    StatusCode::CREATED
}

async fn inspect_image(State(docker): State<FakeDocker>, Path(reference): Path<String>) -> Response {
    let reference = reference.trim_end_matches("/json");
    let mut state = docker.0.lock().unwrap();
    state.calls.push(format!("image {reference}"));
    match state.images.get(reference) {
        Some(image) => Json(image.clone()).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"message": format!("No such image: {reference}")})),
        )
            .into_response(),
    }
}

async fn pull_image(State(docker): State<FakeDocker>, Query(query): Query<HashMap<String, String>>) -> String {
    let reference = format!("{}:{}", query["fromImage"], query["tag"]);
    let mut state = docker.0.lock().unwrap();
    state.calls.push(format!("pull {reference}"));
    if state.images.contains_key(&reference) {
        "{\"status\":\"Pulling\"}\n{\"status\":\"Downloaded newer image\"}\n".to_owned()
    } else {
        "{\"status\":\"Pulling\"}\n{\"error\":\"manifest unknown\"}\n".to_owned()
    }
}

async fn create_container(
    State(docker): State<FakeDocker>,
    Query(query): Query<HashMap<String, String>>,
    Json(spec): Json<Value>,
) -> Json<Value> {
    let mut state = docker.0.lock().unwrap();
    state.calls.push(format!("create {}", query["name"]));
    state.created.push(spec);
    Json(json!({"Id": "new-container"}))
}

async fn container_action(
    State(docker): State<FakeDocker>,
    Path((id, action)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let mut state = docker.0.lock().unwrap();
    let detail = query.get("name").map(|name| format!(" {name}")).unwrap_or_default();
    let call = format!("{action} {id}{detail}");
    state.calls.push(call.clone());

    refused(&state, &call).unwrap_or_else(|| StatusCode::NO_CONTENT.into_response())
}

async fn remove_container(State(docker): State<FakeDocker>, Path(id): Path<String>) -> Response {
    let mut state = docker.0.lock().unwrap();
    let call = format!("remove {id}");
    state.calls.push(call.clone());

    refused(&state, &call).unwrap_or_else(|| StatusCode::NO_CONTENT.into_response())
}

pub const PAIRING_CODE: &str = "ABCDE-FGHJK";
pub const HOST_TOKEN: &str = "stp_test_token";

#[derive(Default)]
pub struct CloudState {
    pub reports: Vec<Value>,
    pub pending: Vec<Value>,
    pub results: Vec<(u64, Value)>,
    pub revoked: bool,
    pub unlinked: bool,
}

#[derive(Clone, Default)]
pub struct FakeCloud(pub Arc<Mutex<CloudState>>);

impl FakeCloud {
    pub fn queue_update(&self, id: u64, service_key: &str, target_version: &str) {
        self.0.lock().unwrap().pending.push(json!({
            "id": id, "type": "update", "service_key": service_key, "target_version": target_version,
        }));
    }

    pub fn revoke(&self) {
        self.0.lock().unwrap().revoked = true;
    }

    pub fn reports(&self) -> Vec<Value> {
        self.0.lock().unwrap().reports.clone()
    }

    pub fn results(&self) -> Vec<(u64, Value)> {
        self.0.lock().unwrap().results.clone()
    }

    pub fn unlinked(&self) -> bool {
        self.0.lock().unwrap().unlinked
    }

    pub async fn start(&self) -> String {
        let router = Router::new()
            .route("/api/agent/v1/pair", post(cloud_pair))
            .route("/api/agent/v1/report", post(cloud_report))
            .route("/api/agent/v1/commands/{id}/result", post(cloud_result))
            .route("/api/agent/v1/link", delete(cloud_unlink))
            .with_state(self.clone());

        serve(router).await
    }
}

fn linked(state: &CloudState, headers: &HeaderMap) -> bool {
    let presented = headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok());
    !state.revoked && presented == Some(format!("Bearer {HOST_TOKEN}").as_str())
}

async fn cloud_pair(Json(body): Json<Value>) -> Response {
    if body["code"] == PAIRING_CODE {
        (
            StatusCode::CREATED,
            Json(json!({"host_id": 7, "name": body["name"], "token": HOST_TOKEN})),
        )
            .into_response()
    } else {
        let rejection = json!({"error": "invalid_code", "message": "This pairing code is invalid or has expired."});
        (StatusCode::UNPROCESSABLE_ENTITY, Json(rejection)).into_response()
    }
}

async fn cloud_report(State(cloud): State<FakeCloud>, headers: HeaderMap, Json(report): Json<Value>) -> Response {
    let mut state = cloud.0.lock().unwrap();
    if !linked(&state, &headers) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"message": "Unauthenticated."}))).into_response();
    }
    state.reports.push(report);
    let commands = std::mem::take(&mut state.pending);
    Json(json!({"commands": commands, "poll_after_seconds": 300})).into_response()
}

async fn cloud_result(
    State(cloud): State<FakeCloud>,
    Path(id): Path<u64>,
    headers: HeaderMap,
    Json(result): Json<Value>,
) -> StatusCode {
    let mut state = cloud.0.lock().unwrap();
    if !linked(&state, &headers) {
        return StatusCode::UNAUTHORIZED;
    }
    state.results.push((id, result));
    StatusCode::NO_CONTENT
}

async fn cloud_unlink(State(cloud): State<FakeCloud>, headers: HeaderMap) -> StatusCode {
    let mut state = cloud.0.lock().unwrap();
    if !linked(&state, &headers) {
        return StatusCode::UNAUTHORIZED;
    }
    state.unlinked = true;
    StatusCode::NO_CONTENT
}

pub struct Lab {
    pub dataset: FakeDataset,
    pub docker: FakeDocker,
    pub cloud: FakeCloud,
    pub data: tempfile::TempDir,
}

impl Lab {
    pub fn new() -> Self {
        Self {
            dataset: FakeDataset::with_fixtures(),
            docker: FakeDocker::default(),
            cloud: FakeCloud::default(),
            data: tempfile::tempdir().unwrap(),
        }
    }

    pub async fn app(&self, settings: &[(&str, &str)]) -> (Arc<stp_agent::app::App>, String) {
        let cloud_url = self.cloud.start().await;
        let mut environment = HashMap::from([
            ("STP_DOCKER_HOST".to_owned(), self.docker.start().await),
            ("STP_DATASET_URL".to_owned(), self.dataset.start().await),
            ("STP_CLOUD_URL".to_owned(), cloud_url.clone()),
            (
                "STP_DATA_DIR".to_owned(),
                self.data.path().to_string_lossy().into_owned(),
            ),
        ]);
        environment.extend(
            settings
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
        );

        let config = stp_agent::config::Config::from_lookup(|name| environment.get(name).cloned()).unwrap();
        let host = stp_agent::host::HostFacts {
            arch: "amd64".to_owned(),
            cpu_flags: Some(["avx2".to_owned()].into()),
        };
        let app = stp_agent::app::App::start(config, host).await.unwrap();
        app.scan().await;

        (app, cloud_url)
    }

    pub fn run_nextcloud_29(&self) {
        self.docker.run(FakeContainer {
            id: "nc1",
            name: "nextcloud",
            image: "nextcloud:latest",
            image_id: "sha256:nc29",
            stack: Some("cloud"),
            env: &[
                "NEXTCLOUD_ADMIN_PASSWORD=hunter2",
                "PATH=/usr/local/bin",
                "IMMICH_BUILD=old",
            ],
            repo_digests: &["nextcloud@sha256:nextcloud29index"],
        });
    }

    pub fn publish_nextcloud_latest(&self, digest: &str) {
        self.docker.publish_image(
            "docker.io/library/nextcloud:latest",
            json!({"Id": "sha256:new", "RepoDigests": [format!("nextcloud@{digest}")], "Config": {}}),
        );
    }
}
