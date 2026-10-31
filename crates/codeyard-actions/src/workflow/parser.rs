//! Follows actions/runner's `Sdk/WorkflowParser/workflow-v1.0.json` and `Conversion/IdBuilder.cs`.
use super::{Step, StepAction, Workflow};
use anyhow::{bail, ensure, Context, Result};
use serde_yaml::{Mapping, Value};
use std::collections::{BTreeMap, BTreeSet};

const WORKFLOW: &[&str] = &["github", "inputs", "vars"];
const NEEDS: &[&str] = &["github", "inputs", "vars", "needs"];
const MATRIX: &[&str] = &["github", "inputs", "vars", "needs", "strategy", "matrix"];
const RUNNER: &[&str] = &[
    "github", "inputs", "vars", "needs", "strategy", "matrix", "secrets", "steps", "job", "runner",
    "env",
];
const STATUS: &[&str] = &["always", "success", "failure", "cancelled"];

pub(super) fn mapping<'a>(value: &'a Value, path: &str) -> Result<&'a Mapping> {
    value
        .as_mapping()
        .with_context(|| format!("{path}: Expected a mapping"))
}

pub fn text(value: &Value, path: &str) -> Result<String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Number(n) => {
            Ok(crate::expressions::Value::Number(n.as_f64().context("Invalid number")?).to_text())
        }
        Value::Null => Ok(String::new()),
        _ => bail!("{path}: Expected a string"),
    }
}

pub(super) fn keys(map: &Mapping, allowed: &[&str], path: &str) -> Result<()> {
    for key in map.keys() {
        let key = key
            .as_str()
            .with_context(|| format!("{path}: Expected a string key"))?;
        ensure!(allowed.contains(&key), "{path}: Unexpected value '{key}'");
    }
    Ok(())
}

fn required<'a>(map: &'a Mapping, key: &str, path: &str) -> Result<&'a Value> {
    map.get(key)
        .with_context(|| format!("{path}: Required property is missing: {key}"))
}

fn nonempty(value: &Value, path: &str) -> Result<()> {
    ensure!(
        !text(value, path)?.is_empty(),
        "{path}: Unexpected value ''"
    );
    Ok(())
}

fn identifier(id: &str, path: &str) -> Result<()> {
    ensure!(
        super::valid_identifier(id),
        "{path}: The identifier '{id}' is invalid. IDs must start with a letter or '_' and contain only alphanumeric characters, '_', and '-' (less than 100 characters; '__' is reserved)."
    );
    Ok(())
}

/// Extract templates without treating a quoted `}}` inside an expression as its end.
fn expressions(template: &str) -> Result<Vec<&str>> {
    let mut result = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("${{") {
        rest = &rest[start + 3..];
        let i = crate::expressions::expression_end(rest)?;
        result.push(&rest[..i]);
        rest = &rest[i + 2..];
    }
    Ok(result)
}

fn expression_context(
    expression: &str,
    contexts: &[&str],
    functions: &[&str],
    path: &str,
) -> Result<()> {
    let bytes = expression.as_bytes();
    let mut i = 0;
    let mut previous = None;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b == b'\'' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    i += 1;
                    if bytes.get(i) != Some(&b'\'') {
                        break;
                    }
                }
                i += 1;
            }
            previous = Some(b'\'');
            continue;
        }
        if b.is_ascii_digit() {
            i += 1;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'.' | b'+' | b'-'))
            {
                i += 1;
            }
            previous = Some(b'0');
            continue;
        }
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            i += 1;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'-'))
            {
                i += 1;
            }
            let name = expression[start..i].to_ascii_lowercase();
            let next = bytes[i..].iter().find(|b| !b.is_ascii_whitespace());
            if previous != Some(b'.') {
                if next == Some(&b'(') {
                    ensure!(
                        [
                            "contains",
                            "startswith",
                            "endswith",
                            "format",
                            "join",
                            "tojson",
                            "fromjson"
                        ]
                        .contains(&name.as_str())
                            || functions.contains(&name.as_str()),
                        "{path}: Unrecognized function: '{name}'"
                    );
                } else {
                    ensure!(
                        ["true", "false", "null"].contains(&name.as_str())
                            || contexts.contains(&name.as_str()),
                        "{path}: Unrecognized named-value: '{name}'"
                    );
                }
            }
            previous = Some(b'a');
        } else {
            previous = Some(b);
            i += 1;
        }
    }
    // Validate the original grammar without spending limits on a synthetic wrapper.
    let contexts = crate::expressions::Contexts {
        values: contexts
            .iter()
            .map(|name| ((*name).to_owned(), crate::expressions::Value::Null))
            .collect(),
    };
    crate::expressions::validate(expression, &contexts)
    .with_context(|| path.to_owned())?;
    Ok(())
}

