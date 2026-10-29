//! Authenticated runner progress. The server supplies the private envelope nonce; workflow
//! output is data inside a Log update and cannot become another lifecycle event.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_STATE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_STEP_NUMBER: u32 = 10_000;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub job: String,
    pub variant: u32,
    pub at_ms: u64,
    pub update: Update,
}
impl std::fmt::Debug for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RunnerProgress(..)")
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Update {
    Queued {
        workflow_name: String,
        name: String,
        labels: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        matrix: Option<Matrix>,
    },
    /// The job waits for its concurrency group; the worker answers when it may start.
    Concurrency {
        group: String,
        cancel_in_progress: bool,
    },
    Started,
    StepStarted {
        number: u32,
        name: String,
    },
    StepFinished {
        number: u32,
        conclusion: Conclusion,
        duration_ms: u64,
    },
    Finished {
        conclusion: Conclusion,
        duration_ms: u64,
    },
    Log {
        step: Option<u32>,
        text: String,
    },
    Annotation {
        annotation: Annotation,
    },
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Annotation {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_column: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_column: Option<u32>,
    pub annotation_level: String,
    pub message: String,
    pub title: Option<String>,
}

/// The wrapper distinguishes a known non-matrix (JSON null) from unavailable metadata.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Matrix {
    pub values: serde_json::Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Conclusion {
    Success,
    Failure,
    Skipped,
    Cancelled,
    TimedOut,
}
impl Conclusion {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Skipped => "skipped",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
        }
    }
    pub fn from_result(value: &str) -> Option<Self> {
        match value {
            "success" => Some(Self::Success),
            "failure" => Some(Self::Failure),
            "skipped" => Some(Self::Skipped),
            "cancelled" => Some(Self::Cancelled),
            "timed_out" => Some(Self::TimedOut),
            _ => None,
        }
    }
}

/// Private execution state for later job-scoped replays. This is never an API document.
#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub jobs: BTreeMap<String, JobResult>,
    pub variants: BTreeMap<String, BTreeMap<u32, JobResult>>,
    /// Actual parent aggregation order, not inferred from clocks or variant indices.
    #[serde(default)]
    pub variant_order: BTreeMap<String, Vec<u32>>,
}
impl State {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.has_valid_variant_order(), "invalid observed variant order");
        struct Count(usize);
        impl std::io::Write for Count {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 = self.0.saturating_add(bytes.len());
                if self.0 > MAX_STATE_BYTES { return Err(std::io::ErrorKind::FileTooLarge.into()); }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        serde_json::to_writer(Count(0), self)?;
        Ok(())
    }

    pub fn has_valid_variant_order(&self) -> bool {
        self.variant_order.iter().all(|(job, order)| {
            self.variants.get(job).is_some_and(|variants| {
                order.len() == variants.len()
                    && order
                        .iter()
                        .copied()
                        .collect::<std::collections::BTreeSet<_>>()
                        == variants
                            .keys()
                            .copied()
                            .collect::<std::collections::BTreeSet<_>>()
            })
        })
    }
    pub fn retained_outputs(
        &self,
        job: &str,
        selected: &std::collections::BTreeSet<u32>,
        total: usize,
        called: bool,
    ) -> Result<BTreeMap<String, String>, &'static str> {
        let variants = self
            .variants
            .get(job)
            .ok_or("Source matrix state is unavailable")?;
        if variants.len() != total || (0..total as u32).any(|index| !variants.contains_key(&index))
        {
            return Err("Source matrix shape has changed; re-run the whole workflow");
        }
        let retained: std::collections::BTreeSet<_> = variants
            .keys()
            .filter(|index| !selected.contains(*index))
            .copied()
            .collect();
        let order: Vec<_> = if retained.len() <= 1 {
            retained.iter().copied().collect()
        } else {
            self.variant_order
                .get(job)
                .ok_or("Source matrix completion order is unavailable")?
                .iter()
                .filter(|index| retained.contains(*index))
                .copied()
                .collect()
        };
        if order.len() != retained.len()
            || order
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
                != retained
        {
            return Err("Source matrix completion order is unavailable");
        }
        let mut outputs = BTreeMap::new();
        for index in order {
            let variant = &variants[&index];
            if !called {
                outputs.extend(variant.outputs.clone());
            } else if variant.result == "success" {
                merge_outputs(&mut outputs, variant.outputs.clone());
            }
        }
        Ok(outputs)
    }
}
impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JobReplayState(..)")
    }
}
/// Merges one successful called workflow's outputs into a matrix's outputs. As on GitHub, a
/// later empty value does not replace an earlier nonempty one.
pub fn merge_outputs(outputs: &mut BTreeMap<String, String>, next: BTreeMap<String, String>) {
    for (key, value) in next {
        let keep_earlier =
            value.is_empty() && outputs.get(&key).is_some_and(|earlier| !earlier.is_empty());
        if !keep_earlier {
            outputs.insert(key, value);
        }
    }
}

/// Private source results and the exact jobs/variants to execute. Inherited results supply
/// dependency contexts; they never become fresh progress events or execution records.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Replay {
    pub version: u8,
    pub state: State,
    pub selected: BTreeMap<String, Option<std::collections::BTreeSet<u32>>>,
    /// Actual source matrix values, keyed by the observed variant index.
    #[serde(default)]
    pub matrices: BTreeMap<String, BTreeMap<u32, serde_json::Value>>,
}
impl std::fmt::Debug for Replay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JobReplay(..)")
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobResult {
    pub result: String,
    pub outputs: BTreeMap<String, String>,
}
