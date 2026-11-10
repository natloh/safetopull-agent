//! Types for the public dataset: `/v1/index.json` and `/v1/apps/{slug}.json`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::version::Version;

#[derive(Debug, Clone, Deserialize)]
pub struct AppIndex {
    pub apps: Vec<IndexedApp>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IndexedApp {
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub images: Vec<IndexedImage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IndexedImage {
    pub variant: String,
    pub registry: String,
    pub repository: String,
    #[serde(default)]
    pub rolling_tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppManifest {
    #[serde(default)]
    pub digest_map: HashMap<String, DigestEntry>,
    #[serde(default)]
    pub releases: Vec<Release>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DigestEntry {
    pub version: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub version: String,
    pub semver: Vec<u64>,
    #[serde(default)]
    pub database_migration: bool,
    #[serde(default)]
    pub minimum_prior_version: Option<String>,
    #[serde(default)]
    pub required_cpu_flags: Vec<String>,
    #[serde(default)]
    pub arch_readiness: HashMap<String, HashMap<String, bool>>,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

impl Release {
    pub fn parsed_version(&self) -> Version {
        Version::from_parts(&self.semver)
    }
}

impl AppManifest {
    pub fn latest(&self) -> Option<&Release> {
        self.releases.iter().max_by_key(|release| release.parsed_version())
    }

    pub fn release(&self, version: Version) -> Option<&Release> {
        self.releases.iter().find(|release| release.parsed_version() == version)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    pub rule_slug: String,
    pub severity: Severity,
    pub category: String,
    pub condition: Condition,
    pub action: Action,
    pub explanation: String,
}

/// A severity this agent does not know yet is treated as blocking: unknown never means safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Blocking,
    ConditionalBlocking,
    Warning,
    Info,
    #[serde(other)]
    Unknown,
}

impl Severity {
    pub fn blocks(self) -> bool {
        matches!(self, Self::Blocking | Self::ConditionalBlocking | Self::Unknown)
    }
}

/// A condition type this agent does not know yet is assumed to apply.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Condition {
    Always,
    EnvKeyPresent { key: String },
    EnvKeyMissing { key: String },
    EnvValueMatches { key: String },
    CompanionServiceImageMatches {
        #[serde(default)]
        service: Option<String>,
        pattern: String,
    },
    HostArchEquals { arch: String },
    MissingCpuFlag { flag: String },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    RenameEnvKey {
        from_key: String,
        to_key: String,
    },
    AddEnvKey {
        key: String,
        #[serde(default)]
        default_value: Option<String>,
        #[serde(default)]
        required: bool,
    },
    RemoveEnvKey {
        key: String,
    },
    ReplaceCompanionImage {
        #[serde(default)]
        service: Option<String>,
        #[serde(default)]
        from_image: Option<String>,
        to_image: String,
    },
    UpdateVolumeTarget {
        #[serde(default)]
        service: Option<String>,
        from_target: String,
        to_target: String,
    },
    RequireSteppingStone {
        minimum_prior_version: String,
    },
    ManualCliIntervention {
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        instructions: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

pub struct Instruction {
    pub text: String,
    pub command: Option<String>,
}

impl Action {
    pub fn instruction(&self) -> Option<Instruction> {
        let text = |text: String| Some(Instruction { text, command: None });

        match self {
            Self::RenameEnvKey { from_key, to_key } => text(format!("Rename {from_key} to {to_key}.")),
            Self::AddEnvKey { key, default_value, required } => {
                let suggestion = default_value
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .map(|value| format!(" (suggested value: {value})"))
                    .unwrap_or_default();
                let requirement = if *required { ", which is required" } else { "" };
                text(format!("Add {key}{suggestion}{requirement}."))
            }
            Self::RemoveEnvKey { key } => text(format!("Remove {key}.")),
            Self::ReplaceCompanionImage { service, from_image, to_image } => {
                let service = service.as_deref().unwrap_or("companion");
                let from = from_image.as_deref().map(|image| format!(" from {image}")).unwrap_or_default();
                text(format!("Change the {service} image{from} to {to_image}."))
            }
            Self::UpdateVolumeTarget { service, from_target, to_target } => {
                let service = service.as_deref().map(|name| format!("{name} ")).unwrap_or_default();
                text(format!("Change the {service}volume mount target from {from_target} to {to_target}."))
            }
            Self::ManualCliIntervention { command, instructions } => Some(Instruction {
                text: instructions.clone().unwrap_or_else(|| "Run this command before updating.".to_owned()),
                command: command.clone(),
            }),
            Self::RequireSteppingStone { .. } | Self::Unknown => None,
        }
    }
}
