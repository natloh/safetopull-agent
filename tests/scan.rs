mod support;

use stp_agent::catalog::{Catalog, CatalogError, Freshness};
use stp_agent::config::UpdateMode;
use stp_agent::docker::Docker;
use stp_agent::host::HostFacts;
use stp_agent::outbound::Outbound;
use stp_agent::scan::{Scanner, ServiceStatus, Snapshot};
use stp_agent::triage::Verdict;
use support::{FakeContainer, FakeDataset, FakeDocker};

const IMMICH_V1_132: &str =
    "ghcr.io/immich-app/immich-server@sha256:1111111111111111111111111111111111111111111111111111111111111111";

struct Lab {
    dataset: FakeDataset,
    docker: FakeDocker,
    cache: tempfile::TempDir,
    outbound: Outbound,
}

impl Lab {
    fn new() -> Self {
        Self {
            dataset: FakeDataset::with_fixtures(),
            docker: FakeDocker::default(),
            cache: tempfile::tempdir().unwrap(),
            outbound: Outbound::new().unwrap(),
        }
    }

    async fn catalog(&self) -> Catalog {
        Catalog::new(self.outbound.clone(), &self.dataset.start().await, self.cache.path())
    }

    async fn scan(&self, update_mode: UpdateMode) -> Snapshot {
        let scanner = Scanner {
            docker: Docker::connect(&self.docker.start().await).unwrap(),
            catalog: self.catalog().await,
            host: HostFacts {
                arch: "amd64".to_owned(),
                cpu_flags: Some(["avx2".to_owned()].into()),
            },
            update_mode,
        };
        scanner.scan().await
    }

    fn run_immich(&self, image: &str, repo_digests: &[&str], env: &[&str]) {
        self.docker.run(FakeContainer {
            id: "immich1",
            name: "immich-server",
            image,
            image_id: "sha256:immichimage",
            stack: Some("media"),
            env,
            repo_digests,
        });
    }
}

fn only(snapshot: &Snapshot) -> &ServiceStatus {
    assert_eq!(snapshot.services.len(), 1, "{snapshot:?}");
    &snapshot.services[0]
}

#[tokio::test]
async fn identifies_the_running_version_by_digest_and_holds_back_a_breaking_update() {
    let lab = Lab::new();
    lab.run_immich(
        "ghcr.io/immich-app/immich-server:release",
        &[IMMICH_V1_132],
        &["DB_HOSTNAME=db", "DB_PASSWORD=hunter2"],
    );
    lab.docker.run(FakeContainer {
        id: "db1",
        name: "database",
        image: "tensorchord/pgvecto-rs:pg14-v0.2.0",
        image_id: "sha256:dbimage",
        stack: Some("media"),
        env: &["POSTGRES_PASSWORD=hunter2"],
        repo_digests: &[],
    });

    let snapshot = lab.scan(UpdateMode::Apply).await;
    let immich = only(&snapshot);

    assert_eq!(immich.key, "media/immich-server");
    assert_eq!(immich.current_version.as_deref(), Some("v1.132.0"));
    assert_eq!(immich.latest_version.as_deref(), Some("v2.0.0"));
    assert_eq!(immich.target_version.as_deref(), Some("v1.133.0"));
    assert_eq!(immich.verdict, Verdict::Green);
    assert!(immich.summary.ends_with("Then continue to v2.0.0."));
    assert!(!immich.can_update, "a rolling tag cannot land on a stepping stone");
    assert_eq!(snapshot.untracked_containers, 1);
    assert!(!serde_json::to_string(&snapshot).unwrap().contains("hunter2"));
}

#[tokio::test]
async fn applies_conditional_rules_using_companions_and_environment_names_from_the_same_stack() {
    let lab = Lab::new();
    lab.run_immich("ghcr.io/immich-app/immich-server:v1.133.0", &[], &["DB_HOSTNAME=db"]);
    lab.docker.run(FakeContainer {
        id: "db1",
        name: "database",
        image: "tensorchord/pgvecto-rs:pg14-v0.2.0",
        image_id: "sha256:dbimage",
        stack: Some("media"),
        env: &[],
        repo_digests: &[],
    });

    let snapshot = lab.scan(UpdateMode::Advisory).await;
    let immich = only(&snapshot);
    let slugs: Vec<&str> = immich
        .assessment
        .as_ref()
        .unwrap()
        .findings
        .iter()
        .map(|finding| finding.slug.as_str())
        .collect();

    assert_eq!(immich.verdict, Verdict::Red);
    assert_eq!(
        slugs,
        [
            "immich-v2-vectorchord-swap",
            "immich-v2-volume-target",
            "immich-v2-env-postgres-host"
        ]
    );
    assert!(!immich.can_update);
}

