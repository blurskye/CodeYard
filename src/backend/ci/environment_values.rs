//! Bind environment values to admitted job IDs before either engine receives them.
use crate::backend::{
    crypto::SecretEncryptor,
    db::{
        environment_values::{self as values, WorkflowValues},
        models::CiRun,
        DbPool,
    },
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct EnvironmentValues {
    pub environments: BTreeMap<String, WorkflowValues>,
    pub workflows: BTreeMap<String, BTreeMap<String, String>>,
}

pub(super) async fn load(
    pool: &DbPool,
    encryptor: Option<&SecretEncryptor>,
    run: &CiRun,
    definitions: &BTreeMap<String, Vec<u8>>,
) -> Result<EnvironmentValues> {
    let definitions = super::workflows::parse_definitions(definitions).await?;
    let mut result = EnvironmentValues::default();
    // Each definition's bindings use its own job IDs. The run's roots qualify them through their calls.
    let mut local = BTreeMap::new();
    for (path, workflow) in &definitions {
        let jobs = workflow
            .get("jobs")
            .and_then(serde_yaml::Value::as_mapping)
            .context("Invalid admitted jobs")?;
        let mut bindings = BTreeMap::new();
        for (job, definition) in jobs {
            let Some(environment) = definition.get("environment") else {
                continue;
            };
            let environment = environment
                .as_str()
                .or_else(|| environment.get("name").and_then(serde_yaml::Value::as_str))
                .context("Invalid admitted job environment")?;
            ensure!(
                !environment.contains("${{"),
                "CI requires literal job environment names"
            );
            let key = values::environment_key(environment)?;
            if let std::collections::btree_map::Entry::Vacant(entry) =
                result.environments.entry(key.clone())
            {
                let scoped =
                    values::for_run(pool, encryptor, run.repo_id, environment, run.is_fork).await?;
                entry.insert(scoped);
            }
            let job = job.as_str().context("Invalid admitted job ID")?;
            bindings.insert(job.to_owned(), key);
        }
        local.insert(path.clone(), bindings);
    }
    for root in run.workflow_path.split(',').filter(|path| !path.is_empty()) {
        let mut bindings = BTreeMap::new();
        let mut visited = 0;
        qualify(&definitions, &local, root, "", &mut visited, &mut bindings)?;
        result.workflows.insert(root.to_owned(), bindings);
    }
    Ok(result)
}

/// Names each binding by the job ID the runner uses at that depth, such as `call/deploy`. A job
/// in a called workflow must not inherit a binding that its name has at the root.
fn qualify(
    definitions: &BTreeMap<String, serde_yaml::Value>,
    local: &BTreeMap<String, BTreeMap<String, String>>,
    path: &str,
    prefix: &str,
    visited: &mut usize,
    out: &mut BTreeMap<String, String>,
) -> Result<()> {
    ensure!(
        prefix.matches('/').count() < codeyard_actions::workflow::MAX_REUSABLE_LEVELS,
        "Called workflow nesting exceeds limit"
    );
    let Some(workflow) = definitions.get(path) else {
        return Ok(());
    };
    let jobs = workflow
        .get("jobs")
        .and_then(serde_yaml::Value::as_mapping)
        .context("Invalid admitted jobs")?;
    for (job, definition) in jobs {
        // Nested calls multiply, so the walk shares the same job budget as credential expansion.
        *visited += 1;
        ensure!(
            *visited <= super::tokens::MAX_EXPANDED_JOBS,
            "workflow run expands to too many jobs"
        );
        let job = job.as_str().context("Invalid admitted job ID")?;
        let id = format!("{prefix}{job}");
        if let Some(key) = local.get(path).and_then(|bindings| bindings.get(job)) {
            out.insert(id.clone(), key.clone());
        }
        if let Some(reference) = definition.get("uses").and_then(serde_yaml::Value::as_str) {
            let callee = reference
                .strip_prefix("./")
                .context("unresolved called workflow")?;
            qualify(definitions, local, callee, &format!("{id}/"), visited, out)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn called_jobs_get_only_their_own_bindings() {
        let definitions = BTreeMap::from([
            (
                "ci.yml".to_owned(),
                b"jobs:\n  deploy: {}\n  call: {uses: ./codeyard-call-1.yml}\n".to_vec(),
            ),
            (
                "codeyard-call-1.yml".to_owned(),
                b"jobs:\n  deploy: {}\n  build: {}\n".to_vec(),
            ),
        ]);
        let definitions = definitions
            .into_iter()
            .map(|(path, bytes)| (path, super::super::workflows::parse_yaml(&bytes).unwrap()))
            .collect();
        let local = BTreeMap::from([(
            "ci.yml".to_owned(),
            BTreeMap::from([("deploy".to_owned(), "production".to_owned())]),
        )]);
        let mut bindings = BTreeMap::new();
        let mut visited = 0;
        qualify(
            &definitions,
            &local,
            "ci.yml",
            "",
            &mut visited,
            &mut bindings,
        )
        .unwrap();
        assert_eq!(
            bindings,
            BTreeMap::from([("deploy".to_owned(), "production".to_owned())])
        );
    }
}
