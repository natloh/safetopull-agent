//! The local dashboard and API. It serves the machine it runs on and nothing else.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header::{self, HeaderName, HeaderValue};
use axum::http::{HeaderMap, StatusCode};
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
const SCRIPT: &str = include_str!("ui/app.js");
const STYLES: &str = include_str!("ui/app.css");

/// Scripts and styles load only from this server and nothing is inline, so markup injected into
/// the page could not run. The page may not be framed, which rules out clickjacking its buttons.
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; \
     frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

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
        .route(
            "/app.js",
            get(|| async { asset("text/javascript; charset=utf-8", SCRIPT) }),
        )
        .route("/app.css", get(|| async { asset("text/css; charset=utf-8", STYLES) }))
        .route("/healthz", get(|| async { "ok" }))
        .nest("/api", api)
        .layer(middleware::from_fn(security_headers))
        .with_state(app)
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;

    for (name, value) in [
        (header::CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "no-referrer"),
        (HeaderName::from_static("cross-origin-resource-policy"), "same-origin"),
    ] {
        response.headers_mut().insert(name, HeaderValue::from_static(value));
    }

    response
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

/// Host names are case-insensitive, so the comparison is too.
fn same_origin(headers: &HeaderMap) -> bool {
    let text = |name| headers.get(name).and_then(|value| value.to_str().ok());

    match (text(header::ORIGIN), text(header::HOST)) {
        (None, _) => true,
        (Some(origin), Some(host)) => origin
            .split_once("://")
            .is_some_and(|(_, authority)| authority.eq_ignore_ascii_case(host)),
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

/// The account address is deliberately not part of the request: it comes from `STP_CLOUD_URL`
/// only, so whoever can reach this port cannot point the agent at a server of their choosing.
#[derive(Deserialize)]
struct PairRequest {
    code: String,
    name: String,
}

async fn pair(State(app): State<Arc<App>>, Json(request): Json<PairRequest>) -> Response {
    let (code, name) = (request.code.trim(), request.name.trim());

    if code.is_empty() || name.is_empty() {
        return failure(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Enter the pairing code and a name for this host.",
        );
    }

    match app.cloud.pair(&app.config.cloud_url, code, name).await {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(HeaderName, &'static str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(name, value)| (name.clone(), HeaderValue::from_static(value)))
            .collect()
    }

    #[test]
    fn an_origin_matches_its_host_whatever_the_letter_case() {
        assert!(same_origin(&headers(&[
            (header::ORIGIN, "http://NAS.lan:8484"),
            (header::HOST, "nas.lan:8484")
        ])));
        assert!(same_origin(&headers(&[(header::HOST, "nas.lan:8484")])));
        assert!(!same_origin(&headers(&[
            (header::ORIGIN, "http://evil.example"),
            (header::HOST, "nas.lan:8484")
        ])));
        assert!(!same_origin(&headers(&[(header::ORIGIN, "http://nas.lan:8484")])));
    }
}