#[tokio::test]
async fn offers_an_update_only_in_apply_mode_for_a_rolling_tag() {
    let run = |lab: &Lab, image: &str| {
        lab.docker.run(FakeContainer {
            id: "nc1",
            name: "nextcloud",
            image,
            image_id: "sha256:ncimage",
            stack: None,
            env: &[],
            repo_digests: &["nextcloud@sha256:nextcloud29index"],
        });
    };

    let rolling = Lab::new();
    run(&rolling, "nextcloud:latest");
    let applied = rolling.scan(UpdateMode::Apply).await;
    assert_eq!(only(&applied).verdict, Verdict::Yellow);
    assert_eq!(only(&applied).target_version.as_deref(), Some("v30.0.0"));
    assert!(only(&applied).can_update);
    assert!(!only(&rolling.scan(UpdateMode::Advisory).await).can_update);

    let pinned = Lab::new();
    run(&pinned, "nextcloud:29.0.0");
    let status = pinned.scan(UpdateMode::Apply).await;
    assert!(!only(&status).can_update);
    assert_eq!(
        only(&status).update_note.as_deref(),
        Some("Pinned to 29.0.0: set the image tag to v30.0.0 and recreate the service.")
    );
}

#[tokio::test]
async fn reports_up_to_date_and_unrecognised_versions_without_guessing() {
    let current = Lab::new();
    current.run_immich("ghcr.io/immich-app/immich-server:v2.0.0", &[], &[]);
    let snapshot = current.scan(UpdateMode::Apply).await;
    assert!(only(&snapshot).up_to_date);
    assert_eq!(only(&snapshot).verdict, Verdict::Green);
    assert!(!only(&snapshot).can_update);

    let unknown = Lab::new();
    unknown.run_immich(
        "ghcr.io/immich-app/immich-server:release",
        &["ghcr.io/immich-app/immich-server@sha256:notindataset"],
        &[],
    );
    let snapshot = unknown.scan(UpdateMode::Apply).await;
    assert_eq!(only(&snapshot).verdict, Verdict::Unknown);
    assert_eq!(only(&snapshot).current_version, None);
    assert!(!only(&snapshot).can_update);
}

#[tokio::test]
async fn fetches_only_the_index_and_the_manifests_of_apps_that_run() {
    let lab = Lab::new();
    lab.run_immich("ghcr.io/immich-app/immich-server:v1.132.0", &[], &[]);

    lab.scan(UpdateMode::Advisory).await;

    let paths: Vec<String> = lab.dataset.requests().into_iter().map(|(path, _)| path).collect();
    assert_eq!(paths, ["v1/index.json", "v1/apps/immich.json"]);
    assert!(lab
        .outbound
        .recent()
        .iter()
        .all(|record| record.method == "GET" && record.status == Some(200)));
}

#[tokio::test]
async fn a_docker_or_dataset_outage_is_reported_instead_of_showing_an_empty_healthy_host() {
    let no_docker = Lab::new();
    let scanner = Scanner {
        docker: Docker::connect("tcp://127.0.0.1:9").unwrap(),
        catalog: no_docker.catalog().await,
        host: HostFacts {
            arch: "amd64".to_owned(),
            cpu_flags: None,
        },
        update_mode: UpdateMode::Advisory,
    };
    let docker_error = scanner.scan().await.docker_error.unwrap();
    assert!(
        docker_error.contains("os error"),
        "the root cause must be shown: {docker_error}"
    );

    let no_dataset = Lab::new();
    no_dataset.dataset.fail(true);
    assert!(no_dataset.scan(UpdateMode::Advisory).await.dataset_error.is_some());
}

#[tokio::test]
async fn revalidates_with_etags_and_survives_an_unreachable_dataset_from_cache() {
    let lab = Lab::new();
    let catalog = lab.catalog().await;

    assert_eq!(
        catalog.manifest("immich").await.unwrap().freshness,
        Freshness::Downloaded
    );
    assert_eq!(
        catalog.manifest("immich").await.unwrap().freshness,
        Freshness::Revalidated
    );

    let requests = lab.dataset.requests();
    assert_eq!(requests[0].1, None);
    assert!(requests[1].1.is_some(), "the second request must be conditional");

    lab.dataset.fail(true);
    let offline = catalog.manifest("immich").await.unwrap();
    assert_eq!(offline.freshness, Freshness::Stale);
    assert_eq!(offline.value.releases.len(), 4);

    lab.dataset.fail(false);
    lab.dataset
        .put("v1/apps/immich.json", &support::fixture("nextcloud.json"));
    let changed = catalog.manifest("immich").await.unwrap();
    assert_eq!(changed.freshness, Freshness::Downloaded);
    assert_eq!(changed.value.releases.len(), 3);
}

