//! Turns a release range plus what was observed on the host into a verdict and ordered steps.
//!
//! Mirrors section 5 of the dataset specification. Anything the agent cannot verify is counted
//! against the update, never for it.

use std::collections::BTreeSet;

use serde::Serialize;

use crate::manifest::{Action, AppManifest, Condition, Release, Rule, Severity};
use crate::version::Version;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Green,
    Yellow,
    Red,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Companion {
    pub service: String,
    pub image: String,
}

/// What the agent could observe for one service. Environment variables are names only.
pub struct Observed<'a> {
    pub env_keys: &'a BTreeSet<String>,
    pub companions: &'a [Companion],
    pub arch: &'a str,
    pub cpu_flags: Option<&'a BTreeSet<String>>,
    pub variant: &'a str,
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub release: String,
    pub slug: String,
    pub severity: Severity,
    pub category: String,
    pub explanation: String,
    pub instruction: Option<String>,
    pub command: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Step {
    pub title: String,
    pub detail: Option<String>,
    pub command: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Assessment {
    pub verdict: Verdict,
    pub headline: String,
    pub summary: String,
    pub releases_checked: usize,
    pub stepping_stones: Vec<String>,
    pub database_migration: bool,
    pub unverified_cpu_flags: Vec<String>,
    pub findings: Vec<Finding>,
    pub steps: Vec<Step>,
}

impl Assessment {
    pub fn blocking_count(&self) -> usize {
        self.findings.iter().filter(|finding| finding.severity.blocks()).count()
    }

    pub fn warning_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|finding| finding.severity == Severity::Warning)
            .count()
    }
}

pub fn assess(manifest: &AppManifest, from: Version, target: &Release, observed: &Observed) -> Assessment {
    let to = target.parsed_version();
    let mut releases: Vec<&Release> = manifest
        .releases
        .iter()
        .filter(|release| release.parsed_version() > from && release.parsed_version() <= to)
        .collect();
    releases.sort_by_key(|release| release.parsed_version());

    let mut findings = Vec::new();
    let mut stones: Vec<(Version, String)> = Vec::new();
    let mut unverified_cpu_flags = Vec::new();
    let mut checked_cpu_flags = BTreeSet::new();
    let database_migration = releases.iter().any(|release| release.database_migration);

    for release in &releases {
        let release_version = release.parsed_version();
        let mut add_stone = |candidate: &str| {
            if let Some(stone) = Version::parse(candidate) {
                if stone > from && stone < release_version && !stones.iter().any(|(known, _)| *known == stone) {
                    stones.push((stone, candidate.to_owned()));
                }
            }
        };

        if let Some(minimum) = &release.minimum_prior_version {
            add_stone(minimum);
        }

        for flag in &release.required_cpu_flags {
            if !checked_cpu_flags.insert(flag.clone()) {
                continue;
            }
            match observed.cpu_flags {
                Some(flags) if flags.contains(flag) => {}
                Some(_) => findings.push(Finding {
                    release: release.version.clone(),
                    slug: format!("cpu-{flag}"),
                    severity: Severity::Blocking,
                    category: "cpu_requirement".to_owned(),
                    explanation: format!(
                        "{} requires the {flag} CPU instruction set, which this host lacks.",
                        release.version
                    ),
                    instruction: Some("Do not update on this machine.".to_owned()),
                    command: None,
                }),
                None => unverified_cpu_flags.push(flag.clone()),
            }
        }

        for rule in &release.rules {
            if let Action::RequireSteppingStone { minimum_prior_version } = &rule.action {
                add_stone(minimum_prior_version);
                if Version::parse(minimum_prior_version).is_some_and(|stone| stone <= from) {
                    continue;
                }
            }
            if applies(&rule.condition, observed) {
                findings.push(finding(release, rule));
            }
        }
    }

    if let Some(false) = target
        .arch_readiness
        .get(observed.variant)
        .and_then(|archs| archs.get(observed.arch))
    {
        findings.push(Finding {
            release: target.version.clone(),
            slug: "arch-not-ready".to_owned(),
            severity: Severity::Blocking,
            category: "arch_readiness".to_owned(),
            explanation: format!("{} has no {} image published yet.", target.version, observed.arch),
            instruction: Some("Wait until the image for your architecture is published.".to_owned()),
            command: None,
        });
    }

    stones.sort();
    let stepping_stones: Vec<String> = stones.into_iter().map(|(_, tag)| tag).collect();
    let blocking = findings.iter().filter(|finding| finding.severity.blocks()).count();
    let warnings = findings
        .iter()
        .filter(|finding| finding.severity == Severity::Warning)
        .count();

    let verdict = if blocking > 0 {
        Verdict::Red
    } else if !stepping_stones.is_empty() || database_migration || warnings > 0 || !unverified_cpu_flags.is_empty() {
        Verdict::Yellow
    } else {
        Verdict::Green
    };

    let (headline, summary) = describe(
        verdict,
        releases.len(),
        blocking,
        warnings,
        &stepping_stones,
        database_migration,
    );
    let steps = steps(
        &findings,
        &stepping_stones,
        &unverified_cpu_flags,
        database_migration,
        &target.version,
    );

    Assessment {
        verdict,
        headline,
        summary,
        releases_checked: releases.len(),
        stepping_stones,
        database_migration,
        unverified_cpu_flags,
        findings,
        steps,
    }
}

