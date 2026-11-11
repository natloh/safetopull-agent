//! The only path by which the agent talks to anything outside the host. Every request is
//! recorded so the local dashboard can show exactly what left the machine.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::redirect::Policy;
use reqwest::{RequestBuilder, Response};
use serde::Serialize;

const KEPT_RECORDS: usize = 50;
const MOST_REDIRECTS: usize = 5;

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[derive(Debug, Clone, Serialize)]
pub struct OutboundRecord {
    pub at: u64,
    pub purpose: &'static str,
    pub method: String,
    pub url: String,
    pub status: Option<u16>,
}

#[derive(Clone)]
pub struct Outbound {
    client: reqwest::Client,
    log: Arc<Mutex<VecDeque<OutboundRecord>>>,
}

impl Outbound {
    pub fn new() -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("stp-agent/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .redirect(Policy::custom(|attempt| {
                let left_tls = attempt.previous().last().is_some_and(|from| from.scheme() == "https")
                    && attempt.url().scheme() != "https";

                if left_tls {
                    attempt.error("refusing a redirect from https to an unencrypted address")
                } else if attempt.previous().len() > MOST_REDIRECTS {
                    attempt.error("too many redirects")
                } else {
                    attempt.follow()
                }
            }))
            .build()?;

        Ok(Self {
            client,
            log: Arc::default(),
        })
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub async fn send(&self, purpose: &'static str, request: RequestBuilder) -> Result<Response, reqwest::Error> {
        let request = request.build()?;
        let method = request.method().to_string();
        let url = format!(
            "{}{}",
            request.url().origin().ascii_serialization(),
            request.url().path()
        );

        let result = self.client.execute(request).await;

        let mut log = self.log.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if log.len() == KEPT_RECORDS {
            log.pop_front();
        }
        log.push_back(OutboundRecord {
            at: now(),
            purpose,
            method,
            url,
            status: result.as_ref().ok().map(|response| response.status().as_u16()),
        });

        result
    }

    pub fn recent(&self) -> Vec<OutboundRecord> {
        self.log
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .rev()
            .cloned()
            .collect()
    }
}
