//! Workflow permissions are replaced, not merged, at each explicit permissions block.

use anyhow::ensure;

use anyhow::Result;
use codeyard_database::models::CiRun;
use codeyard_database::DbPool;
use codeyard_db_actions::ci_tokens::NewJobToken;
use codeyard_types::ci::MAX_EXPANDED_JOBS;
use codeyard_types::permissions::Permissions;
use codeyard_types::permissions::PERMISSION_SCOPES as SCOPES;
use serde::Deserialize;
use serde::Serialize;
use serde_yaml::Value;
use std::collections::BTreeMap;

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

pub async fn issue_selected(
    pool: &DbPool,
    run: &CiRun,
    definitions: &BTreeMap<String, Vec<u8>>,
    permissive: bool,
    replay: Option<&codeyard_runner::progress::Replay>,
) -> Result<GithubTokens> {
    anyhow::ensure!(
        replay.is_none() || run.engine.as_deref() == Some("codeyard-actions"),
        "Partial re-runs require a native workflow"
    );
    let parsed = codeyard_types::workflow_yaml::parse_definitions(definitions).await?;
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
                < codeyard_runner::workflow::MAX_REUSABLE_LEVELS,
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
            Ok((
                key.clone(),
                pool.cell_prefix().token(
                    codeyard_types::tokens::ACTIONS_PREFIX,
                    &crate::runtime::secret(),
                )?,
            ))
        })
        .collect::<Result<_>>()?;
    let stored: Vec<_> = policies
        .iter()
        .map(|(job, permissions)| {
            Ok((
                job.as_str(),
                codeyard_support::token_hash::token_hash(&values[job]),
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
    codeyard_db_actions::ci_tokens::issue(
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
        codeyard_db_actions::oidc::bind_job(
            pool,
            &codeyard_support::token_hash::token_hash(&values[key]),
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
        oidc_url: crate::runtime::oidc_url(),
        act: values.get("act").cloned().unwrap_or_default(),
        act_limited,
        completion_secret: crate::runtime::secret(),
    })
}
