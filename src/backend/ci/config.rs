//! CI settings from environment variables.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// How workflow jobs run on a worker node.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CiExecutionMode {
    /// Steps run directly on the worker host (`runs-on` labels map to the host).
    Local,
    /// Jobs run in containers through Podman's Docker-compatible API.
    Podman,
}
impl CiExecutionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Podman => "podman",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CiPipelineKind {
    Ci,
    Cd,
    Full,
}
impl CiPipelineKind {
    pub fn from_value(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "cd" => Self::Cd,
            "full" => Self::Full,
            _ => Self::Ci,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ci => "ci",
            Self::Cd => "cd",
            Self::Full => "full",
        }
    }
    pub fn includes_cd(&self) -> bool {
        matches!(self, Self::Cd | Self::Full)
    }
}

#[derive(Debug, Clone)]
pub struct CiConfig {
    pub mode: CiExecutionMode,
    pub act_binary: String,
    /// `runs-on` label → container image, for Podman mode.
    pub act_platform_map: BTreeMap<String, String>,
    /// Podman API socket used as `DOCKER_HOST` in Podman mode.
    pub podman_socket: String,
    pub workflow_ci: String,
    pub workflow_cd: String,
    /// Non-admin users allowed to run workflows on the CI machines.
    pub trusted_actor_ids: BTreeSet<i64>,
    /// Runs one worker node executes at the same time.
    pub max_active_runs: usize,
    pub run_timeout_seconds: u32,
}

impl Default for CiConfig {
    fn default() -> Self {
        Self {
            mode: CiExecutionMode::Podman,
            act_binary: "act".into(),
            act_platform_map: default_platform_map("ghcr.io/catthehacker/ubuntu:act-latest"),
            podman_socket: default_podman_socket(),
            workflow_ci: ".github/workflows/ci.yml".into(),
            workflow_cd: ".github/workflows/deploy-image.yml".into(),
            trusted_actor_ids: BTreeSet::new(),
            max_active_runs: 4,
            run_timeout_seconds: 3600,
        }
    }
}

impl CiConfig {
    pub fn from_env() -> Result<Self> {
        let defaults = Self::default();
        let var = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
        let mode = match var("CI_EXECUTION_MODE").as_deref() {
            None => defaults.mode,
            Some(value) => match value.to_ascii_lowercase().as_str() {
                "local" => CiExecutionMode::Local,
                "podman" => CiExecutionMode::Podman,
                _ => bail!("CI_EXECUTION_MODE must be `local` or `podman`"),
            },
        };
        let mut act_platform_map = match var("CI_ACT_RUNNER_IMAGE") {
            Some(image) => default_platform_map(&image),
            None => defaults.act_platform_map,
        };
        if let Some(raw) = var("CI_ACT_PLATFORM_MAP") {
            act_platform_map.extend(parse_platform_map(&raw)?);
        }
        let number = |key: &str, default: u32| -> Result<u32> {
            match var(key) {
                None => Ok(default),
                Some(raw) => match raw.parse::<u32>() {
                    Ok(value) if value > 0 => Ok(value),
                    _ => bail!("{key} must be a positive integer"),
                },
            }
        };
        Ok(Self {
            mode,
            act_binary: var("CI_ACT_BINARY").unwrap_or(defaults.act_binary),
            act_platform_map,
            podman_socket: podman_socket_path(
                &var("CI_PODMAN_SOCKET").unwrap_or(defaults.podman_socket),
            )?
            .to_owned(),
            workflow_ci: var("CI_WORKFLOW_CI").unwrap_or(defaults.workflow_ci),
            workflow_cd: var("CI_WORKFLOW_CD").unwrap_or(defaults.workflow_cd),
            trusted_actor_ids: parse_actor_ids(&var("CI_TRUSTED_ACTOR_IDS").unwrap_or_default())?,
            max_active_runs: number("CI_MAX_ACTIVE_RUNS", defaults.max_active_runs as u32)?
                as usize,
            run_timeout_seconds: number("CI_RUN_TIMEOUT_SECONDS", defaults.run_timeout_seconds)?,
        })
    }

    pub fn workflow_paths(&self, pipeline: CiPipelineKind) -> Vec<String> {
        match pipeline {
            CiPipelineKind::Ci => vec![self.workflow_ci.clone()],
            CiPipelineKind::Cd => vec![self.workflow_cd.clone()],
            CiPipelineKind::Full => vec![self.workflow_ci.clone(), self.workflow_cd.clone()],
        }
    }
}

pub(super) fn podman_socket_path(socket: &str) -> Result<&str> {
    let path = socket.strip_prefix("unix://").unwrap_or(socket);
    ensure!(
        path.starts_with('/') && path.len() > 1,
        "CI_PODMAN_SOCKET must be an absolute socket path"
    );
    Ok(path)
}

fn default_podman_socket() -> String {
    match std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir
            .join("podman/podman.sock")
            .to_string_lossy()
            .into_owned(),
        _ => "/run/user/1000/podman/podman.sock".into(),
    }
}

