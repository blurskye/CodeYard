//! Workflow permissions are replaced, not merged, at each explicit permissions block.
use crate::backend::db::{self, ci_tokens::NewJobToken, models::CiRun, DbPool};
use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use std::collections::BTreeMap;

use codeyard_actions::workflow::PERMISSION_SCOPES as SCOPES;

#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permissions(pub BTreeMap<String, u8>);

impl Permissions {
    pub fn allows(&self, scope: &str, write: bool) -> bool {
        self.0.get(scope).copied().unwrap_or(0) >= if write { 2 } else { 1 }
    }

    fn parse(value: Option<&Value>, default: &Self, fork: bool) -> Result<Self> {
        let mut permissions = match value {
            None => default.clone(),
            Some(Value::String(value)) if value == "read-all" || value == "write-all" => Self(
                SCOPES
                    .iter()
                    .map(|scope| {
                        (
                            (*scope).into(),
                            match (*scope, value.as_str()) {
                                ("models" | "metadata" | "vulnerability-alerts", _) => 1,
                                ("id-token", "read-all") => 0,
                                (_, "write-all") => 2,
                                _ => 1,
                            },
                        )
                    })
                    .collect(),
            ),
            Some(Value::Mapping(values)) => {
                let mut result = BTreeMap::new();
                for (name, level) in values {
                    let name = name.as_str().unwrap_or("");
                    ensure!(SCOPES.contains(&name), "unknown workflow permission");
                    let level = match level.as_str() {
                        Some("none") => 0,
                        Some("read") if name != "id-token" => 1,
                        Some("write")
                            if codeyard_actions::workflow::permission_level_allowed(
                                name, "write",
                            ) =>
                        {
                            2
                        }
                        _ => bail!("invalid workflow permission"),
                    };
                    result.insert(name.to_owned(), level);
                }
                Self(result)
            }
            _ => bail!("invalid workflow permissions"),
        };
        permissions.0.insert("metadata".into(), 1);
        if fork {
            for level in permissions.0.values_mut() {
                *level = (*level).min(1);
            }
            permissions.0.insert("id-token".into(), 0);
        }
        Ok(permissions)
    }

    pub fn jobs(bytes: &[u8], permissive: bool, fork: bool) -> Result<BTreeMap<String, Self>> {
        let workflow = super::workflows::parse_yaml(bytes)?;
        Self::jobs_from_yaml(&workflow, permissive, fork)
    }

    fn jobs_from_yaml(
        workflow: &Value,
        permissive: bool,
        fork: bool,
    ) -> Result<BTreeMap<String, Self>> {
        let default = if permissive {
            Self(
                SCOPES
                    .iter()
                    .filter(|scope| **scope != "id-token")
                    .map(|scope| {
                        (
                            (*scope).into(),
                            if matches!(*scope, "models" | "metadata" | "vulnerability-alerts") {
                                1
                            } else {
                                2
                            },
                        )
                    })
                    .collect(),
            )
        } else {
            Self(BTreeMap::from([
                ("contents".into(), 1),
                ("packages".into(), 1),
                ("metadata".into(), 1),
            ]))
        };
        let workflow_default = Self::parse(workflow.get("permissions"), &default, fork)?;
        let jobs = workflow
            .get("jobs")
            .and_then(Value::as_mapping)
            .ok_or_else(|| anyhow::anyhow!("workflow has no jobs"))?;
        jobs.iter()
            .map(|(id, job)| {
                Ok((
                    id.as_str()
                        .ok_or_else(|| anyhow::anyhow!("invalid job id"))?
                        .into(),
                    Self::parse(job.get("permissions"), &workflow_default, fork)?,
                ))
            })
            .collect()
    }

