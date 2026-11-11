//! Opt-in update execution (`STP_UPDATE_MODE=apply`).
//!
//! The container is recreated from its own inspected configuration, which is passed straight back
//! to the local Docker API. That configuration is never stored, logged or sent anywhere else.

use serde_json::{json, Map, Value};

use crate::catalog::{Catalog, CatalogError};
use crate::docker::{Docker, DockerError};
use crate::scan::ServiceStatus;
use crate::triage::Verdict;
use crate::version::Version;

const PREVIOUS_SUFFIX: &str = "-stp-previous";
const COMPOSE_IMAGE_LABEL: &str = "com.docker.compose.image";

/// Settings a container inherits from its image. One that still equals the old image's value was
/// never changed by the user, so it is left out and the new image supplies its own.
const INHERITED_WHEN_UNCHANGED: [&str; 7] = [
    "Cmd",
    "Entrypoint",
    "WorkingDir",
    "User",
    "Healthcheck",
    "StopSignal",
    "Shell",
];

pub const PREVIOUS_IMAGE_GONE: &str = "The image this container was created from is no longer on this host, so the agent cannot rebuild it safely. Recreate it yourself, for example with docker compose up -d.";

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("updates are switched off; start the agent with STP_UPDATE_MODE=apply to allow them")]
    Disabled,
    #[error("no tracked service `{0}` is running")]
    UnknownService(String),
    #[error("{0}")]
    NotUpdatable(String),
    #[error("this update has manual steps; confirm you have read them before applying it")]
    NeedsAcknowledgement,
    #[error("the pulled image is {found}, not the checked {expected}; nothing was changed")]
    UnexpectedVersion { expected: String, found: String },
    #[error("the update failed and the previous container was restored: {0}")]
    RolledBack(String),
    #[error("the update failed ({cause}) and the previous container could not be fully restored: {problem}")]
    RestoreFailed { cause: String, problem: String },
    #[error(transparent)]
    Docker(#[from] DockerError),
    #[error(transparent)]
    Catalog(#[from] CatalogError),
}

#[derive(Clone)]
pub struct Updater {
    pub docker: Docker,
    pub catalog: Catalog,
    pub arch: String,
}

impl Updater {
    /// Pulls the rolling tag, verifies it is exactly the release that was assessed, then swaps the
    /// container. The image being replaced is kept as `{tag}-stp-previous`; if anything after the
    /// pull fails, the previous container is restored and the rolling tag is pointed back at it, so
    /// a later `docker compose up` does not pick up the image that was just rejected.
    pub async fn update(
        &self,
        service: &ServiceStatus,
        acknowledged: bool,
        expected_target: Option<&str>,
    ) -> Result<String, UpdateError> {
        let target = service
            .target_version
            .clone()
            .filter(|_| service.can_update)
            .ok_or_else(|| {
                UpdateError::NotUpdatable(
                    service
                        .update_note
                        .clone()
                        .unwrap_or_else(|| "The agent cannot update this service itself.".to_owned()),
                )
            })?;

        if expected_target.is_some_and(|expected| expected != target) {
            return Err(UpdateError::NotUpdatable(format!(
                "The checked target is now {target}; review it and try again."
            )));
        }
        if service.verdict == Verdict::Yellow && !acknowledged {
            return Err(UpdateError::NeedsAcknowledgement);
        }

        let container = self.docker.inspect_container(&service.container_id).await?;
        let previous_image = self.previous_image(&container).await?;
        let repository = service.image_ref.pull_name();
        let tag = service.image_ref.tag_or_latest();
        let reference = format!("{repository}:{tag}");

        let previous_tag = format!("{tag}{PREVIOUS_SUFFIX}");

        self.docker
            .tag(
                container["Image"].as_str().unwrap_or_default(),
                &repository,
                &previous_tag,
            )
            .await?;
        self.docker.pull(&repository, tag).await?;

        let applied = self
            .replace(&service.app, &container, &previous_image, &reference, &target)
            .await;

        if applied.is_err() {
            let _ = self
                .docker
                .tag(&format!("{repository}:{previous_tag}"), &repository, tag)
                .await;
        }

        applied.map(|()| target)
    }

    async fn replace(
        &self,
        app: &str,
        container: &Value,
        previous_image: &Value,
        reference: &str,
        target: &str,
    ) -> Result<(), UpdateError> {
        let pulled_image = self.verify_pulled(app, reference, target).await?;
        let spec = replacement_spec(container, previous_image, &pulled_image, reference);
        self.swap(container, &spec).await
    }

    /// Read before pulling: once the tag moves, an engine using the containerd image store no
    /// longer has the old image, and without it image defaults cannot be told from user settings.
    async fn previous_image(&self, container: &Value) -> Result<Value, UpdateError> {
        match self
            .docker
            .inspect_image(container["Image"].as_str().unwrap_or_default())
            .await
        {
            Err(DockerError::Api { status: 404, .. }) => Err(UpdateError::NotUpdatable(PREVIOUS_IMAGE_GONE.to_owned())),
            inspected => Ok(inspected?),
        }
    }

    async fn verify_pulled(&self, app: &str, reference: &str, target: &str) -> Result<Value, UpdateError> {
        let manifest = self.catalog.manifest(app).await?.value;
        let image = self.docker.inspect_image_for(reference, &self.arch).await?;

        let pulled = image["RepoDigests"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|repo_digest| repo_digest.as_str()?.rsplit_once('@'))
            .map(|(_, digest)| digest)
            .chain(image["Id"].as_str())
            .find_map(|digest| manifest.digest_map.get(digest))
            .map(|entry| entry.version.clone());

        match pulled {
            Some(found) if Version::parse(&found) == Version::parse(target) => Ok(image),
            found => Err(UpdateError::UnexpectedVersion {
                expected: target.to_owned(),
                found: found.unwrap_or_else(|| "a release that is not in the dataset yet".to_owned()),
            }),
        }
    }

    /// Replaces the container, and puts the original back under its own name, running, if any
    /// step after stopping it fails.
    async fn swap(&self, container: &Value, spec: &Value) -> Result<(), UpdateError> {
        let container_id = container["Id"].as_str().unwrap_or_default();
        let name = container["Name"]
            .as_str()
            .unwrap_or_default()
            .trim_start_matches('/')
            .to_owned();
        let parked = format!("{name}{PREVIOUS_SUFFIX}");

        // A stopped leftover from an earlier update would make the rename below fail. Docker does
        // not remove a running container without `force`, so this cannot take anything live down.
        let _ = self.docker.remove(&parked).await;

        if let Err(error) = self.docker.stop(container_id).await {
            return Err(self.restore(container_id, None, error).await);
        }
        if let Err(error) = self.docker.rename(container_id, &parked).await {
            return Err(self.restore(container_id, None, error).await);
        }

        let replaced = match self.docker.create_container(&name, spec).await {
            Ok(replacement) => match self.docker.start(&replacement).await {
                Ok(()) => Ok(()),
                Err(error) => {
                    let _ = self.docker.remove(&replacement).await;
                    Err(error)
                }
            },
            Err(error) => Err(error),
        };

        match replaced {
            Ok(()) => {
                // The replacement is running, so failing to clear the parked original is not a
                // failed update. It is cleared at the start of the next one.
                let _ = self.docker.remove(container_id).await;
                Ok(())
            }
            Err(error) => Err(self.restore(container_id, Some(&name), error).await),
        }
    }

    /// Attempts every restore step even if one fails, and reports honestly when it did not work.
    async fn restore(&self, container_id: &str, original_name: Option<&str>, cause: DockerError) -> UpdateError {
        let renamed = match original_name {
            Some(name) => self.docker.rename(container_id, name).await.err(),
            None => None,
        };
        let started = self.docker.start(container_id).await.err();

        match renamed.or(started) {
            None => UpdateError::RolledBack(cause.to_string()),
            Some(problem) => UpdateError::RestoreFailed {
                cause: cause.to_string(),
                problem: problem.to_string(),
            },
        }
    }
}

/// The create request for the replacement: the old container's settings minus everything the old
/// image contributed, so defaults baked into the new image take effect.
pub fn replacement_spec(container: &Value, previous_image: &Value, pulled_image: &Value, reference: &str) -> Value {
    let mut spec = container["Config"].as_object().cloned().unwrap_or_default();
    let defaults = &previous_image["Config"];

    if let Some(env) = spec.get_mut("Env").and_then(Value::as_array_mut) {
        let inherited = defaults["Env"].as_array().cloned().unwrap_or_default();
        env.retain(|entry| !inherited.contains(entry));
    }

    if let Some(labels) = spec.get_mut("Labels").and_then(Value::as_object_mut) {
        let inherited = defaults["Labels"].as_object().cloned().unwrap_or_default();
        labels.retain(|key, value| inherited.get(key) != Some(value));

        // Compose recreates a container whose recorded image differs from the tag's current image.
        if let (true, Some(image_id)) = (labels.contains_key(COMPOSE_IMAGE_LABEL), pulled_image["Id"].as_str()) {
            labels.insert(COMPOSE_IMAGE_LABEL.to_owned(), json!(image_id));
        }
    }

    for field in INHERITED_WHEN_UNCHANGED {
        if spec.get(field) == Some(&defaults[field]) {
            spec.remove(field);
        }
    }

    let published = container["HostConfig"]["PortBindings"].as_object();
    drop_inherited_keys(&mut spec, "ExposedPorts", &defaults["ExposedPorts"], |port| {
        published.is_some_and(|bindings| bindings.contains_key(port))
    });
    drop_inherited_keys(&mut spec, "Volumes", &defaults["Volumes"], |_| false);

    let short_id = spec
        .get("Hostname")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let shares_network_namespace = container["HostConfig"]["NetworkMode"]
        .as_str()
        .is_some_and(|mode| mode.starts_with("container:"));

    if shares_network_namespace
        || (!short_id.is_empty() && container["Id"].as_str().is_some_and(|id| id.starts_with(&short_id)))
    {
        spec.remove("Hostname");
    }

    spec.insert("Image".to_owned(), json!(reference));
    spec.insert("HostConfig".to_owned(), host_config_keeping_volumes(container));

    if !shares_network_namespace {
        spec.insert(
            "NetworkingConfig".to_owned(),
            json!({ "EndpointsConfig": endpoints(container, &short_id) }),
        );
    }

    Value::Object(spec)
}

/// Removes the entries of a map-valued setting that the old image declared, so the new image's
/// declarations apply, while keeping the ones the user added or still relies on.
fn drop_inherited_keys(
    spec: &mut Map<String, Value>,
    field: &str,
    inherited: &Value,
    still_needed: impl Fn(&str) -> bool,
) {
    let Some(entries) = spec.get_mut(field).and_then(Value::as_object_mut) else {
        return;
    };

    entries.retain(|key, _| still_needed(key) || inherited.get(key).is_none());

    if entries.is_empty() {
        spec.remove(field);
    }
}

/// Anonymous volumes are attached by name, so the replacement keeps the data of volumes that the
/// image declares or that were created with `-v /path`, as `docker compose` does on recreate.
fn host_config_keeping_volumes(container: &Value) -> Value {
    let mut host_config = container["HostConfig"].clone();
    let configured: Vec<String> = host_config["Binds"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|bind| bind.as_str()?.split(':').nth(1).map(str::to_owned))
        .chain(
            host_config["Mounts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|mount| mount["Target"].as_str().map(str::to_owned)),
        )
        .collect();

    let kept: Vec<Value> = container["Mounts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|mount| mount["Type"] == "volume")
        .filter_map(|mount| {
            let (volume, destination) = (mount["Name"].as_str()?, mount["Destination"].as_str()?);
            let mode = if mount["RW"] == false { ":ro" } else { "" };
            (!configured.iter().any(|target| target == destination))
                .then(|| json!(format!("{volume}:{destination}{mode}")))
        })
        .collect();

    if !kept.is_empty() {
        let mut binds = host_config["Binds"].as_array().cloned().unwrap_or_default();
        binds.extend(kept);
        host_config["Binds"] = Value::Array(binds);
    }

    host_config
}

fn endpoints(container: &Value, short_id: &str) -> Map<String, Value> {
    let networks = container["NetworkSettings"]["Networks"]
        .as_object()
        .cloned()
        .unwrap_or_default();

    networks
        .into_iter()
        .map(|(network, settings)| {
            let mut endpoint = Map::new();
            for field in ["IPAMConfig", "Links", "DriverOpts"] {
                if !settings[field].is_null() {
                    endpoint.insert(field.to_owned(), settings[field].clone());
                }
            }
            if let Some(aliases) = settings["Aliases"].as_array() {
                let kept: Vec<Value> = aliases
                    .iter()
                    .filter(|alias| alias.as_str() != Some(short_id))
                    .cloned()
                    .collect();
                endpoint.insert("Aliases".to_owned(), Value::Array(kept));
            }
            (network, Value::Object(endpoint))
        })
        .collect()
}
