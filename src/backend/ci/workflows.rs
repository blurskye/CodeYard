use super::config::CiPipelineKind;
use super::dispatch_inputs::{self, DispatchInput, InputKind, MAX_INPUTS};
use crate::backend::repositories::store::{ErrorKind, RepoError, RepoStore, MAX_CI_WORKFLOW_BYTES};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use std::{
    collections::VecDeque,
    sync::{LazyLock, Mutex},
    time::Duration,
};
use tokio::sync::Semaphore;
use tracing::warn;

pub(super) static PARSING_SLOTS: Semaphore = Semaphore::const_new(2);
static DISCOVERY_SLOTS: Semaphore = Semaphore::const_new(2);
static DISCOVERY_PAYLOADS: Semaphore = Semaphore::const_new(4);
// Let the store finish its address and replica fallback before discovery gives up.
const DISCOVERY_READ_TIMEOUT: Duration =
    crate::backend::cluster::rpc::TIMEOUT.saturating_add(Duration::from_secs(5));
const WORKFLOW_CACHE_BYTES: usize = 8 * 1024 * 1024;
const WORKFLOW_CACHE_ENTRIES: usize = 32;

struct CachedWorkflows {
    repository: String,
    sha: String,
    bytes: usize,
    definitions: Vec<WorkflowDefinition>,
}

static WORKFLOW_CACHE: LazyLock<Mutex<VecDeque<CachedWorkflows>>> =
    LazyLock::new(|| Mutex::new(VecDeque::new()));

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkflowEventFilter {
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
    pub workflow_dispatch: bool,
    /// The `inputs` of `on.workflow_dispatch`, in the order the file declares them.
    #[serde(default)]
    pub dispatch_inputs: Vec<DispatchInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowDefinition {
    pub path: String,
    pub name: String,
    pub triggers: WorkflowTriggers,
}

pub const MAX_WORKFLOW_MATCH_WORK: u64 = 32 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
#[error("workflow filters exceed the match-work limit")]
pub struct WorkflowMatchLimit;

/// One deterministic CPU budget shared by every workflow considered for an event.
pub struct WorkflowMatchBudget {
    remaining: u64,
}

impl WorkflowMatchBudget {
    pub fn new() -> Self {
        Self {
            remaining: MAX_WORKFLOW_MATCH_WORK,
        }
    }

    fn charge(
        &mut self,
        pattern_bytes: usize,
        value_bytes: usize,
    ) -> Result<(), WorkflowMatchLimit> {
        let work = (pattern_bytes as u64)
            .checked_add(1)
            .and_then(|pattern| {
                (value_bytes as u64)
                    .checked_add(1)
                    .and_then(|value| pattern.checked_mul(value))
            })
            .ok_or(WorkflowMatchLimit)?;
        if work > self.remaining {
            self.remaining = 0;
            return Err(WorkflowMatchLimit);
        }
        self.remaining -= work;
        Ok(())
    }
}

impl Default for WorkflowMatchBudget {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkflowDefinition {
    pub fn supports_event(&self, event: &str) -> bool {
        match event {
            "push" => self.triggers.push.is_some(),
            "pull_request" => self.triggers.pull_request.is_some(),
            "workflow_dispatch" => self.triggers.workflow_dispatch,
            _ => false,
        }
    }

    pub fn matches_event(&self, event: &str, branch: &str) -> bool {
        // Compatibility matcher used by manual trigger validation. This only checks event and
        // ref filters, without changed-path or skip-message rules.
        self.matches_event_with_budget(event, branch, &mut WorkflowMatchBudget::new())
            .unwrap_or(false)
    }

    pub fn matches_event_with_budget(
        &self,
        event: &str,
        branch: &str,
        budget: &mut WorkflowMatchBudget,
    ) -> std::result::Result<bool, WorkflowMatchLimit> {
        match event {
            "workflow_dispatch" => Ok(self.triggers.workflow_dispatch),
            "push" => match self.triggers.push.as_ref() {
                Some(filter) => ref_matches_filter(filter, branch, budget),
                None => Ok(false),
            },
            "pull_request" => match self.triggers.pull_request.as_ref() {
                Some(filter) => named_ref_matches_patterns(
                    branch,
                    &filter.branches,
                    &filter.branches_ignore,
                    budget,
                ),
                None => Ok(false),
            },
            _ => Ok(false),
        }
    }

    pub fn matches_event_with_context(
        &self,
        event: &str,
        git_ref: &str,
        changed_paths: &[String],
        head_commit_message: Option<&str>,
    ) -> bool {
        self.matches_event_with_context_and_budget(
            event,
            git_ref,
            changed_paths,
            head_commit_message,
            &mut WorkflowMatchBudget::new(),
        )
        .unwrap_or(false)
    }

    pub fn matches_event_with_context_and_budget(
        &self,
        event: &str,
        git_ref: &str,
        changed_paths: &[String],
        head_commit_message: Option<&str>,
        budget: &mut WorkflowMatchBudget,
    ) -> std::result::Result<bool, WorkflowMatchLimit> {
        if matches!(event, "push" | "pull_request")
            && head_commit_message
                .map(should_skip_run_from_message)
                .unwrap_or(false)
        {
            return Ok(false);
        }

        let filter = match event {
            "workflow_dispatch" => return Ok(self.triggers.workflow_dispatch),
            "push" => self.triggers.push.as_ref(),
            "pull_request" => self.triggers.pull_request.as_ref(),
            _ => return Ok(false),
        };
        let Some(filter) = filter else {
            return Ok(false);
        };
        let ref_matches = if event == "pull_request" {
            named_ref_matches_patterns(git_ref, &filter.branches, &filter.branches_ignore, budget)?
        } else {
            ref_matches_filter(filter, git_ref, budget)?
        };
        if !ref_matches {
            return Ok(false);
        }
        paths_match_filter(filter, event, git_ref, changed_paths, budget)
    }
}

pub fn should_skip_run_from_message(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    if [
        "[skip ci]",
        "[ci skip]",
        "[no ci]",
        "[skip actions]",
        "[actions skip]",
    ]
    .iter()
    .any(|token| lowered.contains(token))
    {
        return true;
    }

    message
        .lines()
        .any(|line| line.trim().eq_ignore_ascii_case("skip-checks: true"))
}

pub async fn commit_message_for_ref(store: &RepoStore, commitish: &str) -> Option<String> {
    store
        .commit(commitish)
        .await
        .ok()
        .map(|commit| commit.message)
}

pub fn infer_pipeline_from_workflow_path(path: &str) -> CiPipelineKind {
    let lowered = path.to_ascii_lowercase();
    if lowered.contains("full") {
        CiPipelineKind::Full
    } else if lowered.contains("cd") || lowered.contains("deploy") || lowered.contains("release") {
        CiPipelineKind::Cd
    } else {
        CiPipelineKind::Ci
    }
}

pub fn workflow_name_from_path(path: &str) -> String {
    let file_name = path.rsplit('/').next().unwrap_or(path);
    file_name
        .trim_end_matches(".yml")
        .trim_end_matches(".yaml")
        .to_string()
}

const MAX_WORKFLOW_NAME_BYTES: usize = 1024;
const MAX_WORKFLOW_FILTER_PATTERNS: usize = 256;
const MAX_WORKFLOW_PATTERN_BYTES: usize = 1024;

async fn discovery_slot(
    slots: &Semaphore,
    deadline: tokio::time::Instant,
) -> Result<tokio::sync::SemaphorePermit<'_>> {
    tokio::time::timeout_at(deadline, slots.acquire())
        .await
        .context("workflow discovery busy")?
        .map_err(Into::into)
}

pub async fn list_workflows_for_ref(
    store: &RepoStore,
    git_ref: &str,
) -> Result<Vec<WorkflowDefinition>> {
    let deadline = tokio::time::Instant::now() + DISCOVERY_READ_TIMEOUT;
    // Resolve on every request so a moved branch cannot return stale definitions. Only the
    // immutable result is cached; callers still authorize repository reads before discovery.
    let sha = tokio::time::timeout(DISCOVERY_READ_TIMEOUT, store.commit(git_ref))
        .await
        .context("workflow repository read timed out")??
        .sha;
    let repository = store.location().to_owned();
    {
        let cache = WORKFLOW_CACHE.lock().unwrap();
        if let Some(entry) = cache
            .iter()
            .find(|entry| entry.repository == repository && entry.sha == sha)
        {
            return Ok(entry.definitions.clone());
        }
    }
    // Bound payloads before loading them, separately from YAML parsing capacity.
    // A stalled storage node holds these slots up to the failover deadline, so others do not queue behind it.
    let payload = discovery_slot(
        &DISCOVERY_PAYLOADS,
        deadline.min(tokio::time::Instant::now() + Duration::from_secs(10)),
    )
    .await?;
    let files = match tokio::time::timeout_at(deadline, store.workflow_files(&sha))
        .await
        .context("workflow repository read timed out")?
    {
        Ok(files) => files,
        Err(error) if is_missing_workflow_tree_error(&error) => Vec::new(),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to list workflows at ref {}", git_ref));
        }
    };

    let permit = discovery_slot(&DISCOVERY_SLOTS, deadline).await?;
    let git_ref = git_ref.to_owned();
    tokio::task::spawn_blocking(move || {
        // Cancellation must not release capacity while YAML is still being parsed or dropped.
        let (_permit, _payload) = (permit, payload);
        let mut workflows = Vec::new();
        for file in files {
            match parse_workflow_definition(&file.path, &file.content) {
                Ok(workflow) => workflows.push(workflow),
                Err(error) => {
                    warn!(
                        "failed to parse workflow file {} at {}: {}",
                        file.path, git_ref, error
                    );
                }
            }
        }

        workflows.sort_by(|a, b| a.name.cmp(&b.name).then(a.path.cmp(&b.path)));
        let bytes = serde_json::to_vec(&workflows)
            .expect("workflow definitions serialize")
            .len();
        if bytes <= WORKFLOW_CACHE_BYTES {
            let mut cache = WORKFLOW_CACHE.lock().unwrap();
            cache.retain(|entry| entry.repository != repository || entry.sha != sha);
            while cache.len() >= WORKFLOW_CACHE_ENTRIES
                || cache.iter().map(|entry| entry.bytes).sum::<usize>() + bytes
                    > WORKFLOW_CACHE_BYTES
            {
                cache.pop_front();
            }
            cache.push_back(CachedWorkflows {
                repository,
                sha,
                bytes,
                definitions: workflows.clone(),
            });
        }
        workflows
    })
    .await
    .context("workflow discovery task failed")
}

