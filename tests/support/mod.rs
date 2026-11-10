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
        serve(Router::new().route("/{*path}", get(dataset_file)).with_state(self.clone())).await
    }
}

fn etag_of(body: &str) -> String {
    format!("\"{}-{}\"", body.len(), body.bytes().fold(0u64, |sum, byte| sum.wrapping_mul(31).wrapping_add(byte.into())))
}

async fn dataset_file(State(dataset): State<FakeDataset>, Path(path): Path<String>, headers: HeaderMap) -> Response {
    let mut state = dataset.0.lock().unwrap();
    let presented = headers.get(header::IF_NONE_MATCH).and_then(|value| value.to_str().ok()).map(str::to_owned);
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
    pub failing_starts: Vec<String>,
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
                    "Labels": labels,
                },
                "HostConfig": {"Binds": ["/srv/data:/data"], "RestartPolicy": {"Name": "unless-stopped"}},
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
                    "Labels": {"maintainer": "someone@example.test"},
                },
            }),
        );
    }

    pub fn publish_image(&self, reference: &str, image: Value) {
        self.0.lock().unwrap().images.insert(reference.to_owned(), image);
    }

    pub fn fail_start_of(&self, container_id: &str) {
        self.0.lock().unwrap().failing_starts.push(container_id.to_owned());
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
            .route("/images/{*reference}", get(inspect_image))
            .with_state(self.clone());

        serve(router).await.replacen("http://", "tcp://", 1)
    }
}

async fn list_containers(State(docker): State<FakeDocker>) -> Json<Value> {
    let mut state = docker.0.lock().unwrap();
    state.calls.push("list".to_owned());
    Json(Value::Array(state.containers.clone()))
}

async fn inspect_container(State(docker): State<FakeDocker>, Path(id): Path<String>) -> Response {
    let mut state = docker.0.lock().unwrap();
    state.calls.push(format!("inspect {id}"));
    match state.inspections.get(&id) {
        Some(inspection) => Json(inspection.clone()).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({"message": format!("No such container: {id}")}))).into_response(),
    }
}

async fn inspect_image(State(docker): State<FakeDocker>, Path(reference): Path<String>) -> Response {
    let reference = reference.trim_end_matches("/json");
    let mut state = docker.0.lock().unwrap();
    state.calls.push(format!("image {reference}"));
    match state.images.get(reference) {
        Some(image) => Json(image.clone()).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({"message": format!("No such image: {reference}")}))).into_response(),
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
    state.calls.push(format!("{action} {id}{detail}"));

    if action == "start" && state.failing_starts.contains(&id) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"message": "port is already allocated"}))).into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn remove_container(State(docker): State<FakeDocker>, Path(id): Path<String>) -> StatusCode {
    docker.0.lock().unwrap().calls.push(format!("remove {id}"));
    StatusCode::NO_CONTENT
}