pub(super) fn templates(value: &Value, contexts: &[&str], functions: &[&str], path: &str) -> Result<()> {
    match value {
        Value::String(s) => {
            for expression in expressions(s).with_context(|| path.to_owned())? {
                ensure!(
                    !contexts.is_empty(),
                    "{path}: A template expression is not allowed in this context"
                );
                expression_context(expression, contexts, functions, path)?;
            }
        }
        Value::Sequence(values) => {
            for (i, v) in values.iter().enumerate() {
                templates(v, contexts, functions, &format!("{path}[{i}]"))?;
            }
        }
        Value::Mapping(map) => {
            for (k, v) in map {
                let key = text(k, path)?;
                ensure!(
                    !key.is_empty() && !key.contains("${{"),
                    "{path}: Invalid mapping key '{key}'"
                );
                templates(v, contexts, functions, &format!("{path}.{key}"))?;
            }
        }
        Value::Tagged(_) => bail!("{path}: Unexpected YAML tag"),
        _ => {}
    }
    Ok(())
}

fn is_template(v: &Value) -> bool {
    v.as_str().is_some_and(|s| s.contains("${{"))
}

fn boolean(value: &Value, path: &str) -> Result<()> {
    ensure!(
        value.is_bool() || is_template(value),
        "{path}: Expected a boolean"
    );
    Ok(())
}

fn number(value: &Value, path: &str) -> Result<()> {
    ensure!(
        value.is_number() || is_template(value),
        "{path}: Expected a number"
    );
    Ok(())
}

pub(super) fn strings(value: &Value, path: &str) -> Result<Vec<String>> {
    match value {
        Value::Sequence(values) => values
            .iter()
            .map(|v| {
                nonempty(v, path)?;
                text(v, path)
            })
            .collect(),
        _ => {
            nonempty(value, path)?;
            Ok(vec![text(value, path)?])
        }
    }
}

pub(super) fn string_map(value: Option<&Value>, path: &str) -> Result<BTreeMap<String, String>> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    mapping(value, path)?
        .iter()
        .map(|(k, v)| {
            nonempty(k, path)?;
            Ok((text(k, path)?, text(v, path)?))
        })
        .collect()
}

pub(super) fn environment_template(value: Option<&Value>, path: &str) -> Result<super::EnvironmentTemplate> {
    if let Some(Value::String(template)) = value {
        let template = template.trim();
        let inner = template.strip_prefix("${{").with_context(|| format!("{path}: Expected an environment mapping or whole expression"))?;
        ensure!(crate::expressions::expression_end(inner)? + 2 == inner.len(), "{path}: Expected a whole expression");
        Ok(super::EnvironmentTemplate::Expression(template.into()))
    } else {
        Ok(super::EnvironmentTemplate::Entries(string_map(value, path)?))
    }
}

fn condition(value: &Value, contexts: &[&str], hash_files: bool, path: &str) -> Result<()> {
    let s = text(value, path)?;
    if s.trim().is_empty() {
        return Ok(());
    }
    let mut functions = STATUS.to_vec();
    if hash_files {
        functions.push("hashfiles");
    }
    if s.contains("${{") {
        templates(value, contexts, &functions, path)
    } else {
        expression_context(&s, contexts, &functions, path)
    }
}

