//! Dependency-ready logical jobs; leaf permits remain shared across reusable calls.
use super::*;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

pub(super) type SavedJobs = Arc<Mutex<Option<JobState>>>;

impl WorkflowRun {
    pub(super) fn record_state(
        &self,
        saved: &SavedJobs,
        update: impl FnOnce(&mut JobState),
    ) -> Result<()> {
        let Some(path) = &self.options.job_state_file else {
            return Ok(());
        };
        let mut saved = saved.lock().unwrap_or_else(|error| error.into_inner());
        let Some(state) = saved.as_mut() else {
            return Ok(());
        };
        update(state);
        if state.validate().is_err() {
            // Omission stays sticky: never publish a later, incomplete replacement snapshot.
            *saved = None;
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            Log {
                prefix: "Replay".into(),
                masker: self.masker.clone(),
                progress: None,
                output: self.tokens.output.clone(),
            }
            .line("Warning: Private replay state exceeded its budget; replay snapshot omitted");
            return Ok(());
        }
        save_job_state(&self.options, state, &self.masker)
    }

    pub(super) fn execute(
        &self,
        raw: serde_yaml::Value,
        mut contexts: Contexts,
        env: BTreeMap<String, String>,
        name_prefix: String,
        token_prefix: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<JobResult>> + Send + '_>> {
        Box::pin(async move {
            let workflow = Arc::new(workflow::parse(&serde_yaml::to_string(&raw)?)?);
            let order = workflow::job_order(&workflow)?;
            let declared = workflow::job_dependencies(&workflow)?;
            let indices: BTreeMap<_, _> = order
                .iter()
                .enumerate()
                .map(|(index, id)| (id.to_ascii_lowercase(), index))
                .collect();
            let mut dependencies = vec![Vec::new(); order.len()];
            let mut dependents = vec![Vec::new(); order.len()];
            let mut remaining = vec![0usize; order.len()];
            for (index, id) in order.iter().enumerate() {
                for name in &declared[id] {
                    let dependency = *indices
                        .get(&name.to_ascii_lowercase())
                        .context("Unknown job dependency")?;
                    dependencies[index].push((name.clone(), dependency));
                    dependents[dependency].push(index);
                    remaining[index] += 1;
                }
            }
            let mut ready: VecDeque<_> = remaining
                .iter()
                .enumerate()
                .filter_map(|(index, count)| (*count == 0).then_some(index))
                .collect();
            let mut results = vec![None::<JobResult>; order.len()];
            let mut ancestors = vec![Status::default(); order.len()];
            let raw = Arc::new(raw);
            let saved = Arc::new(Mutex::new(Some(JobState::default())));
            let limit = self.options.concurrent_jobs.max(1) as usize;
            let mut tasks = tokio::task::JoinSet::new();
            let mut running = HashMap::new();
            let scheduled = async {
                loop {
                    anyhow::ensure!(
                        !self
                            .options
                            .containment_lost
                            .load(std::sync::atomic::Ordering::Acquire),
                        "Container cleanup failed; refusing further execution"
                    );
                    while tasks.len() < limit && !*self.cancel.borrow() {
                        let Some(index) = ready.pop_front() else {
                            break;
                        };
                        let needs: Vec<_> = dependencies[index]
                            .iter()
                            .map(|(name, dependency)| {
                                (
                                    name.clone(),
                                    results[*dependency]
                                        .as_ref()
                                        .expect("ready dependencies completed")
                                        .clone(),
                                    ancestors[*dependency],
                                )
                            })
                            .collect();
                        let mut status = Status::default();
                        for (_, result, ancestor) in &needs {
                            status.failed |= result.result == "failure" || ancestor.failed;
                            status.cancelled |= result.result == "cancelled" || ancestor.cancelled;
                        }
                        ancestors[index] = status;
                        let execution = self.clone();
                        let raw = raw.clone();
                        let workflow = workflow.clone();
                        let id = order[index].clone();
                        let contexts = contexts.clone();
                        let env = env.clone();
                        let name_prefix = name_prefix.clone();
                        let token_prefix = token_prefix.clone();
                        let saved = saved.clone();
                        let task = tasks.spawn(async move {
                            let result = execution
                                .execute_job(
                                    raw,
                                    workflow,
                                    id,
                                    needs,
                                    saved,
                                    contexts,
                                    env,
                                    name_prefix,
                                    token_prefix,
                                )
                                .await;
                            (index, result)
                        });
                        running.insert(task.id(), index);
                    }
                    let Some(finished) = tasks.join_next_with_id().await else {
                        break;
                    };
                    let (index, result) = match finished {
                        Ok((task, (index, result))) => {
                            running.remove(&task);
                            (index, result)
                        }
                        Err(error) => {
                            let index = running
                                .remove(&error.id())
                                .context("Unknown logical job task")?;
                            self.options
                                .containment_lost
                                .store(true, std::sync::atomic::Ordering::Release);
                            (index, Err(anyhow::anyhow!("Job task failed: {error}")))
                        }
                    };
                    let result = match result {
                        Ok(result) => result,
                        Err(error) => {
                            Log {
                                prefix: format!("{name_prefix}{}", order[index]),
                                masker: self.masker.clone(),
                                progress: None,
                                output: self.tokens.output.clone(),
                            }
                            .line(&format!("Job failed: {error:#}"));
                            let result = JobResult {
                                result: if *self.cancel.borrow() || error.is::<Cancelled>() {
                                    "cancelled"
                                } else {
                                    "failure"
                                }
                                .into(),
                                outputs: BTreeMap::new(),
                            };
                            self.record_state(&saved, |saved| {
                                saved.jobs.insert(order[index].clone(), result.clone());
                            })?;
                            self.tokens
                                .completed(&format!("{token_prefix}{}", order[index]));
                            result
                        }
                    };
                    results[index] = Some(result);
                    for dependent in &dependents[index] {
                        remaining[*dependent] -= 1;
                        if remaining[*dependent] == 0 {
                            ready.push_back(*dependent);
                        }
                    }
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if scheduled.is_err() {
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
            }
            scheduled?;
            let results: BTreeMap<_, _> = order
                .into_iter()
                .zip(results)
                .filter_map(|(id, result)| result.map(|result| (id, result)))
                .collect();
            let result = if *self.cancel.borrow() {
                "cancelled"
            } else if results.values().any(|result| result.result == "failure") {
                "failure"
            } else if results.values().any(|result| result.result == "cancelled") {
                "cancelled"
            } else {
                "success"
            };
            contexts.values.insert("jobs".into(), Value::Object(results.iter().map(|(id, result)| (
                id.clone(), Value::from_json(&serde_json::json!({"result": result.result, "outputs": result.outputs}))
            )).collect()));
            Ok(JobResult {
                result: result.into(),
                outputs: if token_prefix.is_empty() {
                    BTreeMap::new()
                } else {
                    reusable::outputs(&raw, &contexts, &self.masker)?
                },
            })
        })
    }
}