fn applies(condition: &Condition, observed: &Observed) -> bool {
    match condition {
        Condition::Always | Condition::Unknown => true,
        Condition::EnvKeyPresent { key } => observed.env_keys.contains(key),
        Condition::EnvKeyMissing { key } => !observed.env_keys.contains(key),
        // Values are never read, so a rule about a value applies whenever its key exists.
        Condition::EnvValueMatches { key } => observed.env_keys.contains(key),
        Condition::CompanionServiceImageMatches { service, pattern } => {
            let named: Vec<&Companion> = observed
                .companions
                .iter()
                .filter(|companion| service.as_deref() == Some(companion.service.as_str()))
                .collect();
            let candidates = if named.is_empty() {
                observed.companions.iter().collect()
            } else {
                named
            };
            candidates
                .iter()
                .any(|companion| glob_matches(pattern, &companion.image))
        }
        Condition::HostArchEquals { arch } => arch.eq_ignore_ascii_case(observed.arch),
        Condition::MissingCpuFlag { flag } => observed.cpu_flags.is_none_or(|flags| !flags.contains(flag)),
    }
}

/// Case-insensitive match where `*` stands for any run of characters.
pub fn glob_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let value = value.to_ascii_lowercase();
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();

    let Some(mut rest) = value.strip_prefix(first) else {
        return false;
    };
    let mut parts: Vec<&str> = parts.collect();
    let Some(last) = parts.pop() else {
        return rest.is_empty();
    };

    for part in parts {
        match rest.find(part) {
            Some(position) => rest = &rest[position + part.len()..],
            None => return false,
        }
    }

    rest.ends_with(last)
}

fn finding(release: &Release, rule: &Rule) -> Finding {
    let instruction = rule.action.instruction();

    Finding {
        release: release.version.clone(),
        slug: rule.rule_slug.clone(),
        severity: rule.severity,
        category: rule.category.clone(),
        explanation: rule.explanation.clone(),
        command: instruction.as_ref().and_then(|instruction| instruction.command.clone()),
        instruction: instruction.map(|instruction| instruction.text),
    }
}

