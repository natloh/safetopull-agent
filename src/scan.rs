//! One scan: list what runs, recognise tracked apps, fetch only their manifests, assess each.

use std::collections::{BTreeSet, HashMap};

use serde::Serialize;

use crate::catalog::{Catalog, Freshness};
use crate::config::UpdateMode;
use crate::docker::{Docker, DockerError};
use crate::host::HostFacts;
use crate::inventory::{self, ImageRef, RunningService};
use crate::manifest::{AppManifest, IndexedImage, Release};
use crate::outbound::now;
use crate::triage::{self, Assessment, Companion, Observed, Verdict};
use crate::update::PREVIOUS_IMAGE_GONE;
use crate::version::Version;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Snapshot {
    pub scanned_at: u64,
    pub services: Vec<ServiceStatus>,
    pub untracked_containers: usize,
    pub dataset_stale: bool,
    pub dataset_error: Option<String>,
    pub docker_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceStatus {
    pub key: String,
    pub name: String,
    pub stack: Option<String>,
    pub app: String,
    pub app_name: String,
    pub image: String,
    pub current_version: Option<String>,
    pub target_version: Option<String>,
    pub latest_version: Option<String>,
    pub verdict: Verdict,
    pub headline: String,
    pub summary: String,
    pub up_to_date: bool,
    pub can_update: bool,
    pub update_note: Option<String>,
    pub assessment: Option<Assessment>,
    #[serde(skip)]
    pub container_id: String,
    #[serde(skip)]
    pub image_ref: ImageRef,
}

pub struct Scanner {
    pub docker: Docker,
    pub catalog: Catalog,
    pub host: HostFacts,
    pub update_mode: UpdateMode,
}

impl Scanner {
    pub async fn scan(&self) -> Snapshot {
        let mut snapshot = Snapshot {
            scanned_at: now(),
            ..Snapshot::default()
        };

        let index = match self.catalog.index().await {
            Ok(index) => {
                snapshot.dataset_stale = index.freshness == Freshness::Stale;
                index.value
            }
            Err(error) => {
                snapshot.dataset_error = Some(error.to_string());
                return snapshot;
            }
        };

        let mut running = match self.docker.containers().await {
            Ok(containers) => inventory::running_services(containers),
            Err(error) => {
                snapshot.docker_error = Some(error.to_string());
                return snapshot;
            }
        };

        let mut manifests: HashMap<String, Result<AppManifest, String>> = HashMap::new();
        let mut environments: HashMap<String, BTreeSet<String>> = HashMap::new();
        let mut images: HashMap<String, (Vec<String>, bool)> = HashMap::new();

        for service in running.iter_mut().filter(|service| service.is_listed_by_image_id()) {
            if let Ok(details) = self.docker.container_details(&service.container_id).await {
                service.identify_image(&details.reference);
                environments.insert(service.container_id.clone(), details.environment_names);
            }
        }

        for service in &running {
            let Some((app, indexed)) = inventory::match_app(&index, &service.image) else {
                snapshot.untracked_containers += 1;
                continue;
            };

            if !manifests.contains_key(&app.slug) {
                let fetched = self.catalog.manifest(&app.slug).await;
                snapshot.dataset_stale |= fetched
                    .as_ref()
                    .is_ok_and(|manifest| manifest.freshness == Freshness::Stale);
                manifests.insert(
                    app.slug.clone(),
                    fetched
                        .map(|manifest| manifest.value)
                        .map_err(|error| error.to_string()),
                );
            }

            let stack: Vec<&RunningService> = running
                .iter()
                .filter(|other| {
                    other.container_id == service.container_id
                        || (service.stack.is_some() && other.stack == service.stack)
                })
                .collect();
            let mut status = ServiceStatus::unassessed(service, &app.slug, &app.name);
            let mut env_keys = BTreeSet::new();
            let mut unreadable = None;

            for member in &stack {
                if !environments.contains_key(&member.container_id) {
                    match self.docker.container_details(&member.container_id).await {
                        Ok(details) => environments.insert(member.container_id.clone(), details.environment_names),
                        Err(error) => {
                            unreadable = Some(error);
                            break;
                        }
                    };
                }
                env_keys.extend(environments[&member.container_id].iter().cloned());
            }

            if let Some(error) = unreadable {
                status.headline = "Could not be checked".to_owned();
                status.summary = format!("Its configuration could not be read, so no rule could be ruled out: {error}");
                snapshot.services.push(status);
                continue;
            }

            let companions: Vec<Companion> = stack
                .iter()
                .filter(|member| member.container_id != service.container_id)
                .map(|member| Companion {
                    service: member.name.clone(),
                    image: member.image_text.clone(),
                })
                .collect();

            match &manifests[&app.slug] {
                Err(error) => status.summary = error.clone(),
                Ok(manifest) => {
                    if !images.contains_key(&service.image_id) {
                        let inspected = self.docker.image_digests(&service.image_id).await;
                        let gone = matches!(inspected, Err(DockerError::Api { status: 404, .. }));
                        images.insert(service.image_id.clone(), (inspected.unwrap_or_default(), gone));
                    }
                    let (digests, image_gone) = &images[&service.image_id];
                    let observed = Observed {
                        env_keys: &env_keys,
                        companions: &companions,
                        arch: &self.host.arch,
                        cpu_flags: self.host.cpu_flags.as_ref(),
                        variant: &indexed.variant,
                    };
                    self.assess(&mut status, manifest, indexed, service, digests, &observed);

                    if *image_gone && status.can_update {
                        status.can_update = false;
                        status.update_note = Some(PREVIOUS_IMAGE_GONE.to_owned());
                    }
                }
            }

            snapshot.services.push(status);
        }

        snapshot.services.sort_by(|a, b| a.key.cmp(&b.key));
        snapshot
    }

    fn assess(
        &self,
        status: &mut ServiceStatus,
        manifest: &AppManifest,
        indexed: &IndexedImage,
        service: &RunningService,
        digests: &[String],
        observed: &Observed,
    ) {
        let Some(latest) = manifest.latest() else {
            status.summary = "No analysed release is published for this app yet.".to_owned();
            return;
        };
        status.latest_version = Some(latest.version.clone());

        let Some(current) = running_version(manifest, service, digests) else {
            status.headline = "Running version not recognised".to_owned();
            status.summary =
                "The image digest is not in the dataset and its tag is not a version. Pin a version tag to get a verdict.".to_owned();
            return;
        };
        status.current_version = Some(
            manifest
                .release(current)
                .map_or_else(|| format!("v{current}"), |release| release.version.clone()),
        );

        if current >= latest.parsed_version() {
            status.verdict = Verdict::Green;
            status.up_to_date = true;
            status.headline = "Up to date".to_owned();
            status.summary = "Running the newest analysed release.".to_owned();
            return;
        }

        let direct = triage::assess(manifest, current, latest, observed);
        let stone = direct
            .stepping_stones
            .first()
            .and_then(|tag| Version::parse(tag))
            .and_then(|version| manifest.release(version));
        let (target, mut assessment): (&Release, Assessment) = match stone {
            Some(stone) => (stone, triage::assess(manifest, current, stone, observed)),
            None => (latest, direct),
        };

        if stone.is_some() {
            assessment.summary = format!("{} Then continue to {}.", assessment.summary, latest.version);
        }

        let tag = service.image.tag_or_latest();
        let digest_pinned = service.image.digest.is_some();
        let rolling = !digest_pinned && is_rolling(indexed, tag);
        let updatable = matches!(assessment.verdict, Verdict::Green | Verdict::Yellow);

        status.can_update = self.update_mode == UpdateMode::Apply && updatable && rolling && stone.is_none();
        status.update_note = match (updatable, rolling, stone.is_some()) {
            (false, _, _) => None,
            (true, _, true) => Some(format!("Set the image tag to {} to take this step.", target.version)),
            (true, false, false) if digest_pinned => Some(format!(
                "Pinned by digest: change the image reference to {} and recreate the service.",
                target.version
            )),
            (true, false, false) => Some(format!(
                "Pinned to {tag}: set the image tag to {} and recreate the service.",
                target.version
            )),
            (true, true, false) if self.update_mode == UpdateMode::Advisory => None,
            (true, true, false) => Some(format!("Pulls {tag} and recreates the container.")),
        };
        status.target_version = Some(target.version.clone());
        status.verdict = assessment.verdict;
        status.headline = assessment.headline.clone();
        status.summary = assessment.summary.clone();
        status.assessment = Some(assessment);
    }
}

/// A tag moves only if the dataset lists it as rolling or it is `latest`. Where the dataset lists
/// none for an image, any tag that is not a version is assumed to move; an unlisted tag such as
/// `sha-a1b2c3d` is otherwise treated as a pin.
fn is_rolling(indexed: &IndexedImage, tag: &str) -> bool {
    indexed.rolling_tags.iter().any(|rolling| rolling == tag)
        || tag == "latest"
        || (indexed.rolling_tags.is_empty() && Version::parse(tag).is_none())
}

/// Digest first, because a tag only says what was asked for, not what is running. The image ID is
/// tried as a digest too: with the containerd image store it is the manifest digest.
fn running_version(manifest: &AppManifest, service: &RunningService, digests: &[String]) -> Option<Version> {
    let by_digest = digests
        .iter()
        .filter_map(|repo_digest| repo_digest.rsplit_once('@').map(|(_, digest)| digest))
        .chain(service.image.digest.as_deref())
        .chain([service.image_id.as_str()])
        .find_map(|digest| manifest.digest_map.get(digest))
        .and_then(|entry| Version::parse(&entry.version));

    by_digest
        .or_else(|| service.labelled_version.as_deref().and_then(Version::parse))
        .or_else(|| service.image.tag.as_deref().and_then(Version::parse))
}

impl ServiceStatus {
    fn unassessed(service: &RunningService, app: &str, app_name: &str) -> Self {
        Self {
            key: service.key.clone(),
            name: service.name.clone(),
            stack: service.stack.clone(),
            app: app.to_owned(),
            app_name: app_name.to_owned(),
            image: service.image_text.clone(),
            current_version: None,
            target_version: None,
            latest_version: None,
            verdict: Verdict::Unknown,
            headline: "Not checked".to_owned(),
            summary: String::new(),
            up_to_date: false,
            can_update: false,
            update_note: None,
            assessment: None,
            container_id: service.container_id.clone(),
            image_ref: service.image.clone(),
        }
    }
}
