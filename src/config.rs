//! Every setting comes from an environment variable; there is no config file and no hidden default
//! that enables writes or outbound reporting.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateMode {
    Advisory,
    Apply,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub listen: SocketAddr,
    pub docker_host: String,
    pub dataset_url: String,
    pub cloud_url: String,
    pub data_dir: PathBuf,
    pub scan_interval: Duration,
    pub update_mode: UpdateMode,
    pub remote_actions: bool,
    pub api_token: Option<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid {variable}: {reason}")]
pub struct ConfigError {
    pub variable: &'static str,
    pub reason: String,
}

const MINIMUM_SCAN_INTERVAL: u64 = 30;

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|variable| std::env::var(variable).ok())
    }

    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let value = |variable: &str| {
            lookup(variable)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        let invalid = |variable: &'static str, reason: &str| ConfigError {
            variable,
            reason: reason.to_owned(),
        };

        let listen = value("STP_LISTEN")
            .unwrap_or_else(|| "0.0.0.0:8484".to_owned())
            .parse()
            .map_err(|_| invalid("STP_LISTEN", "expected an address such as 0.0.0.0:8484"))?;

        let scan_interval = match value("STP_SCAN_INTERVAL") {
            None => 300,
            Some(seconds) => seconds
                .parse::<u64>()
                .ok()
                .filter(|seconds| *seconds >= MINIMUM_SCAN_INTERVAL)
                .ok_or_else(|| invalid("STP_SCAN_INTERVAL", "expected a number of seconds, at least 30"))?,
        };

        let update_mode = match value("STP_UPDATE_MODE").as_deref() {
            None | Some("advisory") => UpdateMode::Advisory,
            Some("apply") => UpdateMode::Apply,
            Some(_) => return Err(invalid("STP_UPDATE_MODE", "expected advisory or apply")),
        };

        let remote_actions = match value("STP_REMOTE_ACTIONS").as_deref() {
            None | Some("false") => false,
            Some("true") => true,
            Some(_) => return Err(invalid("STP_REMOTE_ACTIONS", "expected true or false")),
        };

        Ok(Self {
            listen,
            docker_host: value("STP_DOCKER_HOST").unwrap_or_else(|| "unix:///var/run/docker.sock".to_owned()),
            dataset_url: base_url(value("STP_DATASET_URL")),
            cloud_url: base_url(value("STP_CLOUD_URL")),
            data_dir: value("STP_DATA_DIR").unwrap_or_else(|| "/data".to_owned()).into(),
            scan_interval: Duration::from_secs(scan_interval),
            update_mode,
            remote_actions,
            api_token: value("STP_API_TOKEN"),
        })
    }
}

fn base_url(configured: Option<String>) -> String {
    configured
        .unwrap_or_else(|| "https://safetopull.com".to_owned())
        .trim_end_matches('/')
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        Config::from_lookup(|variable| {
            pairs
                .iter()
                .find(|(name, _)| *name == variable)
                .map(|(_, value)| (*value).to_owned())
        })
    }

    #[test]
    fn defaults_are_read_only_and_local() {
        let config = config(&[]).unwrap();

        assert_eq!(config.update_mode, UpdateMode::Advisory);
        assert!(!config.remote_actions);
        assert_eq!(config.api_token, None);
        assert_eq!(config.dataset_url, "https://safetopull.com");
        assert_eq!(config.scan_interval, Duration::from_secs(300));
    }

    #[test]
    fn writes_and_remote_actions_are_explicit_opt_ins() {
        let config = config(&[("STP_UPDATE_MODE", "apply"), ("STP_REMOTE_ACTIONS", "true")]).unwrap();

        assert_eq!(config.update_mode, UpdateMode::Apply);
        assert!(config.remote_actions);
    }

    #[test]
    fn rejects_values_it_does_not_understand_instead_of_guessing() {
        for (variable, value) in [
            ("STP_UPDATE_MODE", "yolo"),
            ("STP_REMOTE_ACTIONS", "1"),
            ("STP_SCAN_INTERVAL", "5"),
            ("STP_LISTEN", "everywhere"),
        ] {
            assert_eq!(config(&[(variable, value)]).unwrap_err().variable, variable);
        }
    }

    #[test]
    fn trims_trailing_slashes_from_urls() {
        assert_eq!(
            config(&[("STP_DATASET_URL", "http://cdn.test/")]).unwrap().dataset_url,
            "http://cdn.test"
        );
    }
}