fn parse_workflow_yaml(content: &str) -> Result<Value> {
    ensure!(
        content.len() <= MAX_CI_WORKFLOW_BYTES,
        "CI workflow exceeds size limit"
    );
    parse_yaml(content.as_bytes())
}

/// Parses workflow YAML. Reading a document copies every alias, so a small file could
/// expand to gigabytes. The expanded size is counted first, without building anything.
pub(crate) fn parse_yaml(content: &[u8]) -> Result<Value> {
    parse_yaml_with_limit(content, 8 * MAX_CI_WORKFLOW_BYTES)
}

pub(crate) fn parse_yaml_with_limit(content: &[u8], max_expanded_bytes: usize) -> Result<Value> {
    use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess};
    use std::cell::Cell;

    #[derive(Clone, Copy)]
    struct Measure<'a>(&'a Cell<usize>, usize);

    impl Measure<'_> {
        fn add<E: de::Error>(self, bytes: usize) -> Result<(), E> {
            let total = self.0.get().saturating_add(bytes + 16);
            self.0.set(total);
            if total > self.1 {
                return Err(E::custom("the workflow is too large once aliases expand"));
            }
            Ok(())
        }
    }

    impl<'de> DeserializeSeed<'de> for Measure<'_> {
        type Value = ();
        fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
            deserializer.deserialize_any(self)
        }
    }

    impl<'de> de::Visitor<'de> for Measure<'_> {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a YAML value")
        }
        fn visit_bool<E: de::Error>(self, _: bool) -> Result<(), E> {
            self.add(1)
        }
        fn visit_i64<E: de::Error>(self, _: i64) -> Result<(), E> {
            self.add(8)
        }
        fn visit_u64<E: de::Error>(self, _: u64) -> Result<(), E> {
            self.add(8)
        }
        fn visit_f64<E: de::Error>(self, _: f64) -> Result<(), E> {
            self.add(8)
        }
        fn visit_str<E: de::Error>(self, text: &str) -> Result<(), E> {
            self.add(text.len())
        }
        fn visit_unit<E: de::Error>(self) -> Result<(), E> {
            self.add(0)
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
            self.add(0)?;
            while seq.next_element_seed(self)?.is_some() {}
            Ok(())
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
            self.add(0)?;
            while map.next_key_seed(self)?.is_some() {
                map.next_value_seed(self)?;
            }
            Ok(())
        }
        fn visit_enum<A: de::EnumAccess<'de>>(self, data: A) -> Result<(), A::Error> {
            use de::VariantAccess;
            // The tag name is copied into the value with every alias, so it counts too.
            let (name, variant): (String, _) = data.variant()?;
            self.add(name.len())?;
            variant.newtype_variant_seed(self)
        }
    }

    let used = Cell::new(0);
    Measure(&used, max_expanded_bytes)
        .deserialize(serde_yaml::Deserializer::from_slice(content))?;
    Ok(serde_yaml::from_slice(content)?)
}

pub(crate) fn parse_workflow_definition(path: &str, content: &str) -> Result<WorkflowDefinition> {
    ensure!(
        super::admission::is_safe_workflow_path(path),
        "invalid CI workflow path"
    );
    let yaml = parse_workflow_yaml(content)
        .with_context(|| format!("workflow {} is not valid YAML", path))?;

    workflow_definition_from_yaml(path, &yaml)
}

pub(super) fn workflow_definition_from_yaml(
    path: &str,
    yaml: &Value,
) -> Result<WorkflowDefinition> {
    let name = workflow_name(yaml, path)?;
    let triggers = parse_triggers(extract_on_section(yaml))?;

    Ok(WorkflowDefinition {
        path: path.to_string(),
        name,
        triggers,
    })
}

fn workflow_name(yaml: &Value, path: &str) -> Result<String> {
    let name = extract_string_field(yaml, "name").unwrap_or_else(|| workflow_name_from_path(path));
    ensure!(
        name.len() <= MAX_WORKFLOW_NAME_BYTES,
        "workflow name too long"
    );
    Ok(name)
}

fn extract_on_section(yaml: &Value) -> Option<&Value> {
    let Value::Mapping(mapping) = yaml else {
        return None;
    };

    for (key, value) in mapping {
        if yaml_key_to_string(key).as_deref() == Some("on") {
            return Some(value);
        }
    }

    None
}

