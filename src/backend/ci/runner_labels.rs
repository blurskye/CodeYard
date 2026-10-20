//! `runs-on` labels of a workflow, and which job features each mode supports.
use anyhow::{bail, ensure, Context, Result};
use serde_yaml::Value;
use std::collections::BTreeSet;

/// Literal `runs-on` labels used by the workflow's jobs. In local mode jobs run
/// directly on the worker host, so container jobs and Docker actions are refused.
pub(super) fn labels_from_value(workflow: &Value, local: bool) -> Result<BTreeSet<String>> {
    let jobs = workflow
        .get("jobs")
        .and_then(Value::as_mapping)
        .context("the workflow has no jobs")?;
    ensure!(!jobs.is_empty(), "the workflow has no jobs");
    let mut labels = BTreeSet::new();
    for job in jobs.values() {
        if job.get("uses").is_some() {
            bail!("reusable workflows (`jobs.<id>.uses`) are not supported");
        }
        if local {
            ensure!(
                job.get("container").is_none() && job.get("services").is_none(),
                "container jobs and services need CI_EXECUTION_MODE=podman"
            );
            let steps = job.get("steps").and_then(Value::as_sequence);
            for step in steps.into_iter().flatten() {
                let uses = step.get("uses").and_then(Value::as_str).unwrap_or("");
                ensure!(
                    !uses.starts_with("docker://"),
                    "Docker actions need CI_EXECUTION_MODE=podman"
                );
            }
        }
        let runs_on = job.get("runs-on").context("every job needs `runs-on`")?;
        let values = match runs_on {
            Value::String(_) => vec![runs_on],
            Value::Sequence(values) => values.iter().collect(),
            _ => bail!("`runs-on` must be a label or a list of labels"),
        };
        ensure!(!values.is_empty(), "`runs-on` must name a label");
        for value in values {
            let label = value
                .as_str()
                .context("`runs-on` must be a literal label")?;
            ensure!(
                !label.is_empty()
                    && label.len() <= 128
                    && label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte)),
                "`runs-on` must be a literal label, not an expression"
            );
            labels.insert(label.to_ascii_lowercase());
        }
    }
    Ok(labels)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(yaml: &str, local: bool) -> Result<BTreeSet<String>> {
        labels_from_value(&serde_yaml::from_str::<Value>(yaml).unwrap(), local)
    }

    #[test]
    fn labels_are_literal_and_local_mode_refuses_containers() {
        let workflow = "jobs:\n  build:\n    runs-on: [self-hosted, Linux]\n    steps:\n      - uses: actions/checkout@v4\n      - run: make\n";
        assert_eq!(
            labels(workflow, true).unwrap(),
            BTreeSet::from(["linux".into(), "self-hosted".into()])
        );
        let container = "jobs:\n  build:\n    runs-on: ubuntu-latest\n    container: alpine\n    steps:\n      - run: true\n";
        assert!(labels(container, true).is_err());
        assert!(labels(container, false).is_ok());
        assert!(labels(
            "jobs: {a: {runs-on: '${{ matrix.os }}', steps: [{run: x}]}}",
            false
        )
        .is_err());
        assert!(labels(
            "jobs: {a: {runs-on: linux, steps: [{uses: 'docker://alpine'}]}}",
            true
        )
        .is_err());
        assert!(labels(
            "jobs: {a: {uses: org/repo/.github/workflows/x.yml@main}}",
            false
        )
        .is_err());
    }
}
