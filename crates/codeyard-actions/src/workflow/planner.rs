//! Follows actions/runner's `Sdk/WorkflowParser/Conversion/{MatrixBuilder,JobNameBuilder}.cs`.
use super::parser::{environment_template, keys, mapping, string_map, strings, text};
use super::{parse_step, Concurrency, Container, PlannedJob, Workflow};
use crate::expressions::{evaluate, evaluate_condition, interpolate, Contexts, Status, Value};
use anyhow::{bail, ensure, Context, Result};
use serde_yaml::{Mapping, Value as Yaml};
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn job_order(workflow: &Workflow) -> Result<Vec<String>> {
    let jobs: BTreeMap<_, _> = workflow.jobs.iter().map(|(id, job)| (id.to_ascii_lowercase(), (id, job))).collect();
    let mut result = Vec::new();
    let mut visiting = BTreeSet::new();
    let mut done = BTreeSet::new();
    for (root, _) in &workflow.jobs {
        let mut pending = vec![(root.clone(), false)];
        while let Some((id, exiting)) = pending.pop() {
            if exiting {
                visiting.remove(&id);
                done.insert(id.clone());
                result.push(id);
                continue;
            }
            if done.contains(&id) { continue; }
            ensure!(visiting.insert(id.clone()), "jobs.{id}.needs: The workflow contains a cycle in its job dependencies");
            pending.push((id.clone(), true));
            let (_, job) = jobs[&id.to_ascii_lowercase()];
            for need in needs(job)?.into_iter().rev() {
                let (canonical, _) = jobs.get(&need.to_ascii_lowercase()).with_context(|| format!("jobs.{id}.needs: Job '{id}' needs unknown job '{need}'"))?;
                pending.push(((*canonical).clone(), false));
            }
        }
    }
    Ok(result)
}

pub(super) fn needs(job: &Mapping) -> Result<Vec<String>> {
    job.get("needs")
        .map(|v| strings(v, "needs"))
        .transpose()
        .map(Option::unwrap_or_default)
}

fn expression_value(value: &Yaml) -> Result<Value> {
    match value {
        Yaml::Null => Ok(Value::Null),
        Yaml::Bool(b) => Ok(Value::Bool(*b)),
        Yaml::Number(n) => Ok(Value::Number(n.as_f64().context("Invalid number")?)),
        Yaml::String(s) => Ok(Value::String(s.clone())),
        Yaml::Sequence(values) => Ok(Value::Array(
            values.iter().map(expression_value).collect::<Result<_>>()?,
        )),
        Yaml::Mapping(map) => Ok(Value::Object(
            map.iter()
                .map(|(k, v)| Ok((text(k, "matrix key")?, expression_value(v)?)))
                .collect::<Result<_>>()?,
        )),
        Yaml::Tagged(_) => bail!("YAML tags are not supported in workflows"),
    }
}

fn yaml_value(value: Value) -> Result<Yaml> {
    // Serializing serde_json::Number through YAML leaks its arbitrary-precision marker.
    match value {
        Value::Null => Ok(Yaml::Null),
        Value::Bool(value) => Ok(Yaml::Bool(value)),
        Value::Number(value) => serde_yaml::to_value(value).context("Invalid expression number"),
        Value::String(value) => Ok(Yaml::String(value)),
        Value::Array(values) => Ok(Yaml::Sequence(
            values.into_iter().map(yaml_value).collect::<Result<_>>()?,
        )),
        Value::Object(values) => Ok(Yaml::Mapping(
            values
                .into_iter()
                .map(|(key, value)| Ok((Yaml::String(key), yaml_value(value)?)))
                .collect::<Result<_>>()?,
        )),
    }
}

