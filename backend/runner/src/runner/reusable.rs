//! Called workflow files are read before any job can change the checkout.
use super::*;
use anyhow::ensure;
use serde_yaml::{Mapping, Value as Yaml};
use std::collections::BTreeSet;

pub(super) fn files(root: &Yaml, directory: &Path) -> Result<BTreeMap<String, Yaml>> {
    fn visit(
        workflow: &Yaml,
        directory: &Path,
        depth: usize,
        active: &mut BTreeSet<String>,
        result: &mut BTreeMap<String, Yaml>,
        heights: &mut BTreeMap<String, usize>,
    ) -> Result<usize> {
        let mut height = 0;
        let jobs = workflow
            .get("jobs")
            .and_then(Yaml::as_mapping)
            .context("Missing jobs")?;
        for job in jobs.values() {
            let Some(reference) = job.get("uses").and_then(Yaml::as_str) else {
                continue;
            };
            ensure!(
                depth + 1 < crate::workflow::MAX_REUSABLE_LEVELS,
                "Reusable workflows exceed four levels"
            );
            ensure!(active.insert(reference.into()), "Reusable workflow cycle");
            if !result.contains_key(reference) {
                ensure!(
                    result.len() < crate::workflow::MAX_CALLED_WORKFLOWS,
                    "More than twenty called workflows"
                );
                let path = reference
                    .strip_prefix("./.github/workflows/")
                    .context("Remote reusable workflows must be resolved by the Codeyard server")?;
                ensure!(
                    !path.is_empty()
                        && !path.contains('/')
                        && !path.contains('\\')
                        && !path.contains(".."),
                    "Invalid called workflow path"
                );
                let path = directory
                    .join(".github/workflows")
                    .join(path)
                    .canonicalize()?;
                ensure!(
                    path.starts_with(directory),
                    "Called workflow escapes checkout"
                );
                let source = std::fs::read_to_string(path)?;
                let parsed = workflow::parse(&source)?;
                ensure!(
                    workflow::listens_to(&parsed, "workflow_call"),
                    "Called workflow does not declare workflow_call"
                );
                let called = Yaml::Mapping(parsed.fields);
                workflow::validate_called_concurrency(&called)?;
                result.insert(reference.into(), called);
            }
            let child_height = if let Some(height) = heights.get(reference) {
                *height
            } else {
                let called = result[reference].clone();
                let height = visit(&called, directory, depth + 1, active, result, heights)?;
                heights.insert(reference.into(), height);
                height
            };
            ensure!(
                depth + 1 + child_height < crate::workflow::MAX_REUSABLE_LEVELS,
                "Reusable workflows exceed four levels"
            );
            height = height.max(child_height + 1);
            active.remove(reference);
        }
        Ok(height)
    }
    let mut result = BTreeMap::new();
    visit(
        root,
        directory,
        0,
        &mut BTreeSet::new(),
        &mut result,
        &mut BTreeMap::new(),
    )?;
    Ok(result)
}

fn evaluate(value: &Yaml, contexts: &Contexts) -> Result<Value> {
    match value {
        Yaml::String(value) => Ok(expressions::interpolate(
            value,
            contexts,
            Status::default(),
        )?),
        _ => Ok(Value::from_json(&serde_json::to_value(value)?)),
    }
}