#[tokio::test]
async fn never_caches_a_malformed_file_and_rejects_slugs_that_could_escape_the_cache() {
    let lab = Lab::new();
    let catalog = lab.catalog().await;
    catalog.manifest("immich").await.unwrap();

    lab.dataset.put("v1/apps/immich.json", "<html>captive portal</html>");
    assert!(matches!(
        catalog.manifest("immich").await,
        Err(CatalogError::Malformed { .. })
    ));

    lab.dataset.fail(true);
    assert_eq!(catalog.manifest("immich").await.unwrap().value.releases.len(), 4);

    assert!(matches!(
        catalog.manifest("../../etc/passwd").await,
        Err(CatalogError::InvalidSlug(_))
    ));
    assert!(matches!(
        catalog.manifest("unknown-app").await,
        Err(CatalogError::Unavailable { .. })
    ));
}

#[tokio::test]
async fn recognises_a_container_that_docker_lists_by_image_id_after_its_tag_moved() {
    let lab = Lab::new();
    lab.docker.run(FakeContainer {
        id: "immich1",
        name: "immich-server",
        image: "ghcr.io/immich-app/immich-server:release",
        image_id: "sha256:1111111111111111111111111111111111111111111111111111111111111111",
        stack: Some("media"),
        env: &["DB_HOSTNAME=db"],
        repo_digests: &[],
    });
    lab.docker.move_tag_away_from("immich1");

    let snapshot = lab.scan(UpdateMode::Advisory).await;
    let immich = only(&snapshot);

    assert_eq!(immich.image, "ghcr.io/immich-app/immich-server:release");
    assert_eq!(immich.current_version.as_deref(), Some("v1.132.0"));
    assert_eq!(snapshot.untracked_containers, 0);
}

fn run_nextcloud(lab: &Lab, id: &str, image: &str, repo_digests: &[&str]) {
    lab.docker.run(FakeContainer {
        id,
        name: id,
        image,
        image_id: "sha256:ncimage",
        stack: None,
        env: &[],
        repo_digests,
    });
}

#[tokio::test]
async fn a_digest_pinned_container_is_never_treated_as_a_rolling_tag() {
    let lab = Lab::new();
    run_nextcloud(&lab, "nc1", "nextcloud:latest@sha256:nextcloud29index", &[]);

    let snapshot = lab.scan(UpdateMode::Apply).await;
    let nextcloud = only(&snapshot);

    assert_eq!(nextcloud.target_version.as_deref(), Some("v30.0.0"));
    assert!(!nextcloud.can_update);
    assert!(nextcloud
        .update_note
        .as_deref()
        .unwrap()
        .starts_with("Pinned by digest"));
}

#[tokio::test]
async fn a_tag_the_dataset_does_not_list_as_rolling_is_treated_as_a_pin() {
    let lab = Lab::new();
    run_nextcloud(
        &lab,
        "nc1",
        "nextcloud:sha-a1b2c3d",
        &["nextcloud@sha256:nextcloud29index"],
    );

    let snapshot = lab.scan(UpdateMode::Apply).await;

    assert!(!only(&snapshot).can_update);
    assert!(only(&snapshot)
        .update_note
        .as_deref()
        .unwrap()
        .starts_with("Pinned to sha-a1b2c3d"));
}

#[tokio::test]
async fn a_service_whose_configuration_cannot_be_read_gets_no_verdict() {
    let lab = Lab::new();
    lab.run_immich("ghcr.io/immich-app/immich-server:v1.133.0", &[], &["DB_HOSTNAME=db"]);
    lab.docker.fail("inspect immich1");

    let snapshot = lab.scan(UpdateMode::Apply).await;
    let immich = only(&snapshot);

    assert_eq!(immich.verdict, Verdict::Unknown);
    assert_eq!(immich.headline, "Could not be checked");
    assert!(!immich.can_update);
}

#[tokio::test]
async fn inspects_an_image_once_however_many_containers_run_it() {
    let lab = Lab::new();
    run_nextcloud(&lab, "nc1", "nextcloud:latest", &["nextcloud@sha256:nextcloud29index"]);
    run_nextcloud(&lab, "nc2", "nextcloud:latest", &["nextcloud@sha256:nextcloud29index"]);

    let snapshot = lab.scan(UpdateMode::Advisory).await;

    assert_eq!(snapshot.services.len(), 2);
    let inspections = lab
        .docker
        .calls()
        .iter()
        .filter(|call| *call == "image sha256:ncimage")
        .count();
    assert_eq!(inspections, 1);
}

#[tokio::test]
async fn refuses_a_dataset_file_far_larger_than_any_real_one() {
    let lab = Lab::new();
    let catalog = lab.catalog().await;
    catalog.manifest("immich").await.unwrap();

    lab.dataset.put("v1/apps/immich.json", &" ".repeat(9 * 1024 * 1024));
    assert_eq!(catalog.manifest("immich").await.unwrap().freshness, Freshness::Stale);

    lab.dataset.put("v1/apps/nextcloud.json", &" ".repeat(9 * 1024 * 1024));
    assert!(matches!(
        catalog.manifest("nextcloud").await,
        Err(CatalogError::Unavailable { .. })
    ));
}
