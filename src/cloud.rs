//! The optional account link. Nothing in this module runs unless the user pairs the agent.
//!
//! [`Report`] is the complete list of what a linked agent sends. It has no field for environment
//! variables, labels, mounts, logs or containers of apps the dataset does not track.

use std::path::PathBuf;

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::RwLock;

use crate::config::{Config, UpdateMode};
use crate::host::HostFacts;
use crate::outbound::Outbound;
use crate::scan::Snapshot;
use crate::storage::{write_atomically, Visibility};
use crate::triage::Verdict;

const API: &str = "/api/agent/v1";
const LINK_FILE: &str = "cloud-link.json";

#[derive(Debug, thiserror::Error)]
pub enum CloudError {
    #[error("`{0}` is not accepted: use an https:// address (plain http only works for localhost)")]
    InsecureUrl(String),
    #[error("this agent is not linked to an account")]
    NotLinked,
    #[error("the link was revoked from the account, so this agent unlinked itself")]
    Revoked,
    #[error("{0}")]
    Rejected(String),
    #[error("the account service is unreachable: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("could not store the link: {0}")]
    Storage(#[from] std::io::Error),
}

#[derive(Clone, Serialize, Deserialize)]
struct Link {
    cloud_url: String,
    host_id: u64,
    host_name: String,
    token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LinkStatus {
    pub cloud_url: String,
    pub host_id: u64,
    pub host_name: String,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub agent_version: &'static str,
    pub arch: String,
    pub update_mode: UpdateMode,
    pub remote_actions_enabled: bool,
    pub services: Vec<ReportedService>,
}

#[derive(Debug, Serialize)]
pub struct ReportedService {
    pub key: String,
    pub name: String,
    pub stack: Option<String>,
    pub app: String,
    pub image: String,
    pub current_version: Option<String>,
    pub target_version: Option<String>,
    pub verdict: Verdict,
    pub summary: Option<String>,
    pub blocking_count: usize,
    pub warning_count: usize,
    pub can_update: bool,
}

impl Report {
    pub fn new(config: &Config, host: &HostFacts, snapshot: &Snapshot) -> Self {
        Self {
            agent_version: env!("CARGO_PKG_VERSION"),
            arch: host.arch.clone(),
            update_mode: config.update_mode,
            remote_actions_enabled: config.remote_actions,
            services: snapshot
                .services
                .iter()
                .map(|service| ReportedService {
                    key: service.key.clone(),
                    name: service.name.clone(),
                    stack: service.stack.clone(),
                    app: service.app.clone(),
                    image: service.image.clone(),
                    current_version: service.current_version.clone(),
                    target_version: service.target_version.clone(),
                    verdict: service.verdict,
                    summary: Some(format!("{}. {}", service.headline, service.summary).trim().to_owned()),
                    blocking_count: service
                        .assessment
                        .as_ref()
                        .map_or(0, |assessment| assessment.blocking_count()),
                    warning_count: service
                        .assessment
                        .as_ref()
                        .map_or(0, |assessment| assessment.warning_count()),
                    can_update: service.can_update,
                })
                .collect(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ReportReply {
    #[serde(default)]
    pub commands: Vec<Command>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Command {
    pub id: u64,
    #[serde(rename = "type")]
    pub kind: String,
    pub service_key: String,
    pub target_version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandOutcome {
    Succeeded,
    Failed,
    Rejected,
}

#[derive(Deserialize)]
struct Paired {
    host_id: u64,
    name: String,
    token: String,
}

pub struct Cloud {
    outbound: Outbound,
    link_file: PathBuf,
    link: RwLock<Option<Link>>,
}

impl Cloud {
    pub async fn load(outbound: Outbound, data_dir: impl Into<PathBuf>) -> Self {
        let link_file = data_dir.into().join(LINK_FILE);
        let link = match tokio::fs::read(&link_file).await {
            Ok(stored) => serde_json::from_slice(&stored)
                .inspect_err(|error| eprintln!("stp-agent: ignoring unreadable {}: {error}", link_file.display()))
                .ok(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                eprintln!("stp-agent: could not read {}: {error}", link_file.display());
                None
            }
        };

        Self {
            outbound,
            link_file,
            link: RwLock::new(link),
        }
    }

    pub async fn status(&self) -> Option<LinkStatus> {
        self.link.read().await.as_ref().map(|link| LinkStatus {
            cloud_url: link.cloud_url.clone(),
            host_id: link.host_id,
            host_name: link.host_name.clone(),
        })
    }

    pub async fn pair(&self, cloud_url: &str, code: &str, name: &str) -> Result<LinkStatus, CloudError> {
        let base = secure_base(cloud_url)?;
        let request = self.outbound.client().post(format!("{base}{API}/pair")).json(&json!({
            "code": code,
            "name": name,
            "agent_version": env!("CARGO_PKG_VERSION"),
        }));
        let response = self.outbound.send("link to account", request).await?;

        if !response.status().is_success() {
            return Err(CloudError::Rejected(rejection(response).await));
        }

        let paired: Paired = response.json().await?;
        let link = Link {
            cloud_url: base,
            host_id: paired.host_id,
            host_name: paired.name,
            token: paired.token,
        };

        self.store(&link).await?;
        *self.link.write().await = Some(link);

        Ok(self.status().await.expect("the link was just stored"))
    }

    /// Tells the account to delete this host, then forgets the token even if that request failed.
    pub async fn unlink(&self) -> Result<(), CloudError> {
        if let Some(link) = self.link.read().await.clone() {
            let request = self
                .outbound
                .client()
                .delete(format!("{}{API}/link", link.cloud_url))
                .bearer_auth(&link.token);
            let _ = self.outbound.send("unlink from account", request).await;
        }
        self.forget().await
    }

    pub async fn report(&self, report: &Report) -> Result<ReportReply, CloudError> {
        let link = self.link.read().await.clone().ok_or(CloudError::NotLinked)?;
        let request = self
            .outbound
            .client()
            .post(format!("{}{API}/report", link.cloud_url))
            .bearer_auth(&link.token)
            .json(report);
        let response = self.outbound.send("report to account", request).await?;

        match response.status() {
            StatusCode::UNAUTHORIZED => {
                self.forget().await?;
                Err(CloudError::Revoked)
            }
            status if status.is_success() => Ok(response.json().await?),
            _ => Err(CloudError::Rejected(rejection(response).await)),
        }
    }

    pub async fn command_result(
        &self,
        command_id: u64,
        outcome: CommandOutcome,
        message: &str,
    ) -> Result<(), CloudError> {
        let link = self.link.read().await.clone().ok_or(CloudError::NotLinked)?;
        let request = self
            .outbound
            .client()
            .post(format!("{}{API}/commands/{command_id}/result", link.cloud_url))
            .bearer_auth(&link.token)
            .json(&json!({ "status": outcome, "message": message.chars().take(500).collect::<String>() }));
        let response = self.outbound.send("report action result", request).await?;

        if response.status().is_success() {
            Ok(())
        } else {
            Err(CloudError::Rejected(rejection(response).await))
        }
    }

    async fn store(&self, link: &Link) -> Result<(), CloudError> {
        let stored = serde_json::to_vec(link).expect("a link serialises");

        Ok(write_atomically(&self.link_file, &stored, Visibility::Private).await?)
    }

    async fn forget(&self) -> Result<(), CloudError> {
        *self.link.write().await = None;

        match tokio::fs::remove_file(&self.link_file).await {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        }
    }
}

/// The host token is a bearer credential, so it is only ever sent over TLS or to this machine.
fn secure_base(cloud_url: &str) -> Result<String, CloudError> {
    let insecure = || CloudError::InsecureUrl(cloud_url.to_owned());
    let url = reqwest::Url::parse(cloud_url.trim()).map_err(|_| insecure())?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));

    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        _ => return Err(insecure()),
    }

    Ok(url.origin().ascii_serialization())
}

async fn rejection(response: reqwest::Response) -> String {
    let status = response.status().as_u16();

    response
        .json::<serde_json::Value>()
        .await
        .ok()
        .and_then(|body| {
            body.get("message")
                .and_then(|message| message.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| format!("the account service answered HTTP {status}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_tls_or_loopback_addresses_may_receive_the_token() {
        assert_eq!(
            secure_base("https://safetopull.com/").unwrap(),
            "https://safetopull.com"
        );
        assert_eq!(secure_base("http://localhost:8000").unwrap(), "http://localhost:8000");
        assert_eq!(secure_base("http://127.0.0.1:8000/x").unwrap(), "http://127.0.0.1:8000");

        for rejected in [
            "http://safetopull.com",
            "http://192.168.1.10:8000",
            "ftp://safetopull.com",
            "safetopull.com",
            "",
        ] {
            assert!(
                matches!(secure_base(rejected), Err(CloudError::InsecureUrl(_))),
                "{rejected}"
            );
        }
    }
}