/// The `runs-on` labels the runner image serves.
pub(super) const UBUNTU_LABELS: [&str; 4] = [
    "ubuntu-latest",
    "ubuntu-24.04",
    "ubuntu-22.04",
    "ubuntu-20.04",
];

fn default_platform_map(image: &str) -> BTreeMap<String, String> {
    UBUNTU_LABELS
        .into_iter()
        .map(|label| (label.to_owned(), image.to_owned()))
        .collect()
}

/// What the site's and the repository's Actions settings allow one run. An unset field leaves
/// the worker's own configuration: its run timeout, its runner image, and no container limit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunLimits {
    pub timeout_seconds: Option<u32>,
    pub cpus: Option<u32>,
    pub memory_gib: Option<u32>,
    pub image: Option<String>,
}

impl RunLimits {
    /// The stricter of the two settings for each limit.
    pub fn new(
        site: &crate::backend::db::actions::ActionsSettings,
        repo: &crate::backend::db::actions::RepoActionsSettings,
    ) -> Self {
        let stricter = |a: Option<i32>, b: Option<i32>| {
            let value = match (a, b) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            value.and_then(|value| u32::try_from(value).ok())
        };
        Self {
            timeout_seconds: stricter(site.run_timeout_minutes, repo.run_timeout_minutes)
                .map(|minutes| minutes * 60),
            cpus: stricter(site.job_cpus, repo.job_cpus),
            memory_gib: stricter(site.job_memory_gib, repo.job_memory_gib),
            image: site.runner_image.clone(),
        }
    }

    /// The Podman options that cap a job container, empty when nothing is capped.
    pub(super) fn container_options(&self) -> String {
        let mut options = Vec::new();
        if let Some(cpus) = self.cpus {
            options.push(format!("--cpus={cpus}"));
        }
        if let Some(gib) = self.memory_gib {
            options.push(format!("--memory={gib}g"));
        }
        options.join(" ")
    }
}

fn parse_platform_map(raw: &str) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    for entry in raw.split([',', ';', '\n']).filter(|v| !v.trim().is_empty()) {
        let (label, image) = entry
            .split_once('=')
            .context("CI_ACT_PLATFORM_MAP entries look like label=image")?;
        let label = label.trim().to_ascii_lowercase();
        ensure!(
            !label.is_empty() && !label.contains(char::is_whitespace) && !image.trim().is_empty(),
            "CI_ACT_PLATFORM_MAP entries look like label=image"
        );
        map.insert(label, image.trim().to_owned());
    }
    Ok(map)
}

fn parse_actor_ids(raw: &str) -> Result<BTreeSet<i64>> {
    raw.split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| match value.parse::<i64>() {
            Ok(id) if id > 0 => Ok(id),
            _ => bail!("CI_TRUSTED_ACTOR_IDS must be comma-separated user IDs"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_map_and_actor_ids_parse_strictly() {
        let map = parse_platform_map("self-hosted=img:1; Linux = img:2").unwrap();
        assert_eq!(map["self-hosted"], "img:1");
        assert_eq!(map["linux"], "img:2");
        assert!(parse_platform_map("missing-image").is_err());
        assert_eq!(parse_actor_ids(" 3, 5 ").unwrap(), BTreeSet::from([3, 5]));
        assert!(parse_actor_ids("0").is_err());
        assert!(parse_actor_ids("x").is_err());
    }

    #[test]
    fn podman_socket_accepts_a_path_or_unix_url_and_normalizes_to_a_path() {
        assert_eq!(
            podman_socket_path("/run/user/1000/podman/podman.sock").unwrap(),
            "/run/user/1000/podman/podman.sock"
        );
        assert_eq!(
            podman_socket_path("unix:///run/user/1000/podman/podman.sock").unwrap(),
            "/run/user/1000/podman/podman.sock"
        );
        assert!(podman_socket_path("relative.sock").is_err());
        assert!(podman_socket_path("tcp://localhost:8080").is_err());
    }

    #[test]
    fn run_limits_take_the_stricter_setting_and_default_to_none() {
        use crate::backend::db::actions::{ActionsSettings, RepoActionsSettings};
        let none = RunLimits::new(&ActionsSettings::default(), &RepoActionsSettings::default());
        assert_eq!(none, RunLimits::default());
        assert_eq!(none.container_options(), "");
        let site = ActionsSettings {
            run_timeout_minutes: Some(30),
            job_cpus: Some(8),
            runner_image: Some("img:1".into()),
            ..ActionsSettings::default()
        };
        let repo = RepoActionsSettings {
            run_timeout_minutes: Some(60),
            job_cpus: Some(2),
            job_memory_gib: Some(16),
            ..RepoActionsSettings::default()
        };
        let limits = RunLimits::new(&site, &repo);
        assert_eq!(
            limits,
            RunLimits {
                timeout_seconds: Some(1800),
                cpus: Some(2),
                memory_gib: Some(16),
                image: Some("img:1".into()),
            }
        );
        assert_eq!(limits.container_options(), "--cpus=2 --memory=16g");
    }
}
