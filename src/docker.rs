//! A deliberately small Docker Engine API client.
//!
//! Inspection uses three read-only calls. Container environments are reduced to variable *names*
//! while the response is parsed, and labels to the three keys in [`READ_LABELS`]; values of
//! anything else are never copied out of the response.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use serde::de::{Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::{json, Value};

pub const PROJECT_LABEL: &str = "com.docker.compose.project";
pub const SERVICE_LABEL: &str = "com.docker.compose.service";
pub const VERSION_LABEL: &str = "org.opencontainers.image.version";
pub const READ_LABELS: [&str; 3] = [PROJECT_LABEL, SERVICE_LABEL, VERSION_LABEL];

const INSPECT_TIMEOUT: Duration = Duration::from_secs(15);
const PULL_TIMEOUT: Duration = Duration::from_secs(1800);
const STOP_TIMEOUT: Duration = Duration::from_secs(120);
const LONGEST_PROGRESS_LINE: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum DockerError {
    #[error("unsupported Docker host `{0}`: use unix:///path/to/docker.sock or tcp://host:port")]
    UnsupportedHost(String),
    #[error("Docker API is unreachable: {0}")]
    Transport(String),
    #[error("Docker API refused to {operation} ({status}): {message}")]
    Api {
        operation: &'static str,
        status: u16,
        message: String,
    },
}

