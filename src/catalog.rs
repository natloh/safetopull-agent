//! Fetches the public dataset with conditional requests and keeps the last good copy on disk, so
//! an unreachable CDN degrades to stale data instead of no data.

use std::path::{Path, PathBuf};

use reqwest::header::{ETAG, IF_NONE_MATCH};
use reqwest::StatusCode;
use serde::de::DeserializeOwned;

use crate::manifest::{AppIndex, AppManifest};
use crate::outbound::Outbound;

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("`{0}` is not a valid app slug")]
    InvalidSlug(String),
    #[error("dataset unavailable and nothing cached for {path}: {reason}")]
    Unavailable { path: String, reason: String },
    #[error("dataset file {path} is not valid: {reason}")]
    Malformed { path: String, reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Downloaded,
    Revalidated,
    /// The request failed, so the cached copy was used.
    Stale,
}

#[derive(Debug)]
pub struct Fetched<T> {
    pub value: T,
    pub freshness: Freshness,
}

#[derive(Clone)]
pub struct Catalog {
    outbound: Outbound,
    base_url: String,
    cache_dir: PathBuf,
}

impl Catalog {
    pub fn new(outbound: Outbound, base_url: &str, cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            outbound,
            base_url: base_url.trim_end_matches('/').to_owned(),
            cache_dir: cache_dir.into(),
        }
    }

    pub async fn index(&self) -> Result<Fetched<AppIndex>, CatalogError> {
        self.fetch("app index", "v1/index.json").await
    }

    pub async fn manifest(&self, slug: &str) -> Result<Fetched<AppManifest>, CatalogError> {
        let valid = !slug.is_empty()
            && slug
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');

        if !valid {
            return Err(CatalogError::InvalidSlug(slug.to_owned()));
        }

        self.fetch("app manifest", &format!("v1/apps/{slug}.json")).await
    }

    async fn fetch<T: DeserializeOwned>(&self, purpose: &'static str, path: &str) -> Result<Fetched<T>, CatalogError> {
        let body_file = self.cache_dir.join(path);
        let etag_file = body_file.with_extension("etag");
        let cached_body = tokio::fs::read(&body_file).await.ok();
        let cached_etag = match cached_body {
            Some(_) => tokio::fs::read_to_string(&etag_file).await.ok(),
            None => None,
        };

        let mut request = self.outbound.client().get(format!("{}/{path}", self.base_url));
        if let Some(etag) = &cached_etag {
            request = request.header(IF_NONE_MATCH, etag.trim());
        }

        let mut downloaded_etag = None;
        let (body, freshness) = match self.outbound.send(purpose, request).await {
            Ok(response) if response.status() == StatusCode::NOT_MODIFIED => match cached_body {
                Some(body) => (body, Freshness::Revalidated),
                None => return Err(unavailable(path, "HTTP 304 without a cached copy")),
            },
            Ok(response) if response.status().is_success() => {
                downloaded_etag = response
                    .headers()
                    .get(ETAG)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                (
                    response
                        .bytes()
                        .await
                        .map_err(|error| unavailable(path, error))?
                        .to_vec(),
                    Freshness::Downloaded,
                )
            }
            Ok(response) => match cached_body {
                Some(body) => (body, Freshness::Stale),
                None => return Err(unavailable(path, format!("HTTP {}", response.status().as_u16()))),
            },
            Err(error) => match cached_body {
                Some(body) => (body, Freshness::Stale),
                None => return Err(unavailable(path, error)),
            },
        };

        let value = serde_json::from_slice(&body).map_err(|error| CatalogError::Malformed {
            path: path.to_owned(),
            reason: error.to_string(),
        })?;

        if freshness == Freshness::Downloaded {
            store(&body_file, &etag_file, &body, downloaded_etag).await;
        }

        Ok(Fetched { value, freshness })
    }
}

fn unavailable(path: &str, reason: impl ToString) -> CatalogError {
    CatalogError::Unavailable {
        path: path.to_owned(),
        reason: reason.to_string(),
    }
}

/// A cache that cannot be written only costs bandwidth, so write failures are not errors.
async fn store(body_file: &Path, etag_file: &Path, body: &[u8], etag: Option<String>) {
    if let Some(parent) = body_file.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    if tokio::fs::write(body_file, body).await.is_ok() {
        let _ = match etag {
            Some(etag) => tokio::fs::write(etag_file, etag).await,
            None => tokio::fs::remove_file(etag_file).await,
        };
    }
}