fn defaults(value: &Value, job: bool, path: &str) -> Result<()> {
    let map = mapping(value, path)?;
    keys(map, &["run"], path)?;
    if let Some(run) = map.get("run") {
        let run = mapping(run, path)?;
        keys(run, &["shell", "working-directory"], path)?;
        for v in run.values() {
            nonempty(v, path)?;
        }
    }
    let mut contexts = MATRIX.to_vec();
    contexts.extend(["env", "runner", "job"]);
    templates(value, if job { &contexts } else { &[] }, &[], path)
}

fn permissions(value: &Value, path: &str) -> Result<()> {
    templates(value, &[], &[], path)?;
    if let Value::String(s) = value {
        ensure!(
            ["read-all", "write-all"].contains(&s.as_str()),
            "{path}: Unexpected value '{s}'"
        );
        return Ok(());
    }
    let map = mapping(value, path)?;
    keys(map, super::PERMISSION_SCOPES, path)?;
    for (k, v) in map {
        let level = text(v, path)?;
        let scope = k.as_str().unwrap_or("");
        ensure!(["read", "write", "none"].contains(&level.as_str()), "{path}: Unexpected permission level '{level}'");
        // A known level the scope does not take: name the one it does.
        let other = if level == "read" { "write" } else { "read" };
        ensure!(super::permission_level_allowed(scope, &level), "{path}.{scope}: Expected '{other}' or 'none'");
    }
    Ok(())
}

fn concurrency(value: &Value, contexts: &[&str], path: &str) -> Result<()> {
    templates(value, contexts, &[], path)?;
    if let Value::Mapping(map) = value {
        keys(map, &["group", "cancel-in-progress"], path)?;
        nonempty(required(map, "group", path)?, path)?;
        if let Some(v) = map.get("cancel-in-progress") {
            boolean(v, path)?;
        }
    } else {
        nonempty(value, path)?;
    }
    Ok(())
}

fn container(value: &Value, service: bool, path: &str) -> Result<()> {
    let image = if let Some(map) = value.as_mapping() {
        map.get("image")
    } else {
        Some(value)
    };
    if let Some(image) = image {
        let image = text(image, path)?;
        let trimmed = image.strip_prefix("docker://").unwrap_or(&image);
        // An empty image is allowed, even for a service: it disables that service, as on GitHub.
        ensure!(
            !trimmed.is_empty() || image.is_empty(),
            "{path}: Container image cannot be empty"
        );
    } else {
        ensure!(!service, "{path}: Container image cannot be empty");
    }
    if let Value::Mapping(map) = value {
        keys(
            map,
            &["image", "credentials", "env", "ports", "volumes", "options"],
            path,
        )?;
        for (k, v) in map {
            let key = text(k, path)?;
            let p = format!("{path}.{key}");
            match key.as_str() {
                "credentials" => {
                    let credentials = mapping(v, &p)?;
                    keys(credentials, &["username", "password"], &p)?;
                    for v in credentials.values() {
                        nonempty(v, &p)?;
                    }
                    templates(
                        v,
                        &[
                            "github", "inputs", "vars", "needs", "strategy", "matrix", "secrets",
                            "env",
                        ],
                        &[],
                        &p,
                    )?;
                }
                "env" => {
                    string_map(Some(v), &p)?;
                    templates(v, RUNNER, &[], &p)?;
                }
                "ports" | "volumes" => {
                    ensure!(
                        v.is_sequence() || is_template(v),
                        "{p}: Expected a sequence"
                    );
                    strings(v, &p)?;
                    templates(v, MATRIX, &[], &p)?;
                }
                _ => {
                    text(v, &p)?;
                    templates(v, MATRIX, &[], &p)?;
                }
            }
        }
    } else {
        text(value, path)?;
        templates(value, MATRIX, &[], path)?;
    }
    Ok(())
}

