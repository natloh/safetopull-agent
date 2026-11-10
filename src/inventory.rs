//! Maps running containers onto the apps the dataset tracks.

use crate::docker::{ContainerSummary, PROJECT_LABEL, SERVICE_LABEL, VERSION_LABEL};
use crate::manifest::{AppIndex, IndexedApp, IndexedImage};

const DOCKER_HUB: &str = "registry-1.docker.io";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub registry: String,
    pub repository: String,
    pub tag: Option<String>,
    pub digest: Option<String>,
}

impl ImageRef {
    pub fn parse(reference: &str) -> Self {
        let (name, digest) = match reference.split_once('@') {
            Some((name, digest)) => (name, Some(digest.to_owned())),
            None => (reference, None),
        };

        let (name, tag) = match name.rsplit_once(':') {
            Some((name, tag)) if !tag.contains('/') => (name, Some(tag.to_owned())),
            _ => (name, None),
        };

        let (registry, repository) = match name.split_once('/') {
            Some((host, path)) if host.contains(['.', ':']) || host == "localhost" => (canonical_registry(host), path),
            _ => (DOCKER_HUB, name),
        };
        let repository = match registry {
            DOCKER_HUB if !repository.contains('/') => format!("library/{repository}"),
            _ => repository.to_owned(),
        };

        Self {
            registry: registry.to_owned(),
            repository,
            tag,
            digest,
        }
    }

    /// The reference Docker pulls from, without tag or digest.
    pub fn pull_name(&self) -> String {
        format!(
            "{}/{}",
            if self.registry == DOCKER_HUB {
                "docker.io"
            } else {
                &self.registry
            },
            self.repository
        )
    }

    pub fn tag_or_latest(&self) -> &str {
        self.tag.as_deref().unwrap_or("latest")
    }
}

fn canonical_registry(host: &str) -> &str {
    match host {
        "docker.io" | "index.docker.io" | "registry.hub.docker.com" => DOCKER_HUB,
        other => other,
    }
}

#[derive(Debug, Clone)]
pub struct RunningService {
    pub container_id: String,
    pub key: String,
    pub name: String,
    pub stack: Option<String>,
    pub image_text: String,
    pub image: ImageRef,
    pub image_id: String,
    pub labelled_version: Option<String>,
}

pub fn running_services(containers: Vec<ContainerSummary>) -> Vec<RunningService> {
    containers
        .into_iter()
        .map(|container| {
            let container_name = container
                .names
                .first()
                .map(|name| name.trim_start_matches('/').to_owned())
                .unwrap_or_else(|| container.id.clone());
            let stack = container.labels.get(PROJECT_LABEL).cloned();
            let name = container.labels.get(SERVICE_LABEL).cloned().unwrap_or(container_name);
            let key = match &stack {
                Some(stack) => format!("{stack}/{name}"),
                None => name.clone(),
            };
            let image = ImageRef::parse(&container.image);

            RunningService {
                container_id: container.id,
                key,
                name,
                stack,
                image_text: container.image.split('@').next().unwrap_or_default().to_owned(),
                image,
                image_id: container.image_id,
                labelled_version: container.labels.get(VERSION_LABEL).cloned(),
            }
        })
        .collect()
}

/// An exact registry and repository match wins; otherwise a repository that only one tracked app
/// uses still identifies it, which covers registry mirrors.
pub fn match_app<'a>(index: &'a AppIndex, image: &ImageRef) -> Option<(&'a IndexedApp, &'a IndexedImage)> {
    let candidates: Vec<(&IndexedApp, &IndexedImage)> = index
        .apps
        .iter()
        .flat_map(|app| app.images.iter().map(move |indexed| (app, indexed)))
        .filter(|(_, indexed)| indexed.repository == image.repository)
        .collect();

    candidates
        .iter()
        .find(|(_, indexed)| canonical_registry(&indexed.registry) == image.registry)
        .or_else(|| candidates.first().filter(|_| candidates.len() == 1))
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(reference: &str) -> (String, String, Option<String>) {
        let image = ImageRef::parse(reference);
        (image.registry, image.repository, image.tag)
    }

    fn owned(registry: &str, repository: &str, tag: Option<&str>) -> (String, String, Option<String>) {
        (registry.to_owned(), repository.to_owned(), tag.map(str::to_owned))
    }

    #[test]
    fn parses_the_reference_shapes_docker_accepts() {
        assert_eq!(parsed("postgres"), owned(DOCKER_HUB, "library/postgres", None));
        assert_eq!(parsed("postgres:16"), owned(DOCKER_HUB, "library/postgres", Some("16")));
        assert_eq!(
            parsed("docker.io/traefik:v3.1"),
            owned(DOCKER_HUB, "library/traefik", Some("v3.1"))
        );
        assert_eq!(
            parsed("linuxserver/nextcloud:latest"),
            owned(DOCKER_HUB, "linuxserver/nextcloud", Some("latest"))
        );
        assert_eq!(
            parsed("ghcr.io/immich-app/immich-server:release"),
            owned("ghcr.io", "immich-app/immich-server", Some("release"))
        );
        assert_eq!(
            parsed("localhost:5000/team/app"),
            owned("localhost:5000", "team/app", None)
        );
    }

    #[test]
    fn keeps_a_pinned_digest_apart_from_the_tag() {
        let image = ImageRef::parse("ghcr.io/immich-app/immich-server:v1.132.0@sha256:1111");

        assert_eq!(image.tag.as_deref(), Some("v1.132.0"));
        assert_eq!(image.digest.as_deref(), Some("sha256:1111"));
        assert_eq!(image.pull_name(), "ghcr.io/immich-app/immich-server");
        assert_eq!(ImageRef::parse("postgres").pull_name(), "docker.io/library/postgres");
    }

    fn index() -> AppIndex {
        serde_json::from_value(serde_json::json!({"apps": [
            {"slug": "immich", "name": "Immich", "images": [
                {"variant": "official", "registry": "ghcr.io", "repository": "immich-app/immich-server", "rolling_tags": ["release"]}
            ]},
            {"slug": "nextcloud", "name": "Nextcloud", "images": [
                {"variant": "official", "registry": "registry-1.docker.io", "repository": "library/nextcloud"},
                {"variant": "linuxserver", "registry": "lscr.io", "repository": "linuxserver/nextcloud"}
            ]},
            {"slug": "fork", "name": "Fork", "images": [
                {"variant": "official", "registry": "quay.io", "repository": "library/nextcloud"}
            ]}
        ]}))
        .unwrap()
    }

    fn matched(reference: &str) -> Option<(String, String)> {
        let index = index();
        match_app(&index, &ImageRef::parse(reference)).map(|(app, image)| (app.slug.clone(), image.variant.clone()))
    }

    #[test]
    fn matches_images_to_tracked_apps_and_variants() {
        assert_eq!(
            matched("ghcr.io/immich-app/immich-server:release"),
            Some(("immich".into(), "official".into()))
        );
        assert_eq!(matched("nextcloud:29"), Some(("nextcloud".into(), "official".into())));
        assert_eq!(
            matched("lscr.io/linuxserver/nextcloud"),
            Some(("nextcloud".into(), "linuxserver".into()))
        );
        assert_eq!(matched("redis:7"), None);
    }

    #[test]
    fn a_mirror_matches_only_when_the_repository_is_unambiguous() {
        assert_eq!(
            matched("mirror.lan/immich-app/immich-server:release"),
            Some(("immich".into(), "official".into()))
        );
        assert_eq!(matched("mirror.lan/library/nextcloud:29"), None);
    }
}