/// reqwest reports only "error sending request"; the cause underneath (connection refused,
/// permission denied on the socket) is what tells the user what to fix.
impl From<reqwest::Error> for DockerError {
    fn from(error: reqwest::Error) -> Self {
        let mut description = error.to_string();
        let mut cause = std::error::Error::source(&error);

        while let Some(inner) = cause {
            description.push_str(": ");
            description.push_str(&inner.to_string());
            cause = inner.source();
        }

        Self::Transport(description)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerSummary {
    pub id: String,
    #[serde(default)]
    pub names: Vec<String>,
    pub image: String,
    #[serde(rename = "ImageID")]
    pub image_id: String,
    #[serde(default, deserialize_with = "allowed_labels")]
    pub labels: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ContainerEnvironment {
    config: EnvironmentNames,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct EnvironmentNames {
    #[serde(default)]
    image: String,
    #[serde(default, deserialize_with = "env_names")]
    env: BTreeSet<String>,
}

pub struct ContainerDetails {
    /// The image reference the container was created with, which survives the tag moving.
    pub reference: String,
    pub environment_names: BTreeSet<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ImageDigests {
    #[serde(default)]
    repo_digests: Option<Vec<String>>,
}

#[derive(Clone)]
pub struct Docker {
    client: reqwest::Client,
    base: String,
}

impl Docker {
    pub fn connect(host: &str) -> Result<Self, DockerError> {
        let builder = reqwest::Client::builder();

        if let Some(address) = host.strip_prefix("tcp://").or_else(|| host.strip_prefix("http://")) {
            return Ok(Self {
                client: builder.build()?,
                base: format!("http://{}", address.trim_end_matches('/')),
            });
        }

        #[cfg(unix)]
        if let Some(path) = host.strip_prefix("unix://") {
            return Ok(Self {
                client: builder.unix_socket(path).build()?,
                base: "http://localhost".to_owned(),
            });
        }

        Err(DockerError::UnsupportedHost(host.to_owned()))
    }

    pub async fn containers(&self) -> Result<Vec<ContainerSummary>, DockerError> {
        let response = self
            .client
            .get(self.url("/containers/json"))
            .timeout(INSPECT_TIMEOUT)
            .send()
            .await?;
        Ok(checked("list containers", response).await?.json().await?)
    }

    pub async fn container_details(&self, container_id: &str) -> Result<ContainerDetails, DockerError> {
        let response = self
            .client
            .get(self.url(&format!("/containers/{container_id}/json")))
            .timeout(INSPECT_TIMEOUT)
            .send()
            .await?;
        let container: ContainerEnvironment = checked("inspect a container", response).await?.json().await?;
        Ok(ContainerDetails {
            reference: container.config.image,
            environment_names: container.config.env,
        })
    }

    pub async fn image_digests(&self, image: &str) -> Result<Vec<String>, DockerError> {
        let response = self
            .client
            .get(self.url(&format!("/images/{image}/json")))
            .timeout(INSPECT_TIMEOUT)
            .send()
            .await?;
        let image: ImageDigests = checked("inspect an image", response).await?.json().await?;
        Ok(image.repo_digests.unwrap_or_default())
    }

    pub async fn pull(&self, repository: &str, tag: &str) -> Result<(), DockerError> {
        let response = self
            .client
            .post(self.url("/images/create"))
            .query(&[("fromImage", repository), ("tag", tag)])
            .timeout(PULL_TIMEOUT)
            .send()
            .await?;
        let mut progress = checked("pull an image", response).await?;
        let mut line = Vec::new();

        while let Some(chunk) = progress.chunk().await? {
            for byte in chunk {
                if byte == b'\n' {
                    pull_failure(&line)?;
                    line.clear();
                } else if line.len() < LONGEST_PROGRESS_LINE {
                    line.push(byte);
                }
            }
        }

        pull_failure(&line)
    }

    pub async fn inspect_container(&self, container_id: &str) -> Result<Value, DockerError> {
        let response = self
            .client
            .get(self.url(&format!("/containers/{container_id}/json")))
            .timeout(INSPECT_TIMEOUT)
            .send()
            .await?;
        Ok(checked("inspect a container", response).await?.json().await?)
    }

    pub async fn inspect_image(&self, image: &str) -> Result<Value, DockerError> {
        let response = self
            .client
            .get(self.url(&format!("/images/{image}/json")))
            .timeout(INSPECT_TIMEOUT)
            .send()
            .await?;
        Ok(checked("inspect an image", response).await?.json().await?)
    }

    /// Inspects the image as one platform sees it. On engines that keep multi-platform images its
    /// `Id` is that platform's manifest digest, which is the identity Docker Compose records.
    pub async fn inspect_image_for(&self, image: &str, architecture: &str) -> Result<Value, DockerError> {
        let platform = json!({ "os": "linux", "architecture": architecture }).to_string();
        let response = self
            .client
            .get(self.url(&format!("/images/{image}/json")))
            .query(&[("platform", platform)])
            .timeout(INSPECT_TIMEOUT)
            .send()
            .await?;
        Ok(checked("inspect an image", response).await?.json().await?)
    }

    pub async fn create_container(&self, name: &str, spec: &Value) -> Result<String, DockerError> {
        let response = self
            .client
            .post(self.url("/containers/create"))
            .query(&[("name", name)])
            .json(spec)
            .timeout(INSPECT_TIMEOUT)
            .send()
            .await?;
        let created: Value = checked("create a container", response).await?.json().await?;
        Ok(created.get("Id").and_then(Value::as_str).unwrap_or_default().to_owned())
    }

    pub async fn stop(&self, container_id: &str) -> Result<(), DockerError> {
        self.command(
            "stop a container",
            self.client.post(self.url(&format!("/containers/{container_id}/stop"))),
        )
        .await
    }

    pub async fn start(&self, container_id: &str) -> Result<(), DockerError> {
        self.command(
            "start a container",
            self.client.post(self.url(&format!("/containers/{container_id}/start"))),
        )
        .await
    }

    pub async fn rename(&self, container_id: &str, name: &str) -> Result<(), DockerError> {
        let request = self
            .client
            .post(self.url(&format!("/containers/{container_id}/rename")))
            .query(&[("name", name)]);
        self.command("rename a container", request).await
    }

    pub async fn tag(&self, image: &str, repository: &str, tag: &str) -> Result<(), DockerError> {
        let request = self
            .client
            .post(self.url(&format!("/images/{image}/tag")))
            .query(&[("repo", repository), ("tag", tag)]);
        self.command("tag an image", request).await
    }

    pub async fn remove(&self, container_id: &str) -> Result<(), DockerError> {
        self.command(
            "remove a container",
            self.client.delete(self.url(&format!("/containers/{container_id}"))),
        )
        .await
    }

    async fn command(&self, operation: &'static str, request: reqwest::RequestBuilder) -> Result<(), DockerError> {
        checked(operation, request.timeout(STOP_TIMEOUT).send().await?)
            .await
            .map(|_| ())
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }
}

/// Docker reports a failed pull as an `error` line inside a successful response.
fn pull_failure(progress_line: &[u8]) -> Result<(), DockerError> {
    let message = serde_json::from_slice::<Value>(progress_line)
        .ok()
        .and_then(|event| event.get("error").and_then(Value::as_str).map(str::to_owned));

    match message {
        Some(message) => Err(DockerError::Api {
            operation: "pull an image",
            status: 200,
            message,
        }),
        None => Ok(()),
    }
}

async fn checked(operation: &'static str, response: reqwest::Response) -> Result<reqwest::Response, DockerError> {
    let status = response.status();

    if status.is_success() || status.as_u16() == 304 {
        return Ok(response);
    }

    let body = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|json| json.get("message").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or(body);

    Err(DockerError::Api {
        operation,
        status: status.as_u16(),
        message,
    })
}

struct EnvName(String);

impl<'de> Deserialize<'de> for EnvName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct NameOnly;

        impl Visitor<'_> for NameOnly {
            type Value = EnvName;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a NAME=value string")
            }

            fn visit_str<E>(self, entry: &str) -> Result<EnvName, E> {
                Ok(EnvName(entry.split('=').next().unwrap_or_default().to_owned()))
            }
        }

        deserializer.deserialize_str(NameOnly)
    }
}

fn env_names<'de, D: Deserializer<'de>>(deserializer: D) -> Result<BTreeSet<String>, D::Error> {
    struct Names;

    impl<'de> Visitor<'de> for Names {
        type Value = BTreeSet<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a list of NAME=value strings or null")
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(BTreeSet::new())
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut entries: A) -> Result<Self::Value, A::Error> {
            let mut names = BTreeSet::new();
            while let Some(EnvName(name)) = entries.next_element()? {
                if !name.is_empty() {
                    names.insert(name);
                }
            }
            Ok(names)
        }
    }

