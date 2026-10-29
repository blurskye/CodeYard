//! Private dependency contexts come from completed observations, never workflow plans.
use super::{
    action_jobs::Job,
    ci::{CiAdmissionRefusal as Refusal, CiReplaySelection, NewCiAttempt},
    models::CiRun,
};
use anyhow::{ensure, Result};
use codeyard_actions::progress::{Replay, State};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn compose(previous: Option<Replay>, fresh: State) -> State {
    let mut state = if let Some(previous) = previous {
        let mut state = previous.state;
        for (job, variants) in previous.selected {
            state.jobs.remove(&job);
            if let Some(variants) = variants {
                if let Some(saved) = state.variants.get_mut(&job) {
                    for index in &variants {
                        saved.remove(index);
                    }
                }
                if let Some(order) = state.variant_order.get_mut(&job) {
                    order.retain(|index| !variants.contains(index));
                }
            } else {
                state.variants.remove(&job);
                state.variant_order.remove(&job);
            }
        }
        state
    } else {
        State::default()
    };
    state.jobs.extend(fresh.jobs);
    for (job, variants) in fresh.variants {
        state.variants.entry(job).or_default().extend(variants);
    }
    for (job, order) in fresh.variant_order {
        state.variant_order.entry(job).or_default().extend(order);
    }
    state
}

pub(crate) fn build(
    source: &CiRun,
    request: &NewCiAttempt,
    state: State,
    observations: Vec<Job>,
) -> Result<Option<Replay>> {
    if request.selection == CiReplaySelection::All {
        return Ok(None);
    }
    let unavailable = || {
        Refusal::Conflict(
            "Native dependency or matrix metadata is unavailable; re-run the whole workflow",
        )
    };
    let yaml = request.definition.as_deref().ok_or_else(unavailable)?;
    // A plain workflow is stored as its blob ID. One with calls is stored as a snapshot of its
    // rewritten text, which names each called file by repository, commit and path.
    let unchanged = match source.workflow_blobs.get(&source.workflow_path) {
        Some(serde_json::Value::String(blob)) => {
            git2::Oid::hash_object(git2::ObjectType::Blob, yaml.as_bytes())?.to_string() == *blob
        }
        Some(snapshot) => snapshot.get("content").and_then(serde_json::Value::as_str) == Some(yaml),
        None => return Err(unavailable().into()),
    };
    ensure!(
        unchanged,
        Refusal::Conflict("Rerun workflow snapshot changed")
    );
    let workflow = codeyard_actions::workflow::parse(yaml).map_err(|_| unavailable())?;
    let order = codeyard_actions::workflow::job_order(&workflow).map_err(|_| unavailable())?;
    let raw: serde_yaml::Value = serde_yaml::from_str(yaml).map_err(|_| unavailable())?;
    let mut latest = BTreeMap::new();
    for job in observations {
        if state
            .variants
            .get(&job.logical_job)
            .is_some_and(|variants| variants.contains_key(&(job.variant as u32)))
        {
            latest.insert((job.logical_job.clone(), job.variant as u32), job);
        }
    }
    let mut selected = BTreeMap::<String, Option<BTreeSet<u32>>>::new();
    match request.selection {
        CiReplaySelection::All => unreachable!(),
        CiReplaySelection::Failed => {
            for ((job, variant), observed) in &latest {
                if matches!(
                    observed.conclusion.as_deref(),
                    Some("failure" | "timed_out" | "cancelled")
                ) {
                    selected
                        .entry(job.clone())
                        .or_insert_with(|| Some(BTreeSet::new()))
                        .as_mut()
                        .expect("root selections are variants")
                        .insert(*variant);
                }
            }
            ensure!(
                !selected.is_empty(),
                Refusal::Conflict("No failed jobs have replayable native observations")
            );
        }
        CiReplaySelection::Job(id) => {
            let job = latest
                .values()
                .find(|job| job.id == id && job.started_at.is_some())
                .ok_or(Refusal::Conflict(
                    "Job has a newer execution or its replay metadata is unavailable",
                ))?;
            ensure!(
                job.status == "completed",
                Refusal::Conflict("Cannot re-run an active job")
            );
            selected.insert(
                job.logical_job.clone(),
                Some(BTreeSet::from([job.variant as u32])),
            );
        }
    }
    // The checked native topological order makes this the full dependent closure, including
    // jobs not previously executed. They become observations only if the runner executes them.
    for job in &order {
        let needs = &raw["jobs"][job.as_str()]["needs"];
        let names: Vec<&str> = match needs {
            serde_yaml::Value::String(name) => vec![name],
            serde_yaml::Value::Sequence(names) => {
                names.iter().filter_map(serde_yaml::Value::as_str).collect()
            }
            _ => vec![],
        };
        if names
            .iter()
            .any(|name| selected.keys().any(|key| key.eq_ignore_ascii_case(name)))
        {
            selected.insert(job.clone(), None);
        }
    }
    ensure!(
        selected.keys().all(|job| order.contains(job)),
        unavailable()
    );
    ensure!(
        order
            .iter()
            .filter(|job| !selected.contains_key(*job))
            .all(|job| state.jobs.contains_key(job)),
        unavailable()
    );
    for job in order.iter().filter(|job| !selected.contains_key(*job)) {
        let result = state.jobs.get(job).ok_or_else(unavailable)?;
        if result.result != "skipped" {
            if let Some(outputs) = raw["jobs"][job.as_str()]["outputs"].as_mapping() {
                for key in outputs.keys().filter_map(serde_yaml::Value::as_str) {
                    ensure!(result.outputs.contains_key(key),Refusal::Conflict("Dependency outputs were redacted or are unavailable; re-run the whole workflow"));
                }
            }
        }
    }
    let mut matrices = BTreeMap::new();
    for (job, variants) in &selected {
        if let Some(variants) = variants {
            ensure!(state.jobs.contains_key(job), unavailable());
            let source_variants = state.variants.get(job).ok_or_else(unavailable)?;
            state
                .retained_outputs(
                    job,
                    variants,
                    source_variants.len(),
                    raw["jobs"][job.as_str()].get("uses").is_some(),
                )
                .map_err(Refusal::Conflict)?;
            if let Some(outputs) = raw["jobs"][job.as_str()]["outputs"].as_mapping() {
                for (_, result) in source_variants
                    .iter()
                    .filter(|(index, _)| !variants.contains(*index))
                {
                    for key in outputs.keys().filter_map(serde_yaml::Value::as_str) {
                        ensure!(result.outputs.contains_key(key),Refusal::Conflict("Matrix outputs were redacted or are unavailable; re-run the whole workflow"));
                    }
                }
            }
            let mut values = BTreeMap::new();
            for variant in variants {
                let matrix = latest
                    .get(&(job.clone(), *variant))
                    .and_then(|job| job.matrix.as_ref())
                    .ok_or_else(unavailable)?;
                values.insert(*variant, matrix.0.clone());
            }
            matrices.insert(job.clone(), values);
        }
    }
    let replay = Replay {
        version: 1,
        state,
        selected,
        matrices,
    };
    ensure!(
        serde_json::to_vec(&replay)?.len() <= 16 * 1024 * 1024,
        Refusal::Conflict("Native replay state exceeds its limit; re-run the whole workflow")
    );
    Ok(Some(replay))
}
