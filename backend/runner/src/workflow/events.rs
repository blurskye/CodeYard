//! Event declarations shared by discovery, admission and execution.
use super::{
    dispatch_inputs::{self, DispatchInput},
    parser,
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use std::collections::BTreeMap;

pub const MAX_WORKFLOW_FILTER_PATTERNS: usize = 256;
pub const MAX_WORKFLOW_PATTERN_BYTES: usize = 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkflowEventFilter {
    #[serde(default)]
    pub types: Vec<String>,
    #[serde(default)]
    pub workflows: Vec<String>,
    pub branches: Vec<String>,
    pub branches_ignore: Vec<String>,
    pub tags: Vec<String>,
    pub tags_ignore: Vec<String>,
    pub paths: Vec<String>,
    pub paths_ignore: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkflowTriggers {
    pub push: Option<WorkflowEventFilter>,
    pub pull_request: Option<WorkflowEventFilter>,
    #[serde(default)]
    pub pull_request_target: Option<WorkflowEventFilter>,
    pub workflow_dispatch: bool,
    #[serde(default)]
    pub events: BTreeMap<String, WorkflowEventFilter>,
    #[serde(default)]
    pub schedules: Vec<String>,
    #[serde(default)]
    pub dispatch_inputs: Vec<DispatchInput>,
}

pub fn declares_event(value: Option<&Value>, event: &str) -> bool {
    match value {
        Some(Value::String(name)) => name == event,
        Some(Value::Sequence(events)) => events.iter().any(|value| value.as_str() == Some(event)),
        Some(Value::Mapping(events)) => events.contains_key(event),
        _ => false,
    }
}

pub fn parse(value: Option<&Value>) -> Result<WorkflowTriggers> {
    let mut triggers = WorkflowTriggers::default();
    let Some(value) = value else {
        return Ok(triggers);
    };
    parser::events(value)?;
    let events: Vec<_> = match value {
        Value::String(event) => vec![(event.as_str(), &Value::Null)],
        Value::Sequence(events) => events
            .iter()
            .map(|event| Ok((event.as_str().context("Invalid event")?, &Value::Null)))
            .collect::<Result<_>>()?,
        Value::Mapping(events) => events
            .iter()
            .map(|(event, config)| Ok((event.as_str().context("Invalid event")?, config)))
            .collect::<Result<_>>()?,
        _ => unreachable!("validated event declaration"),
    };
    for (event, config) in events {
        match event {
            "push" => triggers.push = Some(filter(config)?),
            "pull_request" => triggers.pull_request = Some(filter(config)?),
            "pull_request_target" => triggers.pull_request_target = Some(filter(config)?),
            "workflow_dispatch" => {
                triggers.workflow_dispatch = true;
                triggers.dispatch_inputs = dispatch_inputs::parse(config)?;
            }
            "schedule" => {
                let schedules = config
                    .as_sequence()
                    .context("Workflow schedules must be a list")?;
                ensure!(schedules.len() <= 32, "Too many workflow schedules");
                for schedule in schedules {
                    triggers.schedules.push(
                        schedule["cron"]
                            .as_str()
                            .context("Workflow schedule requires cron")?
                            .to_owned(),
                    );
                }
            }
            _ => {
                triggers.events.insert(event.to_owned(), filter(config)?);
            }
        }
    }
    Ok(triggers)
}

fn filter(config: &Value) -> Result<WorkflowEventFilter> {
    let mut filter = WorkflowEventFilter::default();
    let Some(mapping) = config.as_mapping() else {
        return Ok(filter);
    };
    for (key, value) in mapping {
        let target = match key.as_str().unwrap_or("") {
            "types" => &mut filter.types,
            "workflows" => &mut filter.workflows,
            "branches" => &mut filter.branches,
            "branches-ignore" => &mut filter.branches_ignore,
            "tags" => &mut filter.tags,
            "tags-ignore" => &mut filter.tags_ignore,
            "paths" => &mut filter.paths,
            "paths-ignore" => &mut filter.paths_ignore,
            _ => continue,
        };
        *target = parser::strings(value, "event filter")?;
        ensure!(
            target.len() <= MAX_WORKFLOW_FILTER_PATTERNS
                && target
                    .iter()
                    .all(|value| value.len() <= MAX_WORKFLOW_PATTERN_BYTES),
            "Workflow filter too large"
        );
    }
    Ok(filter)
}
