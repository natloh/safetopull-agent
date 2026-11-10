//! The local dashboard and API. It serves the machine it runs on and nothing else.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::app::App;
use crate::cloud::CloudError;
use crate::update::UpdateError;

/// A custom header cannot be sent cross-origin without a CORS preflight, and this server never
/// answers one, so other websites cannot trigger actions through a visitor's browser.
pub const INTENT_HEADER: &str = "x-stp-request";

const DASHBOARD: &str = include_str!("ui/index.html");

pub fn router(app: Arc<App>) -> Router {
    let actions = Router::new()
        .route("/refresh", post(refresh))
        .route("/update", post(update))
        .route("/cloud/pair", post(pair))
        .route("/cloud/unlink", post(unlink))
        .layer(middleware::from_fn(same_origin_intent));

    let api = Router::new()
        .route("/status", get(status))
        .merge(actions)
        .layer(middleware::from_fn_with_state(app.clone(), authorized));

    Router::new()
        .route("/", get(|| async { Html(DASHBOARD) }))
        .route("/healthz", get(|| async { "ok" }))
        .nest("/api", api)
        .with_state(app)
}

async fn authorized(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    let Some(expected) = &app.config.api_token else {
        return next.run(request).await;
    };

    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));

    match presented {
        Some(token) if constant_time_eq(token.as_bytes(), expected.as_bytes()) => next.run(request).await,
        _ => failure(StatusCode::UNAUTHORIZED, "This agent requires its API token."),
    }
}

async fn same_origin_intent(request: Request, next: Next) -> Response {
    if !request.headers().contains_key(INTENT_HEADER) {
        return failure(StatusCode::FORBIDDEN, "Actions must come from the agent dashboard.");
    }
    if !same_origin(request.headers()) {
        return failure(StatusCode::FORBIDDEN, "Actions from another site are refused.");
    }
    next.run(request).await
}

fn same_origin(headers: &HeaderMap) -> bool {
    let text = |name| headers.get(name).and_then(|value| value.to_str().ok());

    match (text(header::ORIGIN), text(header::HOST)) {
        (None, _) => true,
        (Some(origin), Some(host)) => origin.split_once("://").is_some_and(|(_, authority)| authority == host),
        (Some(_), None) => false,
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0, |difference, (a, b)| difference | (a ^ b))
            == 0
}

fn failure(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

async fn status(State(app): State<Arc<App>>) -> Response {
    Json(app.status().await).into_response()
}

async fn refresh(State(app): State<Arc<App>>) -> Response {
    app.scan().await;
    Json(app.status().await).into_response()
}

#[derive(Deserialize)]
struct UpdateRequest {
    key: String,
    #[serde(default)]
    acknowledged: bool,
}

async fn update(State(app): State<Arc<App>>, Json(request): Json<UpdateRequest>) -> Response {
    match app.update(&request.key, request.acknowledged, None).await {
        Ok(version) => Json(json!({ "updated_to": version })).into_response(),
        Err(error) => {
            let status = match error {
                UpdateError::Disabled => StatusCode::FORBIDDEN,
                UpdateError::UnknownService(_) => StatusCode::NOT_FOUND,
                UpdateError::NotUpdatable(_) | UpdateError::NeedsAcknowledgement => StatusCode::CONFLICT,
                _ => StatusCode::BAD_GATEWAY,
            };
            failure(status, error.to_string())
        }
    }
}

#[derive(Deserialize)]
struct PairRequest {
    code: String,
    name: String,
    #[serde(default)]
    cloud_url: Option<String>,
}

async fn pair(State(app): State<Arc<App>>, Json(request): Json<PairRequest>) -> Response {
    let name = request.name.trim();
    if name.is_empty() || request.code.trim().is_empty() {
        return failure(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Enter the pairing code and a name for this host.",
        );
    }

    let cloud_url = request
        .cloud_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .unwrap_or(&app.config.cloud_url);

    match app.cloud.pair(cloud_url, request.code.trim(), name).await {
        Ok(link) => {
            app.sync_cloud().await;
            Json(link).into_response()
        }
        Err(error @ (CloudError::InsecureUrl(_) | CloudError::Rejected(_))) => {
            failure(StatusCode::UNPROCESSABLE_ENTITY, error.to_string())
        }
        Err(error) => failure(StatusCode::BAD_GATEWAY, error.to_string()),
    }
}

async fn unlink(State(app): State<Arc<App>>) -> Response {
    match app.cloud.unlink().await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => failure(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}