fn strategy(value: &Value, path: &str) -> Result<()> {
    templates(value, NEEDS, &[], path)?;
    if is_template(value) {
        return Ok(());
    }
    let map = mapping(value, path)?;
    keys(map, &["matrix", "fail-fast", "max-parallel"], path)?;
    if let Some(v) = map.get("fail-fast") {
        boolean(v, &format!("{path}.fail-fast"))?;
    }
    if let Some(v) = map.get("max-parallel") {
        number(v, &format!("{path}.max-parallel"))?;
    }
    if let Some(v) = map.get("matrix") {
        if is_template(v) {
            return Ok(());
        }
        let matrix = mapping(v, &format!("{path}.matrix"))?;
        let has_vector = matrix
            .keys()
            .any(|k| !matches!(k.as_str(), Some("include" | "exclude")));
        let has_include = matrix
            .get("include")
            .is_some_and(|v| is_template(v) || v.as_sequence().is_some_and(|s| !s.is_empty()));
        ensure!(
            has_vector || has_include,
            "{path}.matrix: Matrix must define at least one vector"
        );
        for (k, v) in matrix {
            let key = text(k, path)?;
            ensure!(
                v.is_sequence() || is_template(v),
                "{path}.matrix.{key}: Expected a sequence"
            );
            if matches!(key.as_str(), "include" | "exclude") {
                if let Some(items) = v.as_sequence() {
                    for item in items {
                        if is_template(item) {
                            continue;
                        }
                        let filter = mapping(item, &format!("{path}.matrix.{key}"))?;
                        ensure!(
                            !filter.is_empty(),
                            "{path}.matrix.{key}: Matrix filter must not be empty"
                        );
                        if key == "exclude" {
                            for excluded in filter.keys() {
                                ensure!(matrix.contains_key(excluded) && !matches!(excluded.as_str(), Some("include" | "exclude")), "{path}.matrix.exclude: Matrix exclude key '{}' does not match any key within the matrix", text(excluded, path)?);
                            }
                        }
                    }
                }
            } else if let Some(values) = v.as_sequence() {
                ensure!(
                    !values.is_empty(),
                    "{path}.matrix.{key}: Matrix vector does not contain any values"
                );
            }
        }
    }
    Ok(())
}