fn json_argument(template: &str) -> Option<&str> {
    let expression = template.strip_prefix("${{")?.strip_suffix("}}")?.trim();
    let open = expression.find('(')?;
    if !expression[..open].trim().eq_ignore_ascii_case("fromJSON") {
        return None;
    }
    let bytes = expression.as_bytes();
    let mut depth = 1;
    let mut quoted = false;
    let mut i = open + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' if quoted && bytes.get(i + 1) == Some(&b'\'') => {
                i += 2;
                continue;
            }
            b'\'' => quoted = !quoted,
            b'(' if !quoted => depth += 1,
            b')' if !quoted => {
                depth -= 1;
                if depth == 0 {
                    return expression[i + 1..]
                        .trim()
                        .is_empty()
                        .then_some(&expression[open + 1..i]);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn json_numbers(value: Yaml) -> Result<Yaml> {
    match value {
        Yaml::Mapping(map) if map.len() == 1 && map.contains_key("$serde_json::private::Number") => {
            let number = map["$serde_json::private::Number"].as_str().context("Invalid JSON number")?.parse::<f64>()?;
            ensure!(number.is_finite(), "Invalid JSON number");
            yaml_value(Value::Number(number))
        }
        Yaml::Mapping(map) => Ok(Yaml::Mapping(map.into_iter().map(|(key, value)| Ok((key, json_numbers(value)?))).collect::<Result<_>>()?)),
        Yaml::Sequence(values) => Ok(Yaml::Sequence(values.into_iter().map(json_numbers).collect::<Result<_>>()?)),
        value => Ok(value),
    }
}

fn resolve(value: &Yaml, contexts: &Contexts, status: Status) -> Result<Yaml> {
    match value {
        Yaml::String(template) => {
            // Matrix display names retain JSON key order, which expression objects cannot store.
            // The JSON parser reads it straight into the ordered YAML value, escapes included.
            if let Some(argument) = json_argument(template) {
                if let Value::String(source) = evaluate(argument, contexts, status)? {
                    return json_numbers(serde_json::from_str::<Yaml>(&source).context("Error parsing fromJSON")?);
                }
            }
            yaml_value(interpolate(template, contexts, status)?)
        }
        Yaml::Sequence(values) => Ok(Yaml::Sequence(
            values
                .iter()
                .map(|v| resolve(v, contexts, status))
                .collect::<Result<_>>()?,
        )),
        Yaml::Mapping(map) => Ok(Yaml::Mapping(
            map.iter()
                .map(|(k, v)| Ok((k.clone(), resolve(v, contexts, status)?)))
                .collect::<Result<_>>()?,
        )),
        _ => Ok(value.clone()),
    }
}

fn resolved(job: &Mapping, key: &str, contexts: &Contexts, status: Status) -> Result<Option<Yaml>> {
    job.get(key)
        .map(|value| resolve(value, contexts, status).with_context(|| key.to_owned()))
        .transpose()
}

fn bool_value(value: Option<&Yaml>, default: bool, path: &str) -> Result<bool> {
    value
        .map(|v| {
            v.as_bool()
                .with_context(|| format!("{path}: Expected a boolean"))
        })
        .unwrap_or(Ok(default))
}

fn number_value(value: Option<&Yaml>, default: f64, path: &str) -> Result<f64> {
    let value = value
        .map(|v| {
            v.as_f64()
                .with_context(|| format!("{path}: Expected a number"))
        })
        .unwrap_or(Ok(default))?;
    ensure!(
        value.is_finite() && value > 0.0,
        "{path}: Expected a positive number"
    );
    Ok(value)
}

// Filters compare every leaf, using expression equality (including case folding and coercion).
fn matches(actual: Option<&Yaml>, expected: &Yaml, work: &mut u32) -> Result<bool> {
    *work += 1;
    ensure!(*work <= MAX_MATRIX_VISITS, "strategy.matrix: Matrix is too large to expand (work budget exceeded)");
    match expected {
        Yaml::Mapping(map) => {
            for (key, expected) in map {
                let child = actual.and_then(Yaml::as_mapping).and_then(|m| {
                    m.iter()
                        .find(|(k, _)| match (k.as_str(), key.as_str()) {
                            (Some(k), Some(key)) => k.eq_ignore_ascii_case(key),
                            _ => *k == key,
                        })
                        .map(|(_, v)| v)
                });
                if !matches(child, expected, work)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Yaml::Sequence(items) => {
            for (i, expected) in items.iter().enumerate() {
                let child = actual.and_then(Yaml::as_sequence).and_then(|v| v.get(i));
                if !matches(child, expected, work)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        _ => {
            let Some(actual) = actual else { return Ok(false); };
            if std::mem::discriminant(actual) != std::mem::discriminant(expected) { return Ok(false); }
            let mut contexts = Contexts::default();
            contexts.values.insert(
                "matrix".into(),
                Value::Object(BTreeMap::from([
                    (
                        "actual".into(),
                        expression_value(actual)?,
                    ),
                    ("expected".into(), expression_value(expected)?),
                ])),
            );
            Ok(evaluate(
                "matrix.actual == matrix.expected",
                &contexts,
                Status::default(),
            )?
            .is_truthy())
        }
    }
}

fn name_segments(value: &Yaml, parts: &mut Vec<String>) -> Result<()> {
    match value {
        Yaml::Mapping(map) => {
            for value in map.values() {
                name_segments(value, parts)?;
            }
        }
        Yaml::Sequence(values) => {
            for value in values {
                name_segments(value, parts)?;
            }
        }
        Yaml::Null => {}
        _ => {
            let s = text(value, "matrix")?;
            if !s.is_empty() {
                parts.push(s);
            }
        }
    }
    Ok(())
}

struct Combination {
    matrix: Value,
    name: Vec<String>,
}

fn combination(matrix: Mapping, name: &Mapping) -> Result<Combination> {
    let mut parts = Vec::new();
    name_segments(&Yaml::Mapping(name.clone()), &mut parts)?;
    Ok(Combination {
        matrix: expression_value(&Yaml::Mapping(matrix))?,
        name: parts,
    })
}

/// Most partial combinations one matrix may visit. Exclusions can reject every leaf, so the job
/// limit alone does not bound the work.
const MAX_MATRIX_VISITS: u32 = 65_536;

fn cross_product(
    axes: &[(&Yaml, &[Yaml])],
    index: usize,
    product: &mut Mapping,
    exclude: &[&Mapping],
    result: &mut Vec<Mapping>,
    visits: &mut u32,
) -> Result<()> {
    *visits += 1;
    ensure!(
        *visits <= MAX_MATRIX_VISITS,
        "strategy.matrix: Matrix is too large to expand"
    );
    for filter in exclude {
        *visits += 1;
        ensure!(*visits <= MAX_MATRIX_VISITS, "strategy.matrix: Matrix is too large to expand (work budget exceeded)");
        if filter.keys().all(|key| product.contains_key(key))
            && matches(
                Some(&Yaml::Mapping(product.clone())),
                &Yaml::Mapping((*filter).clone()),
                visits,
            )?
        {
            return Ok(());
        }
    }
    if index == axes.len() {
        ensure!(
            result.len() < 256,
            "strategy.matrix: Matrix cannot generate more than 256 jobs"
        );
        result.push(product.clone());
        return Ok(());
    }
    let (key, values) = axes[index];
    for value in values {
        product.insert(key.clone(), value.clone());
        cross_product(axes, index + 1, product, exclude, result, visits)?;
    }
    product.remove(key);
    Ok(())
}

fn matrix(value: Option<&Yaml>) -> Result<Vec<Combination>> {
    let Some(value) = value else {
        return Ok(vec![Combination {
            matrix: Value::Null,
            name: Vec::new(),
        }]);
    };
    let matrix = mapping(value, "strategy.matrix")?;
    let mut axes = Vec::new();
    for (key, value) in matrix {
        if matches!(key.as_str(), Some("include" | "exclude")) {
            continue;
        }
        let values = value.as_sequence().with_context(|| {
            format!(
                "strategy.matrix.{}: Expected a sequence",
                text(key, "matrix").unwrap_or_default()
            )
        })?;
        ensure!(
            !values.is_empty(),
            "strategy.matrix.{}: Matrix vector does not contain any values",
            text(key, "matrix")?
        );
        ensure!(axes.len() < 64, "strategy.matrix: Too many matrix axes");
        axes.push((key, values.as_slice()));
    }
    let filters = |key: &str| -> Result<Vec<&Mapping>> {
        let Some(value) = matrix.get(key) else {
            return Ok(Vec::new());
        };
        let sequence = value
            .as_sequence()
            .with_context(|| format!("strategy.matrix.{key}: Expected a sequence"))?;
        sequence
            .iter()
            .map(|v| {
                let map = mapping(v, &format!("strategy.matrix.{key}"))?;
                ensure!(
                    !map.is_empty(),
                    "strategy.matrix.{key}: Matrix filter must not be empty"
                );
                Ok(map)
            })
            .collect()
    };
    let include = filters("include")?;
    let exclude = filters("exclude")?;
    ensure!(
        !axes.is_empty() || !include.is_empty(),
        "strategy.matrix: Matrix must define at least one vector"
    );
    for filter in &exclude {
        for key in filter.keys() {
            ensure!(axes.iter().any(|(axis, _)| *axis == key), "strategy.matrix.exclude: Matrix exclude key '{}' does not match any key within the matrix", text(key, "exclude")?);
        }
    }
    let mut work = 0;
    let mut products = Vec::new();
    if !axes.is_empty() {
        cross_product(
            &axes,
            0,
            &mut Mapping::new(),
            &exclude,
            &mut products,
            &mut work,
        )?;
    }
    let mut result = Vec::new();
    let mut matched = vec![false; include.len()];
    for product in products {
        let original = Yaml::Mapping(product.clone());
        let mut extended = product.clone();
        for (i, filter) in include.iter().enumerate() {
            let constraints: Mapping = filter
                .iter()
                .filter(|(key, _)| axes.iter().any(|(axis, _)| *axis == *key))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if matches(Some(&original), &Yaml::Mapping(constraints), &mut work)? {
                matched[i] = true;
                for (key, value) in *filter {
                    if !product.contains_key(key) {
                        extended.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        result.push(combination(extended, &product)?);
    }
    for (i, filter) in include.iter().enumerate() {
        if !matched[i] {
            ensure!(result.len() < 256, "strategy.matrix: Matrix cannot generate more than 256 jobs");
            // Unmatched includes do not extend one another. Names list vector keys first.
            let mut ordered = Mapping::new();
            for (key, _) in &axes {
                if let Some(v) = filter.get(*key) {
                    ordered.insert((*key).clone(), v.clone());
                }
            }
            for (key, v) in *filter {
                if !ordered.contains_key(key) {
                    ordered.insert(key.clone(), v.clone());
                }
            }
            result.push(combination(ordered.clone(), &ordered)?);
        }
    }
    ensure!(
        result.len() <= 256,
        "strategy.matrix: Matrix cannot generate more than 256 jobs"
    );
    Ok(result)
}

fn defaults(fields: &Mapping) -> Result<(Option<String>, Option<String>)> {
    let Some(value) = fields.get("defaults") else {
        return Ok((None, None));
    };
    let map = mapping(value, "defaults")?;
    let Some(value) = map.get("run") else {
        return Ok((None, None));
    };
    let run = mapping(value, "defaults.run")?;
    Ok((
        run.get("shell")
            .map(|v| text(v, "defaults.run.shell"))
            .transpose()?,
        run.get("working-directory")
            .map(|v| text(v, "defaults.run.working-directory"))
            .transpose()?,
    ))
}

fn container(value: &Yaml) -> Result<Container> {
    if !value.is_mapping() {
        let image = text(value, "container")?;
        return Ok(Container {
            image: image.strip_prefix("docker://").unwrap_or(&image).to_owned(),
            ..Container::default()
        });
    }
    let map = mapping(value, "container")?;
    let get = |key: &str| {
        map.get(key)
            .map(|v| text(v, &format!("container.{key}")))
            .transpose()
    };
    let credentials = map
        .get("credentials")
        .map(|v| -> Result<_> {
            let v = mapping(v, "container.credentials")?;
            Ok((
                v.get("username")
                    .map(|v| text(v, "credentials.username"))
                    .transpose()?
                    .unwrap_or_default(),
                v.get("password")
                    .map(|v| text(v, "credentials.password"))
                    .transpose()?
                    .unwrap_or_default(),
            ))
        })
        .transpose()?;
    let image = get("image")?.unwrap_or_default();
    Ok(Container {
        image: image.strip_prefix("docker://").unwrap_or(&image).to_owned(),
        credentials,
        env: string_map(map.get("env"), "container.env")?,
        ports: map
            .get("ports")
            .map(|v| strings(v, "container.ports"))
            .transpose()?
            .unwrap_or_default(),
        volumes: map
            .get("volumes")
            .map(|v| strings(v, "container.volumes"))
            .transpose()?
            .unwrap_or_default(),
        options: get("options")?,
    })
}

/// An evaluated job `concurrency`: a group name, or a mapping with `group` and
/// `cancel-in-progress`.
fn job_concurrency(value: &Yaml, path: &str) -> Result<Concurrency> {
    let (group, cancel) = match value {
        Yaml::Mapping(map) => (
            map.get("group")
                .with_context(|| format!("{path}: Required property is missing: group"))?,
            map.get("cancel-in-progress"),
        ),
        _ => (value, None),
    };
    let group = text(group, path)?;
    ensure!(
        super::valid_concurrency_group(&group),
        "{path}: Invalid concurrency group"
    );
    Ok(Concurrency {
        group,
        cancel_in_progress: bool_value(cancel, false, &format!("{path}.cancel-in-progress"))?,
    })
}

pub(super) fn expand(
    workflow: &Workflow,
    job_id: &str,
    contexts: &Contexts,
    status: Status,
) -> Result<Option<Vec<PlannedJob>>> {
    let (_, job) = workflow
        .jobs
        .iter()
        .find(|(id, _)| id == job_id)
        .with_context(|| format!("Unknown job '{job_id}'"))?;
    let path = format!("jobs.{job_id}");
    let condition = job
        .get("if")
        .map(|v| text(v, &format!("{path}.if")))
        .transpose()?
        .unwrap_or_else(|| "success()".into());
    if !evaluate_condition(&condition, contexts, status).with_context(|| format!("{path}.if"))? {
        return Ok(None);
    }
    let strategy_value = resolved(job, "strategy", contexts, status)?;
    let strategy = strategy_value
        .as_ref()
        .map(|v| mapping(v, &format!("{path}.strategy")))
        .transpose()?;
    if let Some(strategy) = strategy {
        keys(
            strategy,
            &["matrix", "fail-fast", "max-parallel"],
            &format!("{path}.strategy"),
        )?;
    }
    let fail_fast = bool_value(
        strategy.and_then(|s| s.get("fail-fast")),
        true,
        "strategy.fail-fast",
    )?;
    let max_parallel = strategy
        .and_then(|s| s.get("max-parallel"))
        .map(|v| -> Result<u32> {
            let n = number_value(Some(v), 1.0, "strategy.max-parallel")?;
            ensure!(
                n.fract() == 0.0 && n <= u32::MAX as f64,
                "strategy.max-parallel: Expected a positive integer"
            );
            Ok(n as u32)
        })
        .transpose()?;
    let combinations =
        matrix(strategy.and_then(|s| s.get("matrix"))).with_context(|| path.clone())?;
    let total = combinations.len();
    let steps = if job.contains_key("uses") { Vec::new() } else {
        job.get("steps").and_then(Yaml::as_sequence).context("steps: Expected a sequence")?
            .iter().map(parse_step).collect::<Result<Vec<_>>>()?
    };
    let mut result = Vec::new();
    for (index, combination) in combinations.into_iter().enumerate() {
        let mut contexts = contexts.clone();
        contexts
            .values
            .insert("matrix".into(), combination.matrix.clone());
        contexts.values.insert(
            "strategy".into(),
            Value::Object(BTreeMap::from([
                ("fail-fast".into(), Value::Bool(fail_fast)),
                (
                    "max-parallel".into(),
                    Value::Number(max_parallel.map_or(total as f64, f64::from)),
                ),
                ("job-index".into(), Value::Number(index as f64)),
                ("job-total".into(), Value::Number(total as f64)),
            ])),
        );
        let mut name = resolved(job, "name", &contexts, status)?
            .as_ref()
            .map(|v| text(v, "name"))
            .transpose()?
            .unwrap_or_else(|| job_id.into());
        // A name that uses the matrix already supplies its own display segments.
        let custom_matrix_name = job
            .get("name")
            .and_then(Yaml::as_str)
            .is_some_and(|s| s.contains("${{"));
        if !custom_matrix_name && !combination.name.is_empty() {
            name = format!("{name} ({})", combination.name.join(", "));
        }
        let runs_on = if job.contains_key("uses") {
            Yaml::Sequence(Vec::new())
        } else {
            resolved(job, "runs-on", &contexts, status)?
                .with_context(|| format!("{path}.runs-on: Required property is missing"))?
        };
        let runs_on_group = runs_on
            .as_mapping()
            .and_then(|map| map.get("group"))
            .map(|v| text(v, "runs-on.group"))
            .transpose()?;
        let runs_on = if let Some(map) = runs_on.as_mapping() {
            keys(map, &["group", "labels"], &format!("{path}.runs-on"))?;
            ensure!(
                !map.is_empty(),
                "{path}.runs-on: Expected 'group' or 'labels'"
            );
            map.get("labels")
                .map(|v| strings(v, "runs-on.labels"))
                .transpose()?
                .unwrap_or_default()
        } else {
            strings(&runs_on, "runs-on")?
        };
        let timeout = resolved(job, "timeout-minutes", &contexts, status)?;
        let timeout = number_value(timeout.as_ref(), 360.0, &format!("{path}.timeout-minutes"))?;
        super::validate_timeout_minutes(timeout)?;
        let continue_on_error = resolved(job, "continue-on-error", &contexts, status)?;
        let continue_on_error = bool_value(
            continue_on_error.as_ref(),
            false,
            &format!("{path}.continue-on-error"),
        )?;
        let concurrency = resolved(job, "concurrency", &contexts, status)?
            .map(|value| job_concurrency(&value, &format!("{path}.concurrency")))
            .transpose()?;
        let env = vec![environment_template(workflow.fields.get("env"), "env")?,
            environment_template(job.get("env"), &format!("{path}.env"))?];
        let (shell, directory) = defaults(&workflow.fields)?;
        let (job_shell, job_directory) = defaults(job)?;
        let container = job
            .get("container")
            .map(container)
            .transpose()?
            .filter(|c| !c.image.is_empty());
        let services = job
            .get("services")
            .map(|v| -> Result<BTreeMap<String, Container>> {
                mapping(v, "services")?
                    .iter()
                    .map(|(k, v)| Ok((text(k, "service")?, self::container(v)?)))
                    .collect()
            })
            .transpose()?
            .unwrap_or_default();
        result.push(PlannedJob {
            job_id: job_id.into(),
            name,
            needs: needs(job)?
                .into_iter()
                .map(|need| {
                    workflow
                        .jobs
                        .iter()
                        .find(|(key, _)| key.eq_ignore_ascii_case(&need))
                        .map_or(need.clone(), |(key, _)| key.clone())
                })
                .collect(),
            runs_on,
            runs_on_group,
            matrix: combination.matrix,
            fail_fast,
            max_parallel,
            timeout_minutes: Some(timeout),
            continue_on_error,
            env,
            default_shell: job_shell.or(shell),
            default_working_directory: job_directory.or(directory),
            container,
            services,
            steps: steps.clone(),
            outputs: string_map(job.get("outputs"), "outputs")?,
            call: job.contains_key("uses").then(|| job.clone()),
            concurrency,
        });
    }
    Ok(Some(result))
}