fn plural(count: usize, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

fn describe(
    verdict: Verdict,
    releases: usize,
    blocking: usize,
    warnings: usize,
    stones: &[String],
    database_migration: bool,
) -> (String, String) {
    let span = plural(releases, "release");

    match verdict {
        Verdict::Red => (
            "Hold back".to_owned(),
            format!(
                "{} must be handled before this update. {span} checked.",
                plural(blocking, "breaking change")
            ),
        ),
        Verdict::Yellow => {
            let mut reasons = Vec::new();
            if !stones.is_empty() {
                reasons.push(format!("update to {} first", stones.join(", then ")));
            }
            if database_migration {
                reasons.push("back up before a database migration".to_owned());
            }
            if warnings > 0 {
                reasons.push(format!("review {}", plural(warnings, "config change")));
            }
            if reasons.is_empty() {
                reasons.push("confirm a CPU requirement".to_owned());
            }
            (
                "Update with care".to_owned(),
                format!("{span} checked: {}.", reasons.join("; ")),
            )
        }
        Verdict::Green => (
            "Safe to update".to_owned(),
            format!("{span} checked, nothing to change."),
        ),
        Verdict::Unknown => ("Not checked".to_owned(), String::new()),
    }
}

fn steps(
    findings: &[Finding],
    stones: &[String],
    unverified_cpu_flags: &[String],
    database_migration: bool,
    target: &str,
) -> Vec<Step> {
    let actionable: Vec<&Finding> = findings
        .iter()
        .filter(|finding| finding.severity != Severity::Info && finding.instruction.is_some())
        .collect();
    let needs_care = !actionable.is_empty() || database_migration || !stones.is_empty();
    let mut steps = Vec::new();

    if needs_care {
        steps.push(Step {
            title: "Back up before you touch anything".to_owned(),
            detail: Some(if database_migration {
                "This path runs a database migration that pulling the old image will not undo. Dump the database and snapshot your volumes first.".to_owned()
            } else {
                "Snapshot your volumes so you can roll back.".to_owned()
            }),
            command: None,
        });
    }

    for flag in unverified_cpu_flags {
        steps.push(Step {
            title: format!("Confirm your CPU supports {flag}"),
            detail: Some("No output means the CPU lacks it. Do not update on that machine.".to_owned()),
            command: Some(format!("grep -m1 -o -w {flag} /proc/cpuinfo")),
        });
    }

    for stone in stones {
        steps.push(Step {
            title: format!("Update to {stone} first"),
            detail: Some("Start it and wait until its migrations have finished before continuing.".to_owned()),
            command: None,
        });
    }

    for finding in actionable {
        steps.push(Step {
            title: finding.instruction.clone().unwrap_or_default(),
            detail: Some(format!("{} ({})", finding.explanation, finding.release)),
            command: finding.command.clone(),
        });
    }

    steps.push(Step {
        title: format!("Update to {target}"),
        detail: None,
        command: Some("docker compose pull && docker compose up -d".to_owned()),
    });

    if needs_care {
        steps.push(Step {
            title: "Watch the logs until the app is ready".to_owned(),
            detail: None,
            command: Some("docker compose logs -f --tail=100".to_owned()),
        });
    }

    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(name: &str) -> AppManifest {
        let path = format!("{}/tests/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    struct Host {
        env_keys: BTreeSet<String>,
        companions: Vec<Companion>,
        cpu_flags: Option<BTreeSet<String>>,
        arch: &'static str,
    }

    impl Host {
        fn modern() -> Self {
            Self {
                env_keys: BTreeSet::new(),
                companions: Vec::new(),
                cpu_flags: Some(["avx2".to_owned()].into()),
                arch: "amd64",
            }
        }

        fn assess(&self, manifest: &AppManifest, from: &str, to: &str) -> Assessment {
            let target = manifest.release(Version::parse(to).unwrap()).unwrap();
            let observed = Observed {
                env_keys: &self.env_keys,
                companions: &self.companions,
                arch: self.arch,
                cpu_flags: self.cpu_flags.as_ref(),
                variant: "official",
            };
            assess(manifest, Version::parse(from).unwrap(), target, &observed)
        }
    }

    fn slugs(assessment: &Assessment) -> Vec<&str> {
        assessment
            .findings
            .iter()
            .map(|finding| finding.slug.as_str())
            .collect()
    }

    #[test]
    fn a_range_without_rules_is_green() {
        let assessment = Host::modern().assess(&manifest("immich"), "v1.128.0", "v1.133.0");

        assert_eq!(assessment.verdict, Verdict::Green);
        assert_eq!(assessment.releases_checked, 2);
        assert_eq!(assessment.steps.len(), 1);
    }

    #[test]
    fn unconditional_blocking_rules_make_the_range_red() {
        let assessment = Host::modern().assess(&manifest("immich"), "v1.133.0", "v2.0.0");

        assert_eq!(assessment.verdict, Verdict::Red);
        assert_eq!(slugs(&assessment), ["immich-v2-volume-target"]);
        assert!(assessment.database_migration);
        assert!(assessment.stepping_stones.is_empty());
    }

    #[test]
    fn conditional_rules_apply_only_to_what_the_host_runs() {
        let mut host = Host::modern();
        host.env_keys.insert("DB_HOSTNAME".to_owned());
        host.companions.push(Companion {
            service: "database".to_owned(),
            image: "docker.io/tensorchord/pgvecto-rs:pg14-v0.2.0".to_owned(),
        });

        let assessment = host.assess(&manifest("immich"), "v1.133.0", "v2.0.0");

        assert_eq!(
            slugs(&assessment),
            [
                "immich-v2-vectorchord-swap",
                "immich-v2-volume-target",
                "immich-v2-env-postgres-host"
            ]
        );
        assert_eq!(assessment.blocking_count(), 2);
        assert_eq!(assessment.warning_count(), 1);
    }

    #[test]
    fn a_skipped_minimum_prior_version_becomes_a_stepping_stone() {
        let assessment = Host::modern().assess(&manifest("immich"), "v1.132.0", "v2.0.0");

        assert_eq!(assessment.stepping_stones, ["v1.133.0"]);
        assert!(assessment
            .steps
            .iter()
            .any(|step| step.title == "Update to v1.133.0 first"));
    }

    #[test]
    fn a_missing_cpu_flag_blocks_and_an_unreadable_one_is_left_to_the_user() {
        let mut host = Host::modern();
        host.cpu_flags = Some(BTreeSet::new());
        assert!(slugs(&host.assess(&manifest("immich"), "v1.133.0", "v2.0.0")).contains(&"cpu-avx2"));

        host.cpu_flags = None;
        let assessment = host.assess(&manifest("immich"), "v1.133.0", "v2.0.0");
        assert_eq!(assessment.unverified_cpu_flags, ["avx2"]);
        assert!(!slugs(&assessment).contains(&"cpu-avx2"));
    }

    #[test]
    fn a_stepping_stone_rule_stops_applying_once_the_host_is_past_it() {
        let nextcloud = manifest("nextcloud");

        let direct = Host::modern().assess(&nextcloud, "v28.0.0", "v30.0.0");
        assert_eq!(direct.verdict, Verdict::Red);
        assert_eq!(direct.stepping_stones, ["v29.0.0"]);
        assert_eq!(slugs(&direct), ["nextcloud-v30-stepping-stone"]);

        let stepwise = Host::modern().assess(&nextcloud, "v29.0.0", "v30.0.0");
        assert_eq!(stepwise.verdict, Verdict::Yellow);
        assert!(stepwise.findings.is_empty());
    }

    #[test]
    fn an_unpublished_architecture_blocks_the_target() {
        let mut manifest = manifest("immich");
        let target = manifest
            .releases
            .iter_mut()
            .find(|release| release.version == "v1.133.0")
            .unwrap();
        target
            .arch_readiness
            .get_mut("official")
            .unwrap()
            .insert("arm64".to_owned(), false);

        let mut host = Host::modern();
        host.arch = "arm64";

        assert_eq!(
            slugs(&host.assess(&manifest, "v1.132.0", "v1.133.0")),
            ["arch-not-ready"]
        );
    }

    #[test]
    fn rules_this_agent_does_not_understand_count_against_the_update() {
        let rule: Rule = serde_json::from_value(serde_json::json!({
            "rule_slug": "future-rule",
            "severity": "catastrophic",
            "category": "something_new",
            "condition": {"type": "phase_of_moon", "phase": "full"},
            "action": {"type": "sacrifice_goat"},
            "explanation": "A rule type from a newer dataset."
        }))
        .unwrap();
        let mut manifest = manifest("immich");
        manifest
            .releases
            .iter_mut()
            .find(|release| release.version == "v1.133.0")
            .unwrap()
            .rules
            .push(rule);

        let assessment = Host::modern().assess(&manifest, "v1.132.0", "v1.133.0");

        assert_eq!(assessment.verdict, Verdict::Red);
        assert_eq!(slugs(&assessment), ["future-rule"]);
    }

    #[test]
    fn glob_patterns_match_case_insensitively_with_wildcards() {
        assert!(glob_matches("*pgvecto-rs*", "docker.io/tensorchord/PGVECTO-RS:pg14"));
        assert!(glob_matches("redis:*", "redis:7"));
        assert!(glob_matches("exact", "exact"));
        assert!(!glob_matches("redis:*", "valkey:8"));
        assert!(!glob_matches("exact", "exactly"));
        assert!(!glob_matches("a*b*c", "acb"));
    }
}