pub(super) fn events(value: &Value) -> Result<()> {
    const EVENTS: &[&str] = &[
        "branch_protection_rule",
        "check_run",
        "check_suite",
        "create",
        "delete",
        "deployment",
        "deployment_status",
        "discussion",
        "discussion_comment",
        "fork",
        "gollum",
        "image_version",
        "issue_comment",
        "issues",
        "label",
        "merge_group",
        "milestone",
        "page_build",
        "project",
        "project_card",
        "project_column",
        "public",
        "pull_request",
        "pull_request_comment",
        "pull_request_review",
        "pull_request_review_comment",
        "pull_request_target",
        "push",
        "registry_package",
        "release",
        "repository_dispatch",
        "schedule",
        "status",
        "watch",
        "workflow_call",
        "workflow_dispatch",
        "workflow_run",
    ];
    let check_event = |name: &str| -> Result<()> {
        ensure!(EVENTS.contains(&name), "on: Unexpected event '{name}'");
        Ok(())
    };
    match value {
        Value::String(_) | Value::Sequence(_) => {
            templates(value, &[], &[], "on")?;
            let events = strings(value, "on")?;
            ensure!(!events.is_empty(), "on: Expected at least one event");
            for event in events {
                check_event(&event)?;
            }
        }
        Value::Mapping(events) => {
            ensure!(!events.is_empty(), "on: Expected at least one event");
            for (event, config) in events {
                let event = text(event, "on")?;
                check_event(&event)?;
                let p = format!("on.{event}");
                if event != "workflow_call" {
                    templates(config, &[], &[], &p)?;
                }
                if event == "schedule" {
                    let items = config
                        .as_sequence()
                        .context("on.schedule: Expected a sequence")?;
                    ensure!(
                        !items.is_empty(),
                        "on.schedule: Expected at least one schedule"
                    );
                    for item in items {
                        let item = mapping(item, &p)?;
                        keys(item, &["cron"], &p)?;
                        let cron = text(required(item, "cron", &p)?, &p)?;
                        ensure!(
                            cron.split_whitespace().count() == 5,
                            "{p}.cron: Invalid cron expression '{cron}'"
                        );
                    }
                    continue;
                }
                if config.is_null() {
                    continue;
                }
                let map = mapping(config, &p)?;
                if event == "workflow_dispatch" {
                    super::dispatch_inputs::parse(config)?;
                    continue;
                }
                if event == "workflow_call" {
                    keys(
                        map,
                        if event == "workflow_call" {
                            &["inputs", "secrets", "outputs"]
                        } else {
                            &["inputs"]
                        },
                        &p,
                    )?;
                    if let Some(inputs) = map.get("inputs") {
                        for (id, input) in mapping(inputs, &p)? {
                            nonempty(id, &p)?;
                            templates(id, &[], &[], &p)?;
                            let p = format!("{p}.inputs.{}", text(id, &p)?);
                            let input = mapping(input, &p)?;
                            keys(
                                input,
                                &["description", "type", "required", "default", "options"],
                                &p,
                            )?;
                            let kind = input.get("type").map(|v| text(v, &p)).transpose()?;
                            if event == "workflow_call" {
                                required(input, "type", &p)?;
                                ensure!(
                                    !input.contains_key("options"),
                                    "{p}: Unexpected value 'options'"
                                );
                            }
                            if let Some(kind) = &kind {
                                ensure!(
                                    ["string", "boolean", "number"].contains(&kind.as_str())
                                        || (event == "workflow_dispatch"
                                            && ["choice", "environment"].contains(&kind.as_str())),
                                    "{p}.type: Unexpected value '{kind}'"
                                );
                            }
                            for (key, value) in input {
                                templates(
                                    value,
                                    if key.as_str() == Some("default") && event == "workflow_call" {
                                        WORKFLOW
                                    } else {
                                        &[]
                                    },
                                    &[],
                                    &p,
                                )?;
                            }
                            if let Some(v) = input.get("description") {
                                text(v, &p)?;
                            }
                            if let Some(v) = input.get("required") {
                                boolean(v, &p)?;
                            }
                            if let Some(v) = input.get("default") {
                                match kind.as_deref() {
                                    Some("boolean") => boolean(v, &p)?,
                                    Some("number") => number(v, &p)?,
                                    _ => {
                                        text(v, &p)?;
                                    }
                                }
                            }
                            if let Some(v) = input.get("options") {
                                ensure!(v.is_sequence(), "{p}.options: Expected a sequence");
                                strings(v, &p)?;
                            }
                            if kind.as_deref() == Some("choice") {
                                required(input, "options", &p)?;
                            }
                        }
                    }
                    if let Some(secrets) = map.get("secrets") {
                        for (id, secret) in mapping(secrets, &p)? {
                            nonempty(id, &p)?;
                            templates(id, &[], &[], &p)?;
                            if secret.is_null() {
                                continue;
                            }
                            let secret = mapping(secret, &p)?;
                            keys(secret, &["description", "required"], &p)?;
                            templates(&Value::Mapping(secret.clone()), &[], &[], &p)?;
                            if let Some(v) = secret.get("description") {
                                text(v, &p)?;
                            }
                            if let Some(v) = secret.get("required") {
                                boolean(v, &p)?;
                            }
                        }
                    }
                    if let Some(outputs) = map.get("outputs") {
                        for (id, output) in mapping(outputs, &p)? {
                            nonempty(id, &p)?;
                            templates(id, &[], &[], &p)?;
                            let output = mapping(output, &p)?;
                            keys(output, &["description", "value"], &p)?;
                            text(required(output, "value", &p)?, &p)?;
                            templates(
                                required(output, "value", &p)?,
                                &["github", "inputs", "vars", "jobs"],
                                &[],
                                &p,
                            )?;
                            if let Some(description) = output.get("description") {
                                text(description, &p)?;
                                templates(description, &[], &[], &p)?;
                            }
                        }
                    }
                } else {
                    let allowed: &[&str] = match event.as_str() {
                        "push" => &[
                            "branches",
                            "branches-ignore",
                            "tags",
                            "tags-ignore",
                            "paths",
                            "paths-ignore",
                        ],
                        "pull_request" | "pull_request_target" => &[
                            "types",
                            "branches",
                            "branches-ignore",
                            "paths",
                            "paths-ignore",
                        ],
                        "workflow_run" => &["types", "workflows", "branches", "branches-ignore"],
                        _ => &["types"],
                    };
                    keys(map, allowed, &p)?;
                    for (k, v) in map {
                        ensure!(v.is_sequence(), "{p}.{}: Expected a sequence", text(k, &p)?);
                        strings(v, &p)?;
                    }
                    for filter in ["branches", "tags", "paths"] {
                        ensure!(!(map.contains_key(filter) && map.contains_key(format!("{filter}-ignore"))), "{p}: You cannot use both '{filter}' and '{filter}-ignore' for the same event");
                    }
                }
            }
        }
        _ => bail!("on: Expected an event name, sequence, or mapping"),
    }
    Ok(())
}