    pub fn intersection(jobs: impl Iterator<Item = Self>) -> Self {
        let mut jobs = jobs;
        let Some(mut result) = jobs.next() else {
            return Self::default();
        };
        for job in jobs {
            for (scope, level) in &mut result.0 {
                *level = (*level).min(job.0.get(scope).copied().unwrap_or(0));
            }
        }
        result
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct GithubTokens {
    pub workflows: BTreeMap<String, BTreeMap<String, String>>,
    pub act: String,
    #[serde(default)]
    pub oidc_jobs: std::collections::BTreeSet<String>,
    #[serde(default)]
    pub oidc_url: String,
    pub act_limited: bool,
    pub completion_secret: String,
}

pub async fn issue(
    pool: &DbPool,
    run: &CiRun,
    definitions: &BTreeMap<String, Vec<u8>>,
    permissive: bool,
) -> Result<GithubTokens> {
    issue_selected(pool, run, definitions, permissive, None).await
}

/// Reusable calls multiply job identities, so credentials and environment bindings share one budget.
pub(crate) const MAX_EXPANDED_JOBS: usize = 1000;

pub(crate) async fn issue_selected(
    pool: &DbPool,
    run: &CiRun,
    definitions: &BTreeMap<String, Vec<u8>>,
    permissive: bool,
    replay: Option<&codeyard_actions::progress::Replay>,
) -> Result<GithubTokens> {
    anyhow::ensure!(
        replay.is_none() || run.engine.as_deref() == Some("codeyard-actions"),
        "Partial re-runs require a native workflow"
    );
    let parsed = super::workflows::parse_definitions(definitions).await?;
    fn policies_for(
        definitions: &BTreeMap<String, Value>,
        path: &str,
        prefix: &str,
        ceiling: Option<&Permissions>,
        permissive: bool,
        fork: bool,
        policies: &mut BTreeMap<String, Permissions>,
    ) -> Result<()> {
        ensure!(
            prefix
                .split_once(':')
                .map_or(prefix, |(_, job)| job)
                .matches('/')
                .count()
                < codeyard_actions::workflow::MAX_REUSABLE_LEVELS,
            "called workflow nesting exceeds limit"
        );
        let yaml = definitions
            .get(path)
            .ok_or_else(|| anyhow::anyhow!("missing called workflow"))?;
        let mut jobs = Permissions::jobs_from_yaml(yaml, permissive, fork)?;
        if let Some(ceiling) = ceiling {
            let default = Permissions::parse(yaml.get("permissions"), ceiling, fork)?;
            ensure!(
                default
                    .0
                    .iter()
                    .all(|(scope, level)| *level <= ceiling.0.get(scope).copied().unwrap_or(0)),
                "Called workflow permissions exceed caller permissions"
            );
            for (id, permissions) in &mut jobs {
                let job = yaml.get("jobs").and_then(|jobs| jobs.get(id));
                let requested =
                    Permissions::parse(job.and_then(|job| job.get("permissions")), &default, fork)?;
                ensure!(
                    requested
                        .0
                        .iter()
                        .all(|(scope, level)| *level <= ceiling.0.get(scope).copied().unwrap_or(0)),
                    "Called job permissions exceed caller permissions"
                );
                *permissions = requested;
            }
        }
        for (id, permissions) in jobs {
            let key = format!("{prefix}{id}");
            // Each call path is its own job identity, so nested reusable calls multiply. Bound the total
            // before descending, or a few small workflows could allocate hundreds of millions of policies.
            ensure!(
                policies.len() < MAX_EXPANDED_JOBS,
                "workflow run expands to too many jobs"
            );
            policies.insert(key.clone(), permissions.clone());
            if let Some(reference) = yaml
                .get("jobs")
                .and_then(|jobs| jobs.get(&id))
                .and_then(|job| job.get("uses"))
                .and_then(Value::as_str)
            {
                let called = reference
                    .strip_prefix("./")
                    .ok_or_else(|| anyhow::anyhow!("unresolved called workflow"))?;
                policies_for(
                    definitions,
                    called,
                    &format!("{key}/"),
                    Some(&permissions),
                    permissive,
                    fork,
                    policies,
                )?;
            }
        }
        Ok(())
    }
    let mut policies = BTreeMap::new();
    for path in run.workflow_path.split(',').filter(|path| !path.is_empty()) {
        policies_for(
            &parsed,
            path,
            &format!("{path}:"),
            None,
            permissive || run.event == "pull_request_target",
            run.is_fork,
            &mut policies,
        )?;
    }
    if let Some(replay) = replay {
        policies.retain(|key, _| {
            key.split_once(':').is_some_and(|(_, job)| {
                replay
                    .selected
                    .contains_key(job.split('/').next().unwrap_or(job))
            })
        });
    }
    anyhow::ensure!(
        replay.is_none_or(|replay| !replay.selected.is_empty()
            && replay.selected.keys().all(|job| {
                policies
                    .keys()
                    .any(|key| key.split_once(':').is_some_and(|(_, id)| id == job))
            })),
        "Selected job credentials are unavailable"
    );
    let intersection = Permissions::intersection(policies.values().cloned());
    let act_limited = policies.values().any(|policy| {
        SCOPES.iter().any(|scope| {
            policy.0.get(*scope).copied().unwrap_or(0)
                > intersection.0.get(*scope).copied().unwrap_or(0)
        })
    });
    if run.engine.as_deref() == Some("act") {
        policies.clear();
        policies.insert("act".into(), intersection);
    }
    let values: BTreeMap<_, _> = policies
        .keys()
        .map(|key| {
            (
                key.clone(),
                format!(
                    "{}{}",
                    codeyard_support::tokens::ACTIONS_PREFIX,
                    super::runtime::secret()
                ),
            )
        })
        .collect();
    let stored: Vec<_> = policies
        .iter()
        .map(|(job, permissions)| {
            Ok((
                job.as_str(),
                super::runtime::token_hash(&values[job]),
                serde_json::to_string(permissions)?,
                permissions.0.values().any(|value| *value == 2),
            ))
        })
        .collect::<Result<_>>()?;
    let tokens: Vec<_> = stored
        .iter()
        .map(|(job, hash, permissions, write)| NewJobToken {
            job,
            hash,
            permissions,
            write: *write,
        })
        .collect();
    db::ci_tokens::issue(
        pool,
        run.id,
        run.worker
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("run has no worker"))?,
        &tokens,
    )
    .await?;
    let mut oidc_jobs = std::collections::BTreeSet::new();
    for (key, permissions) in &policies {
        if !permissions.allows("id-token", true) {
            continue;
        }
        // act has only one credential, so all jobs must agree on their identity too.
        let (path, job) = if key == "act" {
            anyhow::ensure!(definitions.len() == 1, "OIDC requires one act workflow");
            (definitions.keys().next().unwrap().as_str(), None)
        } else {
            let (path, jobs) = key
                .split_once(':')
                .ok_or_else(|| anyhow::anyhow!("Invalid job identity"))?;
            let mut path = path;
            let mut job = jobs;
            while let Some((caller, child)) = job.split_once('/') {
                let workflow = &parsed[path];
                let reference = workflow
                    .get("jobs")
                    .and_then(|jobs| jobs.get(caller))
                    .and_then(|job| job.get("uses"))
                    .and_then(Value::as_str)
                    .and_then(|reference| reference.strip_prefix("./"))
                    .ok_or_else(|| anyhow::anyhow!("Missing called workflow identity"))?
                    .to_owned();
                path = definitions
                    .get_key_value(&reference)
                    .ok_or_else(|| anyhow::anyhow!("Missing called workflow"))?
                    .0;
                job = child;
            }
            (path, Some(job))
        };
        let workflow = &parsed[path];
        let root = key.split_once(':').map_or(path, |(root, _)| root);
        let name = parsed[root]
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(root);
        let environment = job
            .and_then(|job| workflow.get("jobs")?.get(job)?.get("environment"))
            .and_then(|env| {
                env.as_str()
                    .or_else(|| env.get("name").and_then(Value::as_str))
            });
        anyhow::ensure!(
            environment.is_none_or(|env| !env.contains("${{")),
            "OIDC requires a literal environment"
        );
        if key == "act" {
            anyhow::ensure!(
                workflow
                    .get("jobs")
                    .and_then(Value::as_mapping)
                    .is_some_and(|jobs| jobs.values().all(|job| job.get("environment").is_none())),
                "OIDC environment identity requires the native runner"
            );
        }
        db::oidc::bind_job(
            pool,
            &super::runtime::token_hash(&values[key]),
            path,
            name,
            environment,
        )
        .await?;
        oidc_jobs.insert(key.clone());
    }
    let mut workflows = BTreeMap::new();
    for path in definitions.keys() {
        let prefix = format!("{path}:");
        workflows.insert(
            path.clone(),
            values
                .iter()
                .filter_map(|(key, value)| {
                    key.strip_prefix(&prefix)
                        .map(|job| (job.to_owned(), value.clone()))
                })
                .collect(),
        );
    }
    Ok(GithubTokens {
        workflows,
        oidc_jobs,
        oidc_url: super::runtime::oidc_url(),
        act: values.get("act").cloned().unwrap_or_default(),
        act_limited,
        completion_secret: super::runtime::secret(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn artifact_metadata_permissions_follow_workflow_replacement_and_fork_downgrades() {
        let bytes = b"permissions: {artifact-metadata: write}\njobs:\n  writer: {}\n  reader: {permissions: {artifact-metadata: read}}";
        let jobs = Permissions::jobs(bytes, false, false).unwrap();
        assert!(jobs["writer"].allows("artifact-metadata", true));
        assert!(jobs["reader"].allows("artifact-metadata", false));
        assert!(!jobs["reader"].allows("artifact-metadata", true));
        let jobs = Permissions::jobs(bytes, false, true).unwrap();
        assert!(!jobs["writer"].allows("artifact-metadata", true));
    }

    #[test]
    fn job_overrides_and_intersections_never_add_permissions() {
        let bytes = b"permissions: {contents: write, issues: write}\njobs:\n  first: {}\n  second: {permissions: {contents: read}}";
        let jobs = Permissions::jobs(bytes, false, false).unwrap();
        assert!(jobs["first"].allows("issues", true));
        assert!(!jobs["second"].allows("issues", false));
        let least = Permissions::intersection(jobs.into_values());
        assert!(least.allows("contents", false));
        assert!(!least.allows("contents", true));
        assert!(!least.allows("issues", false));
        for job in Permissions::jobs(bytes, true, true).unwrap().values() {
            assert!(!job.allows("contents", true));
            assert!(!job.allows("id-token", true));
        }
        assert!(
            Permissions::jobs(b"jobs: {test: {}}", false, false).unwrap()["test"]
                .allows("contents", false)
        );
        assert!(
            !Permissions::jobs(b"permissions: {}\njobs: {test: {}}", true, false).unwrap()["test"]
                .0
                .contains_key("contents")
        );
        assert!(
            Permissions::jobs(b"permissions: {invalid: write}\njobs: {}", false, false).is_err()
        );
    }
}