fn extract_string_field(yaml: &Value, key_name: &str) -> Option<String> {
    let Value::Mapping(mapping) = yaml else {
        return None;
    };

    for (key, value) in mapping {
        if yaml_key_to_string(key).as_deref() == Some(key_name) {
            if let Some(text) = value.as_str() {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
    }

    None
}

fn parse_triggers(on_section: Option<&Value>) -> Result<WorkflowTriggers> {
    let mut triggers = WorkflowTriggers::default();
    let Some(on_value) = on_section else {
        return Ok(triggers);
    };

    match on_value {
        Value::String(event) => apply_event_trigger(event, &Value::Null, &mut triggers)?,
        Value::Sequence(events) => {
            for event in events {
                if let Some(event_name) = event.as_str() {
                    apply_event_trigger(event_name, &Value::Null, &mut triggers)?;
                }
            }
        }
        Value::Mapping(events) => {
            for (event_key, config) in events {
                if let Some(event_name) = yaml_key_to_string(event_key) {
                    apply_event_trigger(&event_name, config, &mut triggers)?;
                }
            }
        }
        _ => {}
    }

    Ok(triggers)
}

fn apply_event_trigger(
    event_name: &str,
    config: &Value,
    triggers: &mut WorkflowTriggers,
) -> Result<()> {
    match event_name.to_ascii_lowercase().as_str() {
        "push" => triggers.push = Some(parse_event_filter(config)?),
        "pull_request" => triggers.pull_request = Some(parse_event_filter(config)?),
        "workflow_dispatch" => {
            triggers.workflow_dispatch = true;
            triggers.dispatch_inputs = parse_dispatch_inputs(config)?;
        }
        _ => {}
    }
    Ok(())
}

fn parse_dispatch_inputs(config: &Value) -> Result<Vec<DispatchInput>> {
    let Some(inputs) = field(config, "inputs") else {
        return Ok(Vec::new());
    };
    let Value::Mapping(mapping) = inputs else {
        bail!("workflow inputs must be a mapping");
    };
    ensure!(
        mapping.len() <= MAX_INPUTS,
        "workflow declares too many inputs"
    );
    mapping
        .iter()
        .map(|(key, spec)| -> Result<DispatchInput> {
            let kind = match field(spec, "type")
                .and_then(Value::as_str)
                .unwrap_or("string")
            {
                "string" => InputKind::Text,
                "boolean" => InputKind::Boolean,
                "number" => InputKind::Number,
                "choice" => InputKind::Choice,
                "environment" => InputKind::Environment,
                _ => bail!("unsupported workflow input type"),
            };
            let options = match field(spec, "options") {
                None => Vec::new(),
                Some(Value::Sequence(values)) => {
                    values.iter().map(scalar_text).collect::<Result<_>>()?
                }
                Some(_) => bail!("workflow input options must be a list"),
            };
            let input = DispatchInput {
                name: key
                    .as_str()
                    .context("invalid workflow input name")?
                    .to_owned(),
                description: field(spec, "description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                kind,
                required: field(spec, "required")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                default: field(spec, "default").map(scalar_text).transpose()?,
                options,
            };
            dispatch_inputs::check_declaration(&input).map_err(anyhow::Error::msg)?;
            Ok(input)
        })
        .collect()
}

/// A YAML scalar as the text GitHub shows for it: booleans as `true` or `false`.
fn scalar_text(value: &Value) -> Result<String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Bool(flag) => Ok(flag.to_string()),
        Value::Number(number) => Ok(number.to_string()),
        _ => bail!("workflow input values must be scalars"),
    }
}

fn parse_event_filter(config: &Value) -> Result<WorkflowEventFilter> {
    ensure!(
        config.is_null() || config.is_mapping(),
        "invalid workflow event filter"
    );
    let mut filter = WorkflowEventFilter::default();
    let Value::Mapping(mapping) = config else {
        return Ok(filter);
    };

    for (key, value) in mapping {
        match yaml_key_to_string(key).as_deref() {
            Some("branches") => filter.branches = parse_pattern_list(value)?,
            Some("branches-ignore") => filter.branches_ignore = parse_pattern_list(value)?,
            Some("tags") => filter.tags = parse_pattern_list(value)?,
            Some("tags-ignore") => filter.tags_ignore = parse_pattern_list(value)?,
            Some("paths") => filter.paths = parse_pattern_list(value)?,
            Some("paths-ignore") => filter.paths_ignore = parse_pattern_list(value)?,
            _ => {}
        }
    }

    Ok(filter)
}

fn parse_pattern_list(value: &Value) -> Result<Vec<String>> {
    let values = match value {
        Value::String(value) => vec![value.as_str()],
        Value::Sequence(values) => values
            .iter()
            .map(|value| value.as_str().context("invalid workflow pattern"))
            .collect::<Result<Vec<_>>>()?,
        _ => anyhow::bail!("invalid workflow patterns"),
    };
    ensure!(
        values.len() <= MAX_WORKFLOW_FILTER_PATTERNS
            && values
                .iter()
                .all(|value| value.len() <= MAX_WORKFLOW_PATTERN_BYTES),
        "workflow filter too large"
    );
    Ok(values
        .into_iter()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect())
}

fn yaml_key_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(key) => Some(key.to_ascii_lowercase()),
        Value::Bool(true) => Some("on".to_string()),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefKind {
    Branch,
    Tag,
}

fn parse_git_ref(git_ref: &str) -> (RefKind, &str) {
    if let Some(branch) = git_ref.strip_prefix("refs/heads/") {
        return (RefKind::Branch, branch);
    }

    if let Some(tag) = git_ref.strip_prefix("refs/tags/") {
        return (RefKind::Tag, tag);
    }

    (RefKind::Branch, git_ref)
}

fn ref_matches_filter(
    filter: &WorkflowEventFilter,
    git_ref: &str,
    budget: &mut WorkflowMatchBudget,
) -> std::result::Result<bool, WorkflowMatchLimit> {
    let (ref_kind, ref_name) = parse_git_ref(git_ref);
    let has_branch_filters = !filter.branches.is_empty() || !filter.branches_ignore.is_empty();
    let has_tag_filters = !filter.tags.is_empty() || !filter.tags_ignore.is_empty();

    if ref_kind == RefKind::Branch {
        if has_tag_filters && !has_branch_filters {
            return Ok(false);
        }
        return named_ref_matches_patterns(
            ref_name,
            &filter.branches,
            &filter.branches_ignore,
            budget,
        );
    }

    if has_branch_filters && !has_tag_filters {
        return Ok(false);
    }

    named_ref_matches_patterns(ref_name, &filter.tags, &filter.tags_ignore, budget)
}

fn named_ref_matches_patterns(
    value: &str,
    patterns: &[String],
    ignore_patterns: &[String],
    budget: &mut WorkflowMatchBudget,
) -> std::result::Result<bool, WorkflowMatchLimit> {
    if !patterns.is_empty() && !matches_pattern_sequence(patterns, value, budget)? {
        return Ok(false);
    }

    if !ignore_patterns.is_empty() && matches_any_pattern(ignore_patterns, value, budget)? {
        return Ok(false);
    }

    Ok(true)
}

fn paths_match_filter(
    filter: &WorkflowEventFilter,
    event: &str,
    git_ref: &str,
    changed_paths: &[String],
    budget: &mut WorkflowMatchBudget,
) -> std::result::Result<bool, WorkflowMatchLimit> {
    if filter.paths.is_empty() && filter.paths_ignore.is_empty() {
        return Ok(true);
    }

    let (ref_kind, _) = parse_git_ref(git_ref);
    if event == "push" && ref_kind == RefKind::Tag {
        return Ok(true);
    }

    if changed_paths.is_empty() {
        return Ok(false);
    }

    if !filter.paths.is_empty() {
        let mut included = false;
        for path in changed_paths {
            if matches_pattern_sequence(&filter.paths, path, budget)? {
                included = true;
                break;
            }
        }
        if !included {
            return Ok(false);
        }
    }

    if !filter.paths_ignore.is_empty() {
        let mut included = false;
        for path in changed_paths {
            if !matches_any_pattern(&filter.paths_ignore, path, budget)? {
                included = true;
                break;
            }
        }
        if !included {
            return Ok(false);
        }
    }

    Ok(true)
}

fn matches_pattern_sequence(
    patterns: &[String],
    value: &str,
    budget: &mut WorkflowMatchBudget,
) -> std::result::Result<bool, WorkflowMatchLimit> {
    let has_positive = patterns.iter().any(|pattern| !pattern.starts_with('!'));
    if !has_positive {
        return Ok(false);
    }

    let mut matched = false;
    for pattern in patterns {
        let (is_negative, matcher) = if let Some(negated) = pattern.strip_prefix('!') {
            (true, negated)
        } else {
            (false, pattern.as_str())
        };

        if matcher.is_empty() || !workflow_glob_matches_bounded(matcher, value, budget)? {
            continue;
        }

        matched = !is_negative;
    }

    Ok(matched)
}

fn matches_any_pattern(
    patterns: &[String],
    value: &str,
    budget: &mut WorkflowMatchBudget,
) -> std::result::Result<bool, WorkflowMatchLimit> {
    for pattern in patterns {
        if workflow_glob_matches_bounded(pattern, value, budget)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// GitHub's filter glob: `*` and `?` stop at `/`, `**` crosses it, and `**/` also matches
/// no directory, so `**/README.md` matches a root README.md. Branch protection patterns use
/// this too.
pub(crate) fn github_glob_matches(pattern: &str, value: &str) -> bool {
    glob_matches(pattern, value, false, false)
}

fn glob_matches(pattern: &str, value: &str, escapes: bool, workflow: bool) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let value: Vec<char> = value.chars().collect();
    // Each row records which value prefixes match the pattern consumed so far.
    // O(pattern.len() * value.len()) work avoids exponential wildcard backtracking.
    let mut previous = vec![false; value.len() + 1];
    let mut current = vec![false; value.len() + 1];
    previous[0] = true;
    let mut index = 0;
    let mut next_class_end = pattern.iter().position(|&ch| ch == ']');
    while index < pattern.len() {
        current.fill(false);
        let escaped = escapes && pattern[index] == '\\' && index + 1 < pattern.len();
        if escaped {
            index += 1;
        }
        let ch = pattern[index];
        let double = !escaped && ch == '*' && pattern.get(index + 1) == Some(&'*');
        if double
            && pattern.get(index + 2) == Some(&'/')
            && (index == 0 || pattern[index - 1] == '/')
        {
            // `**/` crosses any directories and then needs a `/`, as in `a/**/b`. It can also
            // match no directory at all, so the rows from before it carry over.
            let mut reached = false;
            for (offset, &letter) in value.iter().enumerate() {
                reached |= previous[offset];
                current[offset + 1] = reached && letter == '/';
            }
            for (offset, &matched) in previous.iter().enumerate() {
                current[offset] |= matched;
            }
            index += 3;
        } else if ch == '*' && !escaped {
            current[0] = previous[0];
            for (offset, &letter) in value.iter().enumerate() {
                current[offset + 1] =
                    previous[offset + 1] || (current[offset] && (double || letter != '/'));
            }
            index += if double { 2 } else { 1 };
        } else {
            let class_end = if ch == '[' && !escaped {
                // Advance the closing-bracket search only forwards, including for malformed
                // classes, so repeated `[` characters cannot make parsing quadratic.
                if next_class_end.is_some_and(|end| end <= index) {
                    next_class_end = pattern[index + 1..]
                        .iter()
                        .position(|&ch| ch == ']')
                        .map(|offset| index + 1 + offset);
                }
                next_class_end.filter(|&end| end > index + 1)
            } else {
                None
            };
            let end = class_end.map_or(index + 1, |end| end + 1);
            let quantifier = workflow.then(|| pattern.get(end)).flatten().copied();
            let repeated = quantifier == Some('+');
            let optional = quantifier == Some('?');
            current[0] = optional && previous[0];
            for (offset, &letter) in value.iter().enumerate() {
                let matches = if let Some(end) = class_end {
                    letter != '/' && glob_class_matches(&pattern[index + 1..end], letter)
                } else if ch == '?' && !escaped {
                    letter != '/'
                } else {
                    ch == letter
                };
                current[offset + 1] = (matches
                    && (previous[offset] || (repeated && current[offset])))
                    || (optional && previous[offset + 1]);
            }
            index = end + usize::from(repeated || optional);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[value.len()]
}

fn glob_class_matches(mut class: &[char], letter: char) -> bool {
    let negated = matches!(class.first(), Some('!' | '^'));
    if negated {
        class = &class[1..];
    }
    let mut matched = false;
    while !class.is_empty() {
        if class.len() >= 3 && class[1] == '-' {
            matched |= class[0] <= letter && letter <= class[2];
            class = &class[3..];
        } else {
            matched |= class[0] == letter;
            class = &class[1..];
        }
    }
    matched != negated
}

/// Charges `budget` for the work before matching, so one budget bounds a caller's total.
pub(crate) fn github_glob_matches_bounded(
    pattern: &str,
    value: &str,
    budget: &mut WorkflowMatchBudget,
) -> std::result::Result<bool, WorkflowMatchLimit> {
    budget.charge(pattern.len(), value.len())?;
    Ok(github_glob_matches(pattern, value))
}

fn workflow_glob_matches_bounded(
    pattern: &str,
    value: &str,
    budget: &mut WorkflowMatchBudget,
) -> std::result::Result<bool, WorkflowMatchLimit> {
    budget.charge(pattern.len(), value.len())?;
    Ok(glob_matches(pattern, value, true, true))
}

/// CODEOWNERS uses backslashes to quote whitespace and wildcard characters.
pub(crate) fn code_owner_glob_matches_bounded(
    pattern: &str,
    value: &str,
    budget: &mut WorkflowMatchBudget,
) -> std::result::Result<bool, WorkflowMatchLimit> {
    budget.charge(pattern.len(), value.len())?;
    Ok(glob_matches(pattern, value, true, false))
}

fn is_missing_workflow_tree_error(error: &RepoError) -> bool {
    // Only an absent workflow directory inside a resolved commit is empty.
    // Missing repositories/refs/objects and unrelated errors remain outages;
    // libgit2's prose is neither stable nor an authorization boundary.
    error.kind == ErrorKind::MissingPath
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_quantifiers_do_not_change_other_glob_dialects() {
        let workflow = parse_workflow_definition(
            ".github/workflows/release.yml",
            "on:\n  push:\n    tags: ['v[0-9]+.[0-9]+.[0-9]+']\n",
        )
        .unwrap();
        for tag in ["v1.2.3", "v12.345.6"] {
            assert!(workflow.matches_event("push", &format!("refs/tags/{tag}")));
            assert!(workflow.matches_event_with_context(
                "push",
                &format!("refs/tags/{tag}"),
                &[],
                None
            ));
        }
        for tag in ["v.2.3", "v1x2.3", "v1.2.3+"] {
            assert!(!workflow.matches_event("push", &format!("refs/tags/{tag}")));
        }
        assert!(
            workflow_glob_matches_bounded("ab?c", "ac", &mut WorkflowMatchBudget::new()).unwrap()
        );
        assert!(
            workflow_glob_matches_bounded("ab?c", "abc", &mut WorkflowMatchBudget::new()).unwrap()
        );
        assert!(
            workflow_glob_matches_bounded(r"a\+b", "a+b", &mut WorkflowMatchBudget::new()).unwrap()
        );
        assert!(github_glob_matches("v[0-9]+", "v1+"));
        assert!(!github_glob_matches("v[0-9]+", "v12"));
        assert!(
            code_owner_glob_matches_bounded("v[0-9]+", "v1+", &mut WorkflowMatchBudget::new())
                .unwrap()
        );
        let mut budget = WorkflowMatchBudget { remaining: 1 };
        assert!(workflow
            .matches_event_with_budget("push", "refs/tags/v1.2.3", &mut budget)
            .is_err());
    }

    #[test]
    fn pull_request_targets_are_short_branch_names_even_with_ref_prefixes() {
        for branch in ["refs/tags/stable", "refs/heads/main"] {
            let workflow = parse_workflow_definition(
                ".github/workflows/ci.yml",
                &format!(
                    "on:\n  pull_request:\n    branches: ['{branch}']\n    paths: ['src/**']\n"
                ),
            )
            .unwrap();
            assert!(workflow.matches_event("pull_request", branch));
            assert!(workflow.matches_event_with_context(
                "pull_request",
                branch,
                &["src/a.rs".into()],
                None
            ));
            assert!(!workflow.matches_event_with_context(
                "pull_request",
                branch,
                &["docs/a.md".into()],
                None
            ));
        }
    }

    #[test]
    fn path_patterns_keep_literal_whitespace_including_after_negation() {
        let workflow = parse_workflow_definition(
            ".github/workflows/ci.yml",
            "on:\n  push:\n    paths: ['docs/guide.md ', ' leading.md', '! leading.md']\n",
        )
        .unwrap();
        assert_eq!(
            workflow.triggers.push.as_ref().unwrap().paths[0],
            "docs/guide.md "
        );
        assert!(workflow.matches_event_with_context(
            "push",
            "main",
            &["docs/guide.md ".into()],
            None
        ));
        assert!(!workflow.matches_event_with_context(
            "push",
            "main",
            &["docs/guide.md".into()],
            None
        ));
        assert!(!workflow.matches_event_with_context(
            "push",
            "main",
            &[" leading.md".into()],
            None
        ));
    }

    #[test]
    fn character_classes_match_release_filters_with_the_shared_budget() {
        let workflow = parse_workflow_definition(
            ".github/workflows/release.yml",
            "on:\n  push:\n    tags: ['v[0-9]*']\n",
        )
        .unwrap();
        assert!(workflow.matches_event("push", "refs/tags/v1.2.3"));
        assert!(!workflow.matches_event("push", "refs/tags/vnext"));
        for (pattern, value, matched) in [
            ("[a-cx-z]", "b", true),
            ("[a-cx-z]", "y", true),
            ("[a-cx-z]", "d", false),
            ("[!0-9]", "a", true),
            ("[!0-9]", "5", false),
            ("[!a]", "/", false),
            ("[-a]", "-", true),
            ("[ab", "[ab", true),
            ("[[[[", "[[[[", true),
            ("]a[0-9]", "]a1", true),
        ] {
            assert_eq!(
                github_glob_matches(pattern, value),
                matched,
                "{pattern} {value}"
            );
        }
        let mut budget = WorkflowMatchBudget { remaining: 1 };
        assert!(workflow
            .matches_event_with_budget("push", "refs/tags/v1", &mut budget)
            .is_err());
    }

    #[test]
    fn discovery_and_admission_reject_the_same_workflow_paths() {
        for path in [
            ".github/workflows/build linux.yml",
            ".github/workflows/雪.yaml",
            &format!(".github/workflows/{}.yml", "x".repeat(238)),
        ] {
            assert!(!super::super::admission::is_safe_workflow_path(path));
            assert!(parse_workflow_definition(path, "on: workflow_dispatch").is_err());
            assert!(parse_push_workflow(path, "on: push").is_err());
        }
        let path = format!(".github/workflows/{}.yml", "x".repeat(233));
        assert_eq!(path.len(), 255);
        assert!(parse_workflow_definition(&path, "on: workflow_dispatch").is_ok());
    }

    #[test]
    fn aliases_may_not_expand_past_the_limit() {
        let ordinary = "base: &steps\n  - run: echo one\njobs:\n  a:\n    steps: *steps\n  b:\n    steps: *steps\n!custom value: 1\n";
        let value = parse_yaml(ordinary.as_bytes()).unwrap();
        assert!(value.get("jobs").is_some());

        // A 64 KiB scalar referenced 200 times is 12 MiB once expanded, from a 1 KiB file.
        let mut bomb = format!("big: &big \"{}\"\nlist:\n", "x".repeat(64 * 1024));
        bomb.push_str(&"  - *big\n".repeat(200));
        let error = parse_yaml(bomb.as_bytes()).unwrap_err();
        assert!(error.to_string().contains("too large"), "{error}");

        // A tag name is copied with every alias as well.
        let mut tagged = format!("big: &big !{} 1\nlist:\n", "t".repeat(64 * 1024));
        tagged.push_str(&"  - *big\n".repeat(200));
        let error = parse_yaml(tagged.as_bytes()).unwrap_err();
        assert!(error.to_string().contains("too large"), "{error}");
    }

    #[test]
    fn only_typed_missing_workflow_tree_is_empty() {
        let absent = git2::Error::new(git2::ErrorCode::NotFound, git2::ErrorClass::Tree, "opaque");
        assert!(is_missing_workflow_tree_error(&RepoError::from(
            anyhow::Error::new(absent).context("caller")
        )));
        for class in [
            git2::ErrorClass::Reference,
            git2::ErrorClass::Repository,
            git2::ErrorClass::Odb,
        ] {
            let error = git2::Error::new(git2::ErrorCode::NotFound, class, "Path not found");
            assert!(!is_missing_workflow_tree_error(&RepoError::from(
                anyhow::Error::from(error)
            )));
        }
        assert!(!is_missing_workflow_tree_error(&RepoError::from(
            anyhow::anyhow!("Path not found")
        )));
    }

    #[test]
    fn workflow_name_defaults_to_file_name() {
        assert_eq!(workflow_name_from_path(".github/workflows/ci.yml"), "ci");
        assert_eq!(
            workflow_name_from_path(".github/workflows/release.yaml"),
            "release"
        );
    }

    #[test]
    fn pipeline_inference_uses_file_name_hints() {
        assert_eq!(
            infer_pipeline_from_workflow_path(".github/workflows/deploy-image.yml"),
            CiPipelineKind::Cd
        );
        assert_eq!(
            infer_pipeline_from_workflow_path(".github/workflows/full-stack.yml"),
            CiPipelineKind::Full
        );
        assert_eq!(
            infer_pipeline_from_workflow_path(".github/workflows/test.yml"),
            CiPipelineKind::Ci
        );
    }

    #[test]
    fn trigger_parser_supports_mapping_and_sequence_forms() {
        let yaml = r#"
name: CI
on:
  push:
    branches: [main, 'release/**']
    branches-ignore: ['release/**-draft']
  workflow_dispatch:
  pull_request:
"#;

        let parsed = parse_workflow_definition(".github/workflows/ci.yml", yaml).unwrap();
        assert!(parsed.supports_event("push"));
        assert!(parsed.supports_event("pull_request"));
        assert!(parsed.supports_event("workflow_dispatch"));
        assert!(parsed.matches_event("push", "main"));
        assert!(parsed.matches_event("push", "release/v1"));
        assert!(!parsed.matches_event("push", "release/v1-draft"));
    }

    #[test]
    fn dispatch_inputs_are_parsed_with_their_types_and_defaults() {
        let yaml = r#"
on:
  workflow_dispatch:
    inputs:
      target:
        description: Where to deploy
        required: true
        type: environment
        options: [staging, production]
      debug:
        type: boolean
        default: true
      count:
        type: number
        default: 3
      note:
        description: Free text
"#;
        let parsed = parse_workflow_definition(".github/workflows/deploy.yml", yaml).unwrap();
        let inputs = &parsed.triggers.dispatch_inputs;
        let names: Vec<_> = inputs.iter().map(|input| input.name.as_str()).collect();
        assert_eq!(names, ["target", "debug", "count", "note"]);
        assert_eq!(inputs[0].kind, InputKind::Environment);
        assert!(inputs[0].required);
        assert_eq!(inputs[0].options, ["staging", "production"]);
        assert_eq!(inputs[1].kind, InputKind::Boolean);
        assert_eq!(inputs[1].default.as_deref(), Some("true"));
        assert_eq!(inputs[2].default.as_deref(), Some("3"));
        assert_eq!(inputs[3].kind, InputKind::Text);
        assert_eq!(inputs[3].default, None);

        for bad in [
            "on:\n  workflow_dispatch:\n    inputs:\n      x:\n        type: date\n",
            "on:\n  workflow_dispatch:\n    inputs:\n      x:\n        type: choice\n",
            "on:\n  workflow_dispatch:\n    inputs:\n      x:\n        type: boolean\n        default: [1]\n",
        ] {
            assert!(parse_workflow_definition(".github/workflows/x.yml", bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn trigger_parser_supports_tag_and_path_filters() {
        let yaml = r#"
on:
  push:
    tags: ['v*']
    paths: ['src/**', '!src/legacy/**']
"#;

        let parsed = parse_workflow_definition(".github/workflows/release.yml", yaml).unwrap();
        assert!(parsed.matches_event_with_context(
            "push",
            "refs/tags/v1.2.3",
            &[],
            Some("release: v1.2.3")
        ));
        assert!(!parsed.matches_event_with_context(
            "push",
            "refs/heads/main",
            &["src/app.rs".to_string()],
            Some("release prep")
        ));
    }

    #[test]
    fn path_filters_observe_negation_order() {
        let yaml = r#"
on:
  pull_request:
    paths: ['docs/**', '!docs/internal/**', 'docs/internal/public/**']
"#;

        let parsed = parse_workflow_definition(".github/workflows/docs.yml", yaml).unwrap();
        assert!(parsed.matches_event_with_context(
            "pull_request",
            "main",
            &["docs/readme.md".to_string()],
            Some("docs")
        ));
        assert!(!parsed.matches_event_with_context(
            "pull_request",
            "main",
            &["docs/internal/secret.md".to_string()],
            Some("docs")
        ));
        assert!(parsed.matches_event_with_context(
            "pull_request",
            "main",
            &["docs/internal/public/guide.md".to_string()],
            Some("docs")
        ));
    }

    #[test]
    fn skip_keywords_block_push_and_pr_runs() {
        let yaml = r#"
on: [push, pull_request]
"#;

        let parsed = parse_workflow_definition(".github/workflows/ci.yml", yaml).unwrap();
        assert!(!parsed.matches_event_with_context(
            "push",
            "main",
            &["src/lib.rs".to_string()],
            Some("chore: cleanup [skip ci]")
        ));
        assert!(!parsed.matches_event_with_context(
            "pull_request",
            "main",
            &["src/lib.rs".to_string()],
            Some("feat: add endpoint\n\nskip-checks: true")
        ));
    }

    #[test]
    fn github_style_globs_match_expected_branches() {
        for (pattern, value, expected) in [
            ("main", "main", true),
            ("main", "mainline", false),
            ("", "", true),
            ("", "main", false),
            ("*", "", true),
            ("**", "", true),
            ("release/*", "release/v1", true),
            ("release/*", "release/v1/hotfix", false),
            ("release/**", "release/v1/hotfix", true),
            ("**/test*", "src/nested/tests", true),
            ("**/test*", "tests", true),
            ("**/README.md", "README.md", true),
            ("**/README.md", "docs/README.md", true),
            ("**/README.md", "xREADME.md", false),
            ("docs/**/README.md", "docs/README.md", true),
            ("docs/**/README.md", "docs/a/b/README.md", true),
            ("docs/**/README.md", "docsREADME.md", false),
            ("*a*b", "aaab", true),
            ("*a*b", "aaa", false),
            ("release/?", "release/β", true),
            ("release/?", "release//", false),
            ("release/*", "release/β", true),
            ("***", "nested/path", true),
        ] {
            assert_eq!(
                github_glob_matches(pattern, value),
                expected,
                "{pattern:?} / {value:?}"
            );
        }
    }

    #[test]
    fn repeated_wildcards_do_not_backtrack_exponentially() {
        let pattern = format!("{}Z", "*a".repeat(64));
        let value = "a".repeat(128);
        assert!(!github_glob_matches(&pattern, &value));
        assert!(github_glob_matches(&pattern, &format!("{value}Z")));
        let pattern = format!("{}Z", "**a".repeat(64));
        assert!(!github_glob_matches(&pattern, &value));
        assert!(github_glob_matches(&pattern, &format!("{value}Z")));
    }

    #[test]
    fn aggregate_glob_work_is_bounded_across_valid_patterns() {
        let hostile = WorkflowDefinition {
            path: ".github/workflows/hostile.yml".into(),
            name: "hostile".into(),
            triggers: WorkflowTriggers {
                push: Some(WorkflowEventFilter {
                    paths: vec!["x".repeat(MAX_WORKFLOW_PATTERN_BYTES); 9],
                    ..Default::default()
                }),
                ..Default::default()
            },
        };
        let changed = vec!["y".repeat(4_096)];
        assert!(hostile
            .matches_event_with_context_and_budget(
                "push",
                "refs/heads/main",
                &changed,
                None,
                &mut WorkflowMatchBudget::new(),
            )
            .is_err());

        let ordinary = parse_push_workflow(
            ".github/workflows/ordinary.yml",
            "on:\n  push:\n    paths: ['src/**']\n",
        )
        .unwrap();
        assert!(ordinary
            .matches_event_with_context_and_budget(
                "push",
                "refs/heads/main",
                &["src/lib.rs".into()],
                None,
                &mut WorkflowMatchBudget::new(),
            )
            .unwrap());
    }

    fn workflow_store(entries: Vec<(String, Vec<u8>)>) -> (tempfile::TempDir, RepoStore) {
        let directory = tempfile::tempdir().unwrap();
        {
            let repository = git2::Repository::init_bare(directory.path()).unwrap();
            let mut workflows = repository.treebuilder(None).unwrap();
            for (name, content) in entries {
                let blob = repository.blob(&content).unwrap();
                workflows.insert(&name, blob, 0o100644).unwrap();
            }
            let workflows = workflows.write().unwrap();
            let mut github = repository.treebuilder(None).unwrap();
            github.insert("workflows", workflows, 0o040000).unwrap();
            let github = github.write().unwrap();
            let mut root = repository.treebuilder(None).unwrap();
            root.insert(".github", github, 0o040000).unwrap();
            let root = repository.find_tree(root.write().unwrap()).unwrap();
            let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
            repository
                .commit(
                    Some("refs/heads/main"),
                    &signature,
                    &signature,
                    "workflows",
                    &root,
                    &[],
                )
                .unwrap();
        }
        let store = RepoStore::from_disk_path(directory.path().to_str().unwrap());
        (directory, store)
    }

    #[tokio::test]
    async fn discovery_slots_bound_payloads_and_waits_separately_from_parsing() {
        let payloads = Semaphore::new(4);
        let held: Vec<_> = (0..4).map(|_| payloads.try_acquire().unwrap()).collect();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(10);
        assert!(discovery_slot(&payloads, deadline).await.is_err());
        drop(held);
        let payload = discovery_slot(
            &payloads,
            tokio::time::Instant::now() + Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(payloads.available_permits(), 3);
        drop(payload);
        assert_eq!(payloads.available_permits(), 4);
    }

    #[tokio::test]
    async fn local_discovery_is_bounded_and_isolates_bad_files() {
        let limit = crate::backend::repositories::store::MAX_CI_WORKFLOW_BYTES;
        let mut exact = b"name: Limit\non: push\n".to_vec();
        exact.resize(limit, b' ');
        let entries = vec![
            ("z.yml".into(), b"name: Alpha\non: push\n".to_vec()),
            ("a.yaml".into(), b"name: Zebra\non: push\n".to_vec()),
            ("limit.yml".into(), exact),
            ("broken.yml".into(), b"on: [push\n".to_vec()),
            (
                "build linux.yml".into(),
                b"on: workflow_dispatch\n".to_vec(),
            ),
            ("雪.yaml".into(), b"on: workflow_dispatch\n".to_vec()),
            (format!("{}.yml", "x".repeat(238)), b"on: push\n".to_vec()),
            ("oversized.yml".into(), vec![b'#'; limit + 1]),
            ("binary.yml".into(), vec![0, 255]),
            (format!("{}.yml", "n".repeat(252)), b"on: push\n".to_vec()),
        ];
        let (_directory, store) = workflow_store(entries);

        let workflows = list_workflows_for_ref(&store, "main").await.unwrap();
        assert_eq!(
            workflows
                .iter()
                .map(|workflow| (workflow.name.as_str(), workflow.path.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("Alpha", ".github/workflows/z.yml"),
                ("Limit", ".github/workflows/limit.yml"),
                ("Zebra", ".github/workflows/a.yaml"),
            ]
        );
        let sha = store.commit("main").await.unwrap().sha;
        assert_eq!(
            store
                .ci_workflow_file(&sha, ".github/workflows/limit.yml")
                .await
                .unwrap()
                .content
                .len(),
            limit
        );
        assert!(store
            .ci_workflow_file(&sha, ".github/workflows/oversized.yml")
            .await
            .unwrap_err()
            .message
            .contains("size limit"));
    }

    #[tokio::test]
    async fn discovery_reuses_immutable_definitions_and_refreshes_moved_refs() {
        let (directory, store) = workflow_store(vec![(
            "cached.yml".into(),
            b"name: Cached\non: push\n".to_vec(),
        )]);
        let first = list_workflows_for_ref(&store, "main").await.unwrap();
        assert_eq!(first[0].name, "Cached");
        let old_sha = store.commit("main").await.unwrap().sha;
        let blob = git2::Oid::hash_object(git2::ObjectType::Blob, b"name: Cached\non: push\n")
            .unwrap()
            .to_string();
        std::fs::remove_file(
            directory
                .path()
                .join("objects")
                .join(&blob[..2])
                .join(&blob[2..]),
        )
        .unwrap();
        assert!(store
            .ci_workflow_file(&old_sha, ".github/workflows/cached.yml")
            .await
            .is_err());
        assert_eq!(
            list_workflows_for_ref(&store, "main").await.unwrap()[0].name,
            "Cached"
        );
        {
            let git = git2::Repository::open_bare(directory.path()).unwrap();
            let parent = git
                .find_commit(git2::Oid::from_str(&old_sha).unwrap())
                .unwrap();
            let tree = git
                .find_tree(git.treebuilder(None).unwrap().write().unwrap())
                .unwrap();
            let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
            git.commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "Remove workflows",
                &tree,
                &[&parent],
            )
            .unwrap();
        }
        assert!(list_workflows_for_ref(&store, "main")
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            list_workflows_for_ref(&store, &old_sha).await.unwrap()[0].name,
            "Cached"
        );
        let cache = WORKFLOW_CACHE.lock().unwrap();
        assert!(cache.len() <= WORKFLOW_CACHE_ENTRIES);
        assert!(cache.iter().map(|entry| entry.bytes).sum::<usize>() <= WORKFLOW_CACHE_BYTES);
    }

    #[tokio::test]
    async fn local_discovery_caps_direct_directory_entries() {
        let entries = (0..=crate::backend::repositories::store::MAX_WORKFLOW_DIRECTORY_ENTRIES)
            .map(|index| (format!("entry-{index}.txt"), Vec::new()))
            .collect();
        let (_directory, store) = workflow_store(entries);
        let error = list_workflows_for_ref(&store, "main").await.unwrap_err();
        assert!(error
            .downcast_ref::<RepoError>()
            .unwrap()
            .message
            .contains("entry limit"));
    }

    #[test]
    fn discovery_and_push_parsers_share_semantic_limits() {
        let parsers: [fn(&str, &str) -> Result<WorkflowDefinition>; 2] =
            [parse_workflow_definition, parse_push_workflow];
        for parse in parsers {
            let name = "n".repeat(MAX_WORKFLOW_NAME_BYTES);
            assert!(parse(
                ".github/workflows/ci.yml",
                &format!("name: '{name}'\non: push")
            )
            .is_ok());
            let name = "n".repeat(MAX_WORKFLOW_NAME_BYTES + 1);
            assert!(parse(
                ".github/workflows/ci.yml",
                &format!("name: '{name}'\non: push")
            )
            .is_err());

            let patterns = std::iter::repeat_n("'src/**'", MAX_WORKFLOW_FILTER_PATTERNS)
                .collect::<Vec<_>>()
                .join(", ");
            assert!(parse(
                ".github/workflows/ci.yml",
                &format!("on:\n  push:\n    paths: [{patterns}]")
            )
            .is_ok());
            let patterns = std::iter::repeat_n("'src/**'", MAX_WORKFLOW_FILTER_PATTERNS + 1)
                .collect::<Vec<_>>()
                .join(", ");
            assert!(parse(
                ".github/workflows/ci.yml",
                &format!("on:\n  push:\n    paths: [{patterns}]")
            )
            .is_err());

            let pattern = "p".repeat(MAX_WORKFLOW_PATTERN_BYTES);
            assert!(parse(
                ".github/workflows/ci.yml",
                &format!("on:\n  push:\n    paths: ['{pattern}']")
            )
            .is_ok());
            let pattern = "p".repeat(MAX_WORKFLOW_PATTERN_BYTES + 1);
            assert!(parse(
                ".github/workflows/ci.yml",
                &format!("on:\n  push:\n    paths: ['{pattern}']")
            )
            .is_err());
        }
    }

    #[tokio::test]
    async fn workflow_followup_tree_header_bound_precedes_traversal() {
        use crate::backend::git::{
            MAX_CI_WORKFLOW_TREE_BYTES, MAX_WORKFLOW_DIRECTORY_ENTRIES,
            MAX_WORKFLOW_FILE_NAME_BYTES,
        };

        let entries = (0..=MAX_WORKFLOW_DIRECTORY_ENTRIES)
            .map(|index| {
                let name = format!(
                    "{index:02}-{}",
                    "n".repeat(MAX_WORKFLOW_FILE_NAME_BYTES - 3)
                );
                assert_eq!(name.len(), MAX_WORKFLOW_FILE_NAME_BYTES);
                (name, Vec::new())
            })
            .collect();
        let (_directory, store) = workflow_store(entries);
        {
            let repository = git2::Repository::open_bare(store.location()).unwrap();
            let commit = repository
                .revparse_single("main")
                .unwrap()
                .peel_to_commit()
                .unwrap();
            let tree = commit.tree().unwrap();
            let entry = tree
                .get_path(std::path::Path::new(".github/workflows"))
                .unwrap();
            let (size, kind) = repository.odb().unwrap().read_header(entry.id()).unwrap();
            assert_eq!(kind, git2::ObjectType::Tree);
            assert!(size > MAX_CI_WORKFLOW_TREE_BYTES);
        }
        let error = store.workflow_files("main").await.unwrap_err();
        assert!(error.message.contains("serialized size limit"), "{error:?}");
    }

    #[test]
    fn workflow_followup_yaml_byte_guard_is_exact() {
        let mut exact = "name: Exact\non: push\n".to_owned();
        exact.extend(std::iter::repeat_n(
            ' ',
            MAX_CI_WORKFLOW_BYTES - exact.len(),
        ));
        assert_eq!(exact.len(), MAX_CI_WORKFLOW_BYTES);
        let oversized = format!("{exact} ");

        let parsers: [fn(&str, &str) -> Result<WorkflowDefinition>; 2] =
            [parse_workflow_definition, parse_push_workflow];
        for parse in parsers {
            assert!(parse(".github/workflows/ci.yml", &exact).is_ok());
            let error = parse(".github/workflows/ci.yml", &oversized).unwrap_err();
            assert!(
                format!("{error:#}").contains("CI workflow exceeds size limit"),
                "{error:#}"
            );
        }
    }

    #[test]
    fn workflow_followup_yaml_keeps_default_recursion_and_alias_limits() {
        let anchored = parse_workflow_definition(
            ".github/workflows/ci.yml",
            "branches: &branches [main, 'release/**']\non:\n  push:\n    branches: *branches\n",
        )
        .unwrap();
        assert_eq!(
            anchored.triggers.push.unwrap().branches,
            ["main", "release/**"]
        );

        let deeply_nested = "[0, ".repeat(1_000) + &"]".repeat(1_000);
        let recursion = parse_workflow_yaml(&deeply_nested).unwrap_err();
        assert!(
            recursion
                .to_string()
                .starts_with("recursion limit exceeded"),
            "{recursion:#}"
        );

        let recursive_alias = parse_workflow_yaml("&a [*a]").unwrap_err();
        assert_eq!(recursive_alias.to_string(), "recursion limit exceeded");

        let repetitive_aliases = concat!(
            "a: &a ~\n",
            "b: &b [*a,*a,*a,*a,*a,*a,*a,*a,*a]\n",
            "c: &c [*b,*b,*b,*b,*b,*b,*b,*b,*b]\n",
            "d: &d [*c,*c,*c,*c,*c,*c,*c,*c,*c]\n",
            "e: &e [*d,*d,*d,*d,*d,*d,*d,*d,*d]\n",
            "f: &f [*e,*e,*e,*e,*e,*e,*e,*e,*e]\n",
            "g: &g [*f,*f,*f,*f,*f,*f,*f,*f,*f]\n",
            "h: &h [*g,*g,*g,*g,*g,*g,*g,*g,*g]\n",
            "i: &i [*h,*h,*h,*h,*h,*h,*h,*h,*h]\n",
        );
        let repetition = parse_workflow_yaml(repetitive_aliases).unwrap_err();
        assert_eq!(repetition.to_string(), "repetition limit exceeded");
    }
}

fn field<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    value.as_mapping()?.iter().find_map(|(key, value)| {
        let matches = key
            .as_str()
            .is_some_and(|key| key.eq_ignore_ascii_case(name))
            || (name == "on" && key == &Value::Bool(true));
        matches.then_some(value)
    })
}

pub(crate) fn parse_push_workflow(path: &str, text: &str) -> Result<WorkflowDefinition> {
    ensure!(
        super::admission::is_safe_workflow_path(path),
        "invalid CI workflow path"
    );
    // `serde_yaml`'s recursion and alias-expansion defaults are intentionally retained.
    let yaml = parse_workflow_yaml(text)?;
    let push = match field(&yaml, "on") {
        Some(Value::String(event)) if event.eq_ignore_ascii_case("push") => {
            Some(WorkflowEventFilter::default())
        }
        Some(Value::Sequence(events))
            if events.iter().any(|event| {
                event
                    .as_str()
                    .is_some_and(|s| s.eq_ignore_ascii_case("push"))
            }) =>
        {
            Some(WorkflowEventFilter::default())
        }
        Some(events @ Value::Mapping(_)) => {
            field(events, "push").map(parse_event_filter).transpose()?
        }
        _ => None,
    };
    let name = workflow_name(&yaml, path)?;
    Ok(WorkflowDefinition {
        path: path.to_owned(),
        name,
        triggers: WorkflowTriggers {
            push,
            ..WorkflowTriggers::default()
        },
    })
}

#[cfg(test)]
mod push_parser_tests {
    use super::*;

    #[test]
    fn push_forms_reuse_the_existing_ref_path_and_skip_message_matcher() {
        for text in [
            "on: push",
            "on: [push, workflow_dispatch]",
            "on:\n  push:\n",
        ] {
            assert!(parse_push_workflow(".github/workflows/ci.yml", text)
                .unwrap()
                .matches_event("push", "main"));
        }
        let workflow = parse_push_workflow(
            ".github/workflows/deploy.yml",
            "on:\n  push:\n    branches: [main]\n    paths: ['src/**']\n",
        )
        .unwrap();
        assert!(workflow.matches_event_with_context(
            "push",
            "refs/heads/main",
            &["src/lib.rs".to_owned()],
            Some("change")
        ));
        assert!(!workflow.matches_event_with_context(
            "push",
            "refs/heads/main",
            &["src/lib.rs".to_owned()],
            Some("[skip ci]")
        ));
        assert!(!workflow.matches_event_with_context(
            "push",
            "refs/heads/topic",
            &["src/lib.rs".to_owned()],
            None
        ));
        assert!(
            parse_push_workflow(".github/workflows/ci.yml", "on:\n  push:\n    paths: 99").is_err()
        );
    }
}