pub(super) fn parse(yaml: &str) -> Result<Workflow> {
    let root: Value = serde_yaml::from_str(yaml).context("Invalid workflow YAML")?;
    let root = mapping(&root, "workflow")?;
    keys(
        root,
        &[
            "name",
            "description",
            "run-name",
            "on",
            "env",
            "defaults",
            "permissions",
            "concurrency",
            "jobs",
        ],
        "workflow",
    )?;
    super::events::parse(Some(required(root, "on", "workflow")?))?;
    for (k, v) in root {
        match k.as_str() {
            Some("name" | "description") => {
                let path = k.as_str().unwrap_or("name");
                text(v, path)?;
                templates(v, &[], &[], path)?;
            }
            Some("run-name") => {
                text(v, "run-name")?;
                templates(v, WORKFLOW, &[], "run-name")?;
            }
            Some("env") => {
                environment_template(Some(v), "env")?;
                templates(v, &["github", "inputs", "vars", "secrets"], &[], "env")?;
            }
            Some("defaults") => defaults(v, false, "defaults")?,
            Some("permissions") => permissions(v, "permissions")?,
            Some("concurrency") => concurrency(v, WORKFLOW, "concurrency")?,
            _ => {}
        }
    }
    let jobs_map = mapping(required(root, "jobs", "workflow")?, "jobs")?;
    ensure!(
        !jobs_map.is_empty(),
        "jobs: A workflow must contain at least one job"
    );
    let mut jobs = Vec::new();
    let mut ids = BTreeSet::new();
    for (id, value) in jobs_map {
        let id = text(id, "jobs")?;
        let p = format!("jobs.{id}");
        identifier(&id, &p)?;
        ensure!(
            ids.insert(id.to_ascii_lowercase()),
            "{p}: The identifier '{id}' may not be used more than once within the same scope"
        );
        let job = mapping(value, &p)?;
        validate_job(job, &p)?;
        jobs.push((id, job.clone()));
    }
    let workflow = Workflow {
        fields: root.clone(),
        jobs,
    };
    super::job_order(&workflow)?;
    Ok(workflow)
}