    deserializer.deserialize_any(Names)
}

fn allowed_labels<'de, D: Deserializer<'de>>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error> {
    struct Allowed;

    impl<'de> Visitor<'de> for Allowed {
        type Value = BTreeMap<String, String>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a map of labels or null")
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(BTreeMap::new())
        }

        fn visit_map<A: MapAccess<'de>>(self, mut labels: A) -> Result<Self::Value, A::Error> {
            let mut kept = BTreeMap::new();
            while let Some(key) = labels.next_key::<String>()? {
                if READ_LABELS.contains(&key.as_str()) {
                    kept.insert(key, labels.next_value()?);
                } else {
                    labels.next_value::<IgnoredAny>()?;
                }
            }
            Ok(kept)
        }
    }

    deserializer.deserialize_any(Allowed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_values_never_survive_parsing() {
        let container: ContainerEnvironment = serde_json::from_str(
            r#"{"Config": {"Env": ["DB_PASSWORD=hunter2", "DB_HOSTNAME=db", "EMPTY=", "URL=postgres://u:p@h/db?x=1", "BARE"]}}"#,
        )
        .unwrap();

        assert_eq!(
            container.config.env.iter().map(String::as_str).collect::<Vec<_>>(),
            ["BARE", "DB_HOSTNAME", "DB_PASSWORD", "EMPTY", "URL"]
        );
        assert!(!format!("{:?}", container.config.env).contains("hunter2"));
    }

    #[test]
    fn a_container_without_environment_has_no_names() {
        let container: ContainerEnvironment = serde_json::from_str(r#"{"Config": {"Env": null}}"#).unwrap();

        assert!(container.config.env.is_empty());
    }

    #[test]
    fn only_allow_listed_labels_are_kept() {
        let container: ContainerSummary = serde_json::from_str(
            r#"{
                "Id": "abc", "Names": ["/immich_server"], "Image": "ghcr.io/immich-app/immich-server:release",
                "ImageID": "sha256:feed", "State": "running", "Command": "start.sh --secret=nope",
                "Labels": {
                    "com.docker.compose.project": "media",
                    "com.docker.compose.service": "immich-server",
                    "traefik.http.middlewares.auth.basicauth.users": "admin:$apr1$secret",
                    "org.opencontainers.image.version": "v1.132.0"
                }
            }"#,
        )
        .unwrap();

        assert_eq!(
            container.labels.keys().map(String::as_str).collect::<Vec<_>>(),
            READ_LABELS
        );
        assert!(!format!("{container:?}").contains("secret"));
    }

    #[test]
    fn rejects_docker_hosts_it_cannot_reach() {
        assert!(matches!(
            Docker::connect("ssh://nas"),
            Err(DockerError::UnsupportedHost(_))
        ));
        assert!(Docker::connect("tcp://docker-proxy:2375").is_ok());
    }
}
