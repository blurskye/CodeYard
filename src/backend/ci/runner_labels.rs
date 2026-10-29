//! `runs-on` labels of a workflow, and which job features each mode supports.
use anyhow::{bail, ensure, Context, Result};
use serde_yaml::Value;
use std::collections::BTreeSet;

/// Literal `runs-on` labels used by the workflow's jobs. In local mode jobs run
/// directly on the worker host, so container jobs and Docker actions are refused.
/// Workers call this on the admitted snapshot, where every called workflow is local.
pub(super) fn labels_from_value(workflow: &Value, local: bool) -> Result<BTreeSet<String>> {
    let jobs = workflow.get("jobs").and_then(Value::as_mapping);
    for reference in jobs
        .into_iter()
        .flatten()
        .filter_map(|(_, job)| job.get("uses"))
    {
        ensure!(
            reference
                .as_str()
                .is_some_and(|value| value.starts_with("./.github/workflows/")),
            "called workflows must be resolved before execution"
        );
    }
    Ok(scheduling_from_value(workflow, local)?
        .labels
        .into_iter()
        .collect())
}

pub(super) fn scheduling_from_value(
    workflow: &Value,
    local: bool,
) -> Result<crate::backend::db::runners::Scheduling> {
    let jobs = workflow
        .get("jobs")
        .and_then(Value::as_mapping)
        .context("the workflow has no jobs")?;
    ensure!(!jobs.is_empty(), "the workflow has no jobs");
    let mut labels = BTreeSet::new();
    let mut group: Option<String> = None;
    for job in jobs.values() {
        // Called workflows bring their own jobs; admission resolves and pins them.
        if job.get("uses").is_some() {
            continue;
        }
        if local {
            ensure!(
                job.get("container").is_none() && job.get("services").is_none_or(|services| services.as_mapping().is_some_and(|services| services.is_empty())),
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
        let selected = runs_on
            .get("group")
            .map(|g| g.as_str().context("runner group must be a literal string"))
            .transpose()?;
        if let Some(selected) = selected {
            ensure!(
                crate::backend::db::runners::valid_group_name(selected)
                    && !selected.contains("${{"),
                "runner group must be a literal string"
            );
            ensure!(
                group
                    .as_deref()
                    .is_none_or(|g| g.eq_ignore_ascii_case(selected)),
                "a run must use one runner group"
            );
            group = Some(selected.to_owned());
        }
        let runs_on = if runs_on.is_mapping() {
            runs_on.get("labels")
        } else {
            Some(runs_on)
        };
        let values = match runs_on {
            Some(value @ Value::String(_)) => vec![value],
            Some(Value::Sequence(values)) => values.iter().collect(),
            None if selected.is_some() => Vec::new(),
            _ => bail!("`runs-on` must be a label or a list of labels"),
        };
        ensure!(
            !values.is_empty() || selected.is_some(),
            "`runs-on` must name a label"
        );
        for value in values {
            let label = value
                .as_str()
                .context("`runs-on` must be a literal label")?;
            ensure!(
                crate::backend::db::runners::valid_label(label),
                "`runs-on` must be a literal label, not an expression"
            );
            labels.insert(label.to_ascii_lowercase());
        }
    }
    Ok(crate::backend::db::runners::Scheduling {
        labels: labels.into_iter().collect(),
        group,
        concurrency: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(yaml: &str, local: bool) -> Result<BTreeSet<String>> {
        labels_from_value(&serde_yaml::from_str::<Value>(yaml).unwrap(), local)
    }

    #[test]
    fn group_selection_is_literal_and_preserves_label_matching() {
        let value: Value = serde_yaml::from_str(
            "jobs: {a: {runs-on: {group: Builders, labels: [self-hosted, Linux]}}}",
        )
        .unwrap();
        let selected = scheduling_from_value(&value, false).unwrap();
        assert_eq!(selected.group.as_deref(), Some("Builders"));
        assert_eq!(selected.labels, ["linux", "self-hosted"]);
        let value: Value = serde_yaml::from_str("jobs: {a: {runs-on: {group: Builders}}}").unwrap();
        assert!(scheduling_from_value(&value, false)
            .unwrap()
            .labels
            .is_empty());
        for yaml in [
            "jobs: {a: {runs-on: {group: '${{ inputs.group }}'}}}",
            "jobs: {a: {runs-on: {group: One}}, b: {runs-on: {group: Two}}}",
        ] {
            assert!(
                scheduling_from_value(&serde_yaml::from_str::<Value>(yaml).unwrap(), false)
                    .is_err()
            );
        }
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