fn validate_job(job: &Mapping, path: &str) -> Result<()> {
    let reusable = job.contains_key("uses");
    keys(
        job,
        if reusable {
            &[
                "name",
                "uses",
                "with",
                "secrets",
                "needs",
                "if",
                "permissions",
                "concurrency",
                "strategy",
            ]
        } else {
            &[
                "name",
                "needs",
                "if",
                "runs-on",
                "environment",
                "strategy",
                "env",
                "defaults",
                "container",
                "services",
                "steps",
                "outputs",
                "timeout-minutes",
                "continue-on-error",
                "permissions",
                "concurrency",
                "snapshot",
            ]
        },
        path,
    )?;
    if reusable {
        nonempty(required(job, "uses", path)?, path)?;
    } else {
        required(job, "runs-on", path)?;
        let steps = required(job, "steps", path)?
            .as_sequence()
            .with_context(|| format!("{path}.steps: Expected a sequence"))?;
        ensure!(
            !steps.is_empty(),
            "{path}.steps: Expected at least one step"
        );
        ensure!(steps.len() <= super::MAX_JOB_STEPS, "{path}.steps: Too many steps");
        let mut ids = BTreeSet::new();
        for (i, step) in steps.iter().enumerate() {
            let step = parse_step(step).with_context(|| format!("{path}.steps[{i}]"))?;
            if let Some(id) = step.id {
                ensure!(ids.insert(id.to_ascii_lowercase()), "{path}.steps[{i}]: The identifier '{id}' may not be used more than once within the same scope");
            }
        }
    }
    for (k, v) in job {
        let key = text(k, path)?;
        let p = format!("{path}.{key}");
        match key.as_str() {
            "needs" => {
                strings(v, &p)?;
                templates(v, &[], &[], &p)?;
            }
            "if" => condition(v, NEEDS, false, &p)?,
            "uses" => templates(v, &[], &[], &p)?,
            "strategy" => strategy(v, &p)?,
            "permissions" => permissions(v, &p)?,
            "concurrency" => concurrency(v, MATRIX, &p)?,
            "defaults" => defaults(v, true, &p)?,
            "env" => {
                environment_template(Some(v), &p)?;
                templates(
                    v,
                    &[
                        "github", "inputs", "vars", "needs", "strategy", "matrix", "secrets",
                    ],
                    &[],
                    &p,
                )?;
            }
            "outputs" => {
                string_map(Some(v), &p)?;
                templates(v, RUNNER, &[], &p)?;
            }
            "container" => container(v, false, &p)?,
            "services" => {
                for (id, value) in mapping(v, &p)? {
                    nonempty(id, &p)?;
                    container(value, true, &format!("{p}.{}", text(id, &p)?))?;
                }
            }
            "runs-on" => {
                templates(v, MATRIX, &[], &p)?;
                if let Value::Mapping(map) = v {
                    keys(map, &["group", "labels"], &p)?;
                    ensure!(!map.is_empty(), "{p}: Expected 'group' or 'labels'");
                    if let Some(v) = map.get("group") {
                        nonempty(v, &p)?;
                    }
                    if let Some(v) = map.get("labels") {
                        strings(v, &p)?;
                    }
                } else {
                    strings(v, &p)?;
                }
            }
            "environment" => {
                if let Value::Mapping(map) = v {
                    keys(map, &["name", "url"], &p)?;
                    text(required(map, "name", &p)?, &p)?;
                    templates(required(map, "name", &p)?, MATRIX, &[], &p)?;
                    if let Some(url) = map.get("url") {
                        text(url, &p)?;
                        let contexts: Vec<_> =
                            RUNNER.iter().copied().filter(|s| *s != "secrets").collect();
                        templates(url, &contexts, &[], &p)?;
                    }
                } else {
                    text(v, &p)?;
                    templates(v, MATRIX, &[], &p)?;
                }
            }
            "name" => {
                text(v, &p)?;
                templates(v, MATRIX, &[], &p)?;
            }
            "timeout-minutes" => {
                number(v, &p)?;
                templates(v, MATRIX, &[], &p)?;
            }
            "continue-on-error" => {
                boolean(v, &p)?;
                templates(v, MATRIX, &[], &p)?;
            }
            // A custom image to create from the runner when the job ends.
            "snapshot" => match v {
                Value::Mapping(map) => {
                    keys(map, &["image-name", "version", "if"], &p)?;
                    let name = required(map, "image-name", &p)?;
                    nonempty(name, &p)?;
                    templates(name, MATRIX, &[], &p)?;
                    if let Some(version) = map.get("version") {
                        nonempty(version, &p)?;
                        templates(version, MATRIX, &[], &p)?;
                    }
                    if let Some(condition) = map.get("if") {
                        self::condition(condition, MATRIX, false, &p)?;
                    }
                }
                _ => {
                    nonempty(v, &p)?;
                    templates(v, MATRIX, &[], &p)?;
                }
            },
            "with" => {
                string_map(Some(v), &p)?;
                templates(v, MATRIX, &[], &p)?;
            }
            "secrets" => {
                if v.as_str() == Some("inherit") {
                    continue;
                }
                string_map(Some(v), &p)?;
                templates(
                    v,
                    &[
                        "github", "inputs", "vars", "needs", "strategy", "matrix", "secrets",
                    ],
                    &[],
                    &p,
                )?;
            }
            _ => {}
        }
    }
    Ok(())
}

