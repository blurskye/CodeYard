//! Workflow concurrency is resolved before enqueue, using only admission-time contexts.
use super::admission::AdmissionError;
use crate::backend::db::action_concurrency::Configuration;
use codeyard_actions::expressions::{self, Contexts, Status};

pub(super) async fn resolve(
    value: serde_yaml::Value,
    contexts: Contexts,
) -> Result<Configuration, AdmissionError> {
    let permit = super::workflows::PARSING_SLOTS.acquire().await
        .map_err(|_| AdmissionError::Unavailable)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        resolve_group(&value, &contexts)
    }).await.map_err(|_| AdmissionError::Unavailable)?
}

fn resolve_group(value: &serde_yaml::Value, contexts: &Contexts) -> Result<Configuration, AdmissionError> {
    let invalid = || AdmissionError::Invalid("Invalid workflow concurrency");
    let group = value
        .as_str()
        .or_else(|| value.get("group").and_then(serde_yaml::Value::as_str))
        .ok_or_else(invalid)?;
    let group = expressions::interpolate_bounded(group, contexts, Status::default(), 256)
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