pub(super) fn contexts(
    call: &Mapping,
    called: &Yaml,
    caller: &Contexts,
    masker: &Masker,
) -> Result<Contexts> {
    let contract = called.get("on").and_then(|on| on.get("workflow_call"));
    let declared = contract
        .and_then(|value| value.get("inputs"))
        .and_then(Yaml::as_mapping);
    let supplied = call.get("with").and_then(Yaml::as_mapping);
    let mut inputs = BTreeMap::new();
    for key in supplied.into_iter().flat_map(|values| values.keys()) {
        ensure!(
            declared.is_some_and(|values| values.contains_key(key)),
            "Unknown workflow_call input"
        );
    }
    let mut defaults = Vec::new();
    for (key, spec) in declared.into_iter().flatten() {
        let name = key.as_str().context("Invalid input name")?;
        let kind = spec
            .get("type")
            .and_then(Yaml::as_str)
            .context("Missing input type")?;
        let value = if let Some(value) = supplied.and_then(|values| values.get(key)) {
            // Arguments belong to the caller; defaults belong to the callee.
            evaluate(value, caller)?
        } else {
            if let Some(value) = spec.get("default") {
                defaults.push((name.to_owned(), value));
            } else {
                ensure!(
                    spec.get("required").and_then(Yaml::as_bool) != Some(true),
                    "Missing required workflow_call input: {name}"
                );
            }
            match kind {
                "boolean" => Value::Bool(false),
                "number" => Value::Number(0.0),
                _ => Value::String(String::new()),
            }
        };
        inputs.insert(name.into(), value);
    }
    // Re-evaluate defaults against a complete callee scope until dependencies stabilize.
    // Acyclic dependencies need at most one pass per default plus a stability check.
    let mut settled = defaults.is_empty();
    for _ in 0..=defaults.len() {
        let mut context = caller.clone();
        context
            .values
            .insert("inputs".into(), Value::Object(inputs.clone()));
        let mut next = inputs.clone();
        let mut error = None;
        for (name, value) in &defaults {
            match evaluate(value, &context) {
                Ok(value) => {
                    next.insert(name.clone(), value);
                }
                Err(cause) => {
                    error = Some(cause);
                }
            }
        }
        if next == inputs {
            if let Some(error) = error {
                return Err(error);
            }
            settled = true;
            break;
        }
        inputs = next;
    }
    ensure!(settled, "Workflow_call input defaults do not converge");
    for (key, spec) in declared.into_iter().flatten() {
        let name = key.as_str().context("Invalid input name")?;
        let kind = spec
            .get("type")
            .and_then(Yaml::as_str)
            .context("Missing input type")?;
        ensure!(
            matches!(
                (kind, &inputs[name]),
                ("string", Value::String(_))
                    | ("boolean", Value::Bool(_))
                    | ("number", Value::Number(_))
            ),
            "Wrong workflow_call input type: {name}"
        );
    }
    let inherited = call.get("secrets").and_then(Yaml::as_str) == Some("inherit");
    let mut secrets = if inherited {
        match caller.values.get("secrets") {
            Some(Value::Object(values)) => values.clone(),
            _ => BTreeMap::new(),
        }
    } else {
        BTreeMap::new()
    };
    let declared = contract
        .and_then(|value| value.get("secrets"))
        .and_then(Yaml::as_mapping);
    if let Some(supplied) = call.get("secrets").and_then(Yaml::as_mapping) {
        for (key, value) in supplied {
            ensure!(
                declared.is_some_and(|values| values.keys().any(|name| name
                    .as_str()
                    .zip(key.as_str())
                    .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b)))),
                "Unknown workflow_call secret"
            );
            let value = evaluate(value, caller)?;
            if !masker.add_requested(&value.to_text()) {
                masker.hide_all();
                anyhow::bail!("Called workflow secrets exceed the masking budget");
            }
            secrets.insert(key.as_str().context("Invalid secret name")?.into(), value);
        }
    }
    for (key, spec) in declared.into_iter().flatten() {
        let name = key.as_str().context("Invalid secret name")?;
        ensure!(
            spec.get("required").and_then(Yaml::as_bool) != Some(true)
                || secrets
                    .iter()
                    .any(|(key, value)| key.eq_ignore_ascii_case(name)
                        && !value.to_text().is_empty()),
            "Missing required workflow_call secret: {name}"
        );
    }
    if let Some(Value::Object(values)) = caller.values.get("secrets") {
        if let Some(token) = values.get("GITHUB_TOKEN") {
            secrets.insert("GITHUB_TOKEN".into(), token.clone());
        }
    }
    let mut contexts = caller.clone();
    contexts
        .values
        .insert("inputs".into(), Value::Object(inputs));
    contexts
        .values
        .insert("secrets".into(), Value::Object(secrets));
    for name in [
        "steps", "needs", "matrix", "strategy", "job", "runner", "jobs",
    ] {
        contexts
            .values
            .insert(name.into(), Value::Object(BTreeMap::new()));
    }
    Ok(contexts)
}

pub(super) fn outputs(
    raw: &Yaml,
    contexts: &Contexts,
    masker: &Masker,
) -> Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    let outputs = raw
        .get("on")
        .and_then(|on| on.get("workflow_call"))
        .and_then(|call| call.get("outputs"))
        .and_then(Yaml::as_mapping);
    for (key, spec) in outputs.into_iter().flatten() {
        let value = crate::workflow::scalar_text(
            spec.get("value").context("Missing workflow output value")?,
            "workflow output",
        )?;
        let value = text(&value, contexts, Status::default())?;
        if !masker.contains(&value) {
            result.insert(key.as_str().context("Invalid output name")?.into(), value);
        }
    }
    Ok(result)
}

#[cfg(test)]
#[path = "reusable_tests.rs"]
mod tests;