pub(super) fn parse_step(value: &Value) -> Result<Step> {
    let map = mapping(value, "step")?;
    ensure!(
        !(map.contains_key("run") && map.contains_key("uses")),
        "step: A step cannot have both 'run' and 'uses'"
    );
    let run = map.contains_key("run");
    keys(
        map,
        if run {
            &[
                "id",
                "name",
                "if",
                "run",
                "shell",
                "working-directory",
                "env",
                "continue-on-error",
                "timeout-minutes",
            ]
        } else {
            &[
                "id",
                "name",
                "if",
                "uses",
                "with",
                "env",
                "continue-on-error",
                "timeout-minutes",
            ]
        },
        "step",
    )?;
    let get = |key: &str| {
        map.get(key)
            .map(|v| text(v, &format!("step.{key}")))
            .transpose()
    };
    let id = get("id")?;
    if let Some(id) = &id {
        identifier(id, "step.id")?;
    }
    for (k, v) in map {
        let key = text(k, "step")?;
        let p = format!("step.{key}");
        match key.as_str() {
            "id" | "uses" => templates(v, &[], &[], &p)?,
            "if" => {
                let contexts: Vec<_> = RUNNER.iter().copied().filter(|s| *s != "secrets").collect();
                condition(v, &contexts, true, &p)?;
            }
            _ => templates(v, RUNNER, &["hashfiles"], &p)?,
        }
        match key.as_str() {
            "continue-on-error" => boolean(v, &p)?,
            "timeout-minutes" => number(v, &p)?,
            "env" => { environment_template(Some(v), &p)?; }
            "with" => { string_map(Some(v), &p)?; }
            "shell" | "uses" => nonempty(v, &p)?,
            _ => {}
        }
    }
    if !run {
        let reference = text(required(map, "uses", "step")?, "step.uses")?;
        if !reference.starts_with("./")
            && !reference.starts_with(".\\")
            && !reference.starts_with("docker://")
        {
            crate::runner::ActionReference::parse(&reference).context("step.uses")?;
        }
    }
    let action = if run {
        StepAction::Run {
            script: text(required(map, "run", "step")?, "step.run")?,
            shell: get("shell")?,
            working_directory: get("working-directory")?,
        }
    } else {
        StepAction::Uses {
            reference: text(required(map, "uses", "step")?, "step.uses")?,
            with: string_map(map.get("with"), "step.with")?,
        }
    };
    Ok(Step {
        id,
        name: get("name")?,
        condition: get("if")?,
        action,
        env: environment_template(map.get("env"), "step.env")?,
        continue_on_error: get("continue-on-error")?,
        timeout_minutes: get("timeout-minutes")?,
    })
}
