//! Wires the pieces together and owns the state the web API and the background loop share.

use std::sync::Arc;

use serde::Serialize;
use tokio::sync::{Mutex, RwLock};

use crate::catalog::Catalog;
use crate::cloud::{Cloud, CloudError, Command, CommandOutcome, LinkStatus, Report};
use crate::config::{Config, UpdateMode};
use crate::docker::{Docker, DockerError};
use crate::host::HostFacts;
use crate::outbound::{Outbound, OutboundRecord};
use crate::scan::{Scanner, Snapshot};
use crate::update::{UpdateError, Updater};

#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error(transparent)]
    Docker(#[from] DockerError),
    #[error("could not build the HTTP client: {0}")]
    Http(#[from] reqwest::Error),
}

pub struct App {
    pub config: Config,
    pub host: HostFacts,
    pub cloud: Cloud,
    outbound: Outbound,
    scanner: Scanner,
    updater: Updater,
    snapshot: RwLock<Snapshot>,
    scanning: Mutex<()>,
    updating: Mutex<()>,
}

#[derive(Serialize)]
pub struct Status {
    pub agent_version: &'static str,
    pub update_mode: UpdateMode,
    pub remote_actions: bool,
    pub arch: String,
    pub dataset_url: String,
    pub default_cloud_url: String,
    pub snapshot: Snapshot,
    pub cloud: Option<LinkStatus>,
    pub outbound: Vec<OutboundRecord>,
}

impl App {
    pub async fn start(config: Config, host: HostFacts) -> Result<Arc<Self>, StartupError> {
        let outbound = Outbound::new()?;
        let docker = Docker::connect(&config.docker_host)?;
        let catalog = Catalog::new(outbound.clone(), &config.dataset_url, config.data_dir.join("dataset"));

        Ok(Arc::new(Self {
            cloud: Cloud::load(outbound.clone(), config.data_dir.clone()).await,
            scanner: Scanner {
                docker: docker.clone(),
                catalog: catalog.clone(),
                host: host.clone(),
                update_mode: config.update_mode,
            },
            updater: Updater { docker, catalog },
            snapshot: RwLock::default(),
            scanning: Mutex::default(),
            updating: Mutex::default(),
            outbound,
            config,
            host,
        }))
    }

    pub async fn status(&self) -> Status {
        Status {
            agent_version: env!("CARGO_PKG_VERSION"),
            update_mode: self.config.update_mode,
            remote_actions: self.config.remote_actions,
            arch: self.host.arch.clone(),
            dataset_url: self.config.dataset_url.clone(),
            default_cloud_url: self.config.cloud_url.clone(),
            snapshot: self.snapshot.read().await.clone(),
            cloud: self.cloud.status().await,
            outbound: self.outbound.recent(),
        }
    }

    pub async fn scan(&self) {
        let _one_at_a_time = self.scanning.lock().await;
        let snapshot = self.scanner.scan().await;
        *self.snapshot.write().await = snapshot;
    }

    /// One full cycle: scan, then (only when linked) report and run what the account asked for.
    pub async fn cycle(&self) {
        self.scan().await;
        self.sync_cloud().await;
    }

    pub async fn update(
        &self,
        key: &str,
        acknowledged: bool,
        expected_target: Option<&str>,
    ) -> Result<String, UpdateError> {
        if self.config.update_mode != UpdateMode::Apply {
            return Err(UpdateError::Disabled);
        }

        let _one_at_a_time = self.updating.lock().await;
        let service = self
            .snapshot
            .read()
            .await
            .services
            .iter()
            .find(|service| service.key == key)
            .cloned()
            .ok_or_else(|| UpdateError::UnknownService(key.to_owned()))?;

        let result = self.updater.update(&service, acknowledged, expected_target).await;
        self.scan().await;
        result
    }

    pub async fn sync_cloud(&self) {
        if self.cloud.status().await.is_none() {
            return;
        }

        let report = Report::new(&self.config, &self.host, &*self.snapshot.read().await);

        match self.cloud.report(&report).await {
            Ok(reply) => {
                for command in reply.commands {
                    let (outcome, message) = self.run(&command).await;
                    if let Err(error) = self.cloud.command_result(command.id, outcome, &message).await {
                        eprintln!(
                            "stp-agent: could not report the result of action {}: {error}",
                            command.id
                        );
                    }
                }
            }
            Err(CloudError::NotLinked) => {}
            Err(error) => eprintln!("stp-agent: account report failed: {error}"),
        }
    }

    /// The account can only ask. Whether anything runs is decided here, from local configuration.
    async fn run(&self, command: &Command) -> (CommandOutcome, String) {
        if !self.config.remote_actions {
            return (
                CommandOutcome::Rejected,
                "Remote actions are switched off on this host.".to_owned(),
            );
        }
        if command.kind != "update" {
            return (
                CommandOutcome::Rejected,
                format!("This agent does not support `{}` actions.", command.kind),
            );
        }

        match self
            .update(&command.service_key, true, Some(&command.target_version))
            .await
        {
            Ok(version) => (CommandOutcome::Succeeded, format!("Updated to {version}.")),
            Err(error @ (UpdateError::Disabled | UpdateError::NotUpdatable(_) | UpdateError::UnknownService(_))) => {
                (CommandOutcome::Rejected, error.to_string())
            }
            Err(error) => (CommandOutcome::Failed, error.to_string()),
        }
    }
}
