//! Workflow concurrency combines immutable admission inputs with transaction-assigned run identity.
use crate::ci_admission::AdmissionError;
use crate::ci_concurrency::Configuration;
use codeyard_actions::expressions::{self, Contexts, Status};

pub async fn resolve(
    value: serde_yaml::Value,
    contexts: Contexts,
) -> Result<Configuration, AdmissionError> {
    let permit = codeyard_model::workflow_yaml::PARSING_SLOTS
        .acquire()
        .await
        .map_err(|_| AdmissionError::Unavailable)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        resolve_group(&value, &contexts)
    })
    .await
    .map_err(|_| AdmissionError::Unavailable)?
}

pub fn run_contexts(
    mut contexts: Contexts,
    run: &crate::models::CiRun,
    original_actor: Option<(i64, String)>,
) -> Contexts {
    use expressions::Value;
    let Some(Value::Object(github)) = contexts.values.get_mut("github") else {
        return contexts;
    };
    github.insert(
        "run_id".into(),
        Value::String(run.workflow_run_id.unwrap_or(run.id).to_string()),
    );
    github.insert(
        "run_number".into(),
        Value::String(run.workflow_run_number.unwrap_or(run.id).to_string()),
    );
    github.insert(
        "run_attempt".into(),
        Value::String(run.workflow_run_attempt.unwrap_or(1).to_string()),
    );
    if let Some((id, actor)) = original_actor {
        if let Some(triggering) = github.get("actor").cloned() {
            github.insert("triggering_actor".into(), triggering);
        }
        github.insert("actor".into(), Value::String(actor));
        github.insert("actor_id".into(), Value::String(id.to_string()));
    }
    if run.event == "pull_request" {
        if let Some(pull) = &run.pull_request_event {
            if let Some(number) = pull["number"].as_i64() {
                let reference = format!("refs/pull/{number}/merge");
                github.insert("ref".into(), Value::String(reference.clone()));
                github.insert("ref_name".into(), Value::String(format!("{number}/merge")));
                github.insert("head_ref".into(), Value::from_json(&pull["head_ref"]));
                github.insert("base_ref".into(), Value::String(run.branch.clone()));
                if let Some(Value::String(workflow_ref)) = github.get_mut("workflow_ref") {
                    if let Some((path, _)) = workflow_ref.split_once('@') {
                        *workflow_ref = format!("{path}@{reference}");
                    }
                }
                let event = github
                    .entry("event".into())
                    .or_insert_with(|| Value::Object(Default::default()));
                if let Value::Object(event) = event {
                    let pull = event
                        .entry("pull_request".into())
                        .or_insert_with(|| Value::Object(Default::default()));
                    if let Value::Object(pull) = pull {
                        pull.insert("number".into(), Value::Number(number as f64));
                    }
                }
            }
        }
    }
    contexts
}

fn resolve_group(
    value: &serde_yaml::Value,
    contexts: &Contexts,
) -> Result<Configuration, AdmissionError> {
    let invalid = || AdmissionError::Invalid("Invalid workflow concurrency");
    let group = value
        .as_str()
        .or_else(|| value.get("group").and_then(serde_yaml::Value::as_str))
        .ok_or_else(invalid)?;
    let group = expressions::interpolate_bounded(
        group,
        contexts,
        Status::default(),
        codeyard_actions::workflow::MAX_CONCURRENCY_GROUP_BYTES,
    )
    .map_err(|_| invalid())?
    .to_text();
    let cancel_in_progress = match value.get("cancel-in-progress") {
        None => false,
        Some(serde_yaml::Value::Bool(cancel)) => *cancel,
        Some(serde_yaml::Value::String(cancel)) if cancel.contains("${{") => {
            match expressions::interpolate(cancel, contexts, Status::default())
                .map_err(|_| invalid())?
            {
                expressions::Value::Bool(cancel) => cancel,
                _ => return Err(invalid()),
            }
        }
        _ => return Err(invalid()),
    };
    let config = Configuration {
        group,
        cancel_in_progress,
    };
    config.validate().map_err(|_| invalid())?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_and_cancellation_use_admission_contexts() {
        let contexts = Contexts {
            values: std::collections::BTreeMap::from([
                (
                    "github".into(),
                    expressions::Value::from_json(
                        &serde_json::json!({"workflow":"Deploy", "ref":"refs/heads/main"}),
                    ),
                ),
                (
                    "inputs".into(),
                    expressions::Value::from_json(&serde_json::json!({"cancel":true})),
                ),
            ]),
        };
        let yaml = serde_yaml::from_str("group: '${{ github.workflow }}-${{ github.ref }}'\ncancel-in-progress: ${{ inputs.cancel }}").unwrap();
        let config = resolve_group(&yaml, &contexts).unwrap();
        assert_eq!(config.group, "Deploy-refs/heads/main");
        assert!(config.cancel_in_progress);
        for yaml in [
            "''",
            "{}",
            "{group: name, cancel-in-progress: 'yes'}",
            "'${{ secrets.TOKEN }}'",
            "'${{ github.workflow'",
            "\"line\\nname\"",
        ] {
            let yaml = serde_yaml::from_str(yaml).unwrap();
            assert!(resolve_group(&yaml, &contexts).is_err());
        }
    }
}
