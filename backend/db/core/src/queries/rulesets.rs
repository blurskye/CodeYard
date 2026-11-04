use crate::queries::rulesets::records::Owner;
use crate::queries::rulesets::records::Ruleset;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;
pub const MAX_DEFINITION_BYTES: usize = 256 * 1024;

pub const MAX_RULESETS: usize = 75;

pub const MAX_RULES: usize = 100;

pub const MAX_PATTERNS: usize = 100;

pub const MAX_BYPASS_ACTORS: usize = 100;

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct Invalid {
    pub field: &'static str,
    pub message: String,
}

pub fn invalid(field: &'static str, message: impl Into<String>) -> Invalid {
    Invalid {
        field,
        message: message.into(),
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    #[default]
    Branch,
    Tag,
    Push,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Enforcement {
    Disabled,
    Active,
    Evaluate,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BypassMode {
    #[default]
    Always,
    PullRequest,
    Exempt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ActorType {
    OrganizationAdmin,
    RepositoryRole,
    Team,
    User,
    DeployKey,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BypassActor {
    pub actor_type: ActorType,
    #[serde(default)]
    pub actor_id: Option<i64>,
    #[serde(default)]
    pub bypass_mode: BypassMode,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Names {
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryNames {
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub protected: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryIds {
    pub repository_ids: Vec<i64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PropertySource {
    #[default]
    Custom,
    System,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Property {
    pub name: String,
    pub property_values: Vec<String>,
    #[serde(default)]
    pub source: PropertySource,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Properties {
    #[serde(default)]
    pub include: Vec<Property>,
    #[serde(default)]
    pub exclude: Vec<Property>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Conditions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_name: Option<Names>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_name: Option<RepositoryNames>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_id: Option<RepositoryIds>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_property: Option<Properties>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operator {
    StartsWith,
    EndsWith,
    Contains,
    Regex,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pattern {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub negate: bool,
    pub operator: Operator,
    pub pattern: String,
    #[serde(skip)]
    compiled: Option<regex::Regex>,
}

impl Pattern {
    fn validate(&mut self) -> Result<(), Invalid> {
        if self.pattern.is_empty()
            || self.pattern.len() > 4096
            || self
                .name
                .as_ref()
                .is_some_and(|s| s.len() > 1024 || s.chars().any(char::is_control))
        {
            return Err(invalid(
                "rules.parameters.pattern",
                "Use a nonempty pattern of at most 4096 bytes and a bounded single-line label",
            ));
        }
        self.compiled = if self.operator == Operator::Regex {
            Some(regex::RegexBuilder::new(&self.pattern).size_limit(1024*1024).dfa_size_limit(1024*1024).nest_limit(64).build().map_err(|_|invalid("rules.parameters.pattern","The regular expression is invalid or exceeds the native matching limits"))?)
        } else {
            None
        };
        Ok(())
    }
    pub fn matches(&self, value: &str) -> Result<bool, Invalid> {
        let matched = match self.operator {
            Operator::StartsWith => value.starts_with(&self.pattern),
            Operator::EndsWith => value.ends_with(&self.pattern),
            Operator::Contains => value.contains(&self.pattern),
            Operator::Regex => self
                .compiled
                .as_ref()
                .ok_or_else(|| {
                    invalid(
                        "rules.parameters.pattern",
                        "The rule has not been validated",
                    )
                })?
                .is_match(value),
        };
        Ok(matched != self.negate)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    #[serde(default)]
    pub update_allows_fetch_and_merge: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestrictedPaths {
    pub restricted_file_paths: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestrictedExtensions {
    pub restricted_file_extensions: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathLength {
    pub max_file_path_length: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSize {
    pub max_file_size: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Rule {
    Creation,
    Update {
        #[serde(default)]
        parameters: Update,
    },
    Deletion,
    RequiredLinearHistory,
    NonFastForward,
    CommitMessagePattern {
        parameters: Pattern,
    },
    CommitAuthorEmailPattern {
        parameters: Pattern,
    },
    CommitterEmailPattern {
        parameters: Pattern,
    },
    BranchNamePattern {
        parameters: Pattern,
    },
    TagNamePattern {
        parameters: Pattern,
    },
    FilePathRestriction {
        parameters: RestrictedPaths,
    },
    FileExtensionRestriction {
        parameters: RestrictedExtensions,
    },
    MaxFilePathLength {
        parameters: PathLength,
    },
    MaxFileSize {
        parameters: FileSize,
    },
}

impl Rule {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Creation => "creation",
            Self::Update { .. } => "update",
            Self::Deletion => "deletion",
            Self::RequiredLinearHistory => "required_linear_history",
            Self::NonFastForward => "non_fast_forward",
            Self::CommitMessagePattern { .. } => "commit_message_pattern",
            Self::CommitAuthorEmailPattern { .. } => "commit_author_email_pattern",
            Self::CommitterEmailPattern { .. } => "committer_email_pattern",
            Self::BranchNamePattern { .. } => "branch_name_pattern",
            Self::TagNamePattern { .. } => "tag_name_pattern",
            Self::FilePathRestriction { .. } => "file_path_restriction",
            Self::FileExtensionRestriction { .. } => "file_extension_restriction",
            Self::MaxFilePathLength { .. } => "max_file_path_length",
            Self::MaxFileSize { .. } => "max_file_size",
        }
    }
    pub(crate) fn supports_evaluation(&self) -> bool {
        matches!(
            self,
            Self::Creation
                | Self::Update { .. }
                | Self::Deletion
                | Self::NonFastForward
                | Self::RequiredLinearHistory
                | Self::BranchNamePattern { .. }
                | Self::TagNamePattern { .. }
        )
    }

    fn validate(&mut self, target: Target) -> Result<(), Invalid> {
        let applicable = match self {
            Self::Creation | Self::Update { .. } | Self::Deletion | Self::NonFastForward => {
                target != Target::Push
            }
            Self::RequiredLinearHistory
            | Self::CommitMessagePattern { .. }
            | Self::CommitAuthorEmailPattern { .. }
            | Self::CommitterEmailPattern { .. }
            | Self::BranchNamePattern { .. } => target == Target::Branch,
            Self::TagNamePattern { .. } => target == Target::Tag,
            Self::FilePathRestriction { .. }
            | Self::FileExtensionRestriction { .. }
            | Self::MaxFilePathLength { .. }
            | Self::MaxFileSize { .. } => target == Target::Push,
        };
        if !applicable {
            return Err(invalid(
                "rules",
                "The rule does not apply to this ruleset target",
            ));
        }
        match self {
            Self::CommitMessagePattern { parameters }
            | Self::CommitAuthorEmailPattern { parameters }
            | Self::CommitterEmailPattern { parameters }
            | Self::BranchNamePattern { parameters }
            | Self::TagNamePattern { parameters } => parameters.validate()?,
            Self::Update { parameters } if parameters.update_allows_fetch_and_merge => {
                return Err(invalid(
                    "rules.parameters.update_allows_fetch_and_merge",
                    "Fork synchronization requires a verified native merge admission",
                ))
            }
            Self::FilePathRestriction { parameters } => {
                validate_patterns(&parameters.restricted_file_paths, false)?
            }
            Self::FileExtensionRestriction { parameters } => {
                validate_patterns(&parameters.restricted_file_extensions, false)?;
                if parameters.restricted_file_extensions.iter().any(|s| {
                    !s.starts_with('.') || s.contains('/') || s.contains(['*', '?', '[', ']'])
                }) {
                    return Err(invalid(
                        "rules.parameters.restricted_file_extensions",
                        "Use literal extensions beginning with a period",
                    ));
                }
            }
            Self::MaxFilePathLength { parameters }
                if !(1..=4096).contains(&parameters.max_file_path_length) =>
            {
                return Err(invalid(
                    "rules.parameters.max_file_path_length",
                    "Use a length between 1 and 4096 bytes",
                ))
            }
            Self::MaxFileSize { parameters }
                if !(1..=102400).contains(&parameters.max_file_size) =>
            {
                return Err(invalid(
                    "rules.parameters.max_file_size",
                    "Use a size between 1 and 102400 MiB",
                ))
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    pub name: String,
    #[serde(default)]
    pub target: Target,
    pub enforcement: Enforcement,
    #[serde(default)]
    pub bypass_actors: Vec<BypassActor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conditions: Option<Conditions>,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

fn validate_patterns(patterns: &[String], sentinels: bool) -> Result<(), Invalid> {
    if patterns.len() > MAX_PATTERNS
        || patterns.iter().any(|s| {
            s.is_empty()
                || s.len() > 1024
                || s.chars().any(char::is_control)
                || (!sentinels && s.starts_with('~'))
                || (s.starts_with('~') && !matches!(s.as_str(), "~ALL" | "~DEFAULT_BRANCH"))
        })
    {
        return Err(invalid(
            "conditions",
            "Use at most 100 nonempty bounded patterns and recognized selectors",
        ));
    }
    Ok(())
}

impl Definition {
    pub fn validate_owner(&self, owner: Owner) -> Result<(), Invalid> {
        if let Some(conditions) = &self.conditions {
            let selected = conditions.repository_name.is_some()
                || conditions.repository_id.is_some()
                || conditions.repository_property.is_some();
            if selected != matches!(owner, Owner::Organization(_)) {
                return Err(invalid(
                    "conditions",
                    "Repository selectors belong only to organization rulesets",
                ));
            }
        }
        Ok(())
    }

    pub fn storage_document(&self, owner: Owner) -> anyhow::Result<String> {
        let validated = Self::parse(&serde_json::to_value(self)?)?;
        validated.validate_owner(owner)?;
        // Pushes record evaluate-mode verdicts as rule suites, but ref admission does not
        // enforce these policies yet; never claim they are active.
        if validated.enforcement == Enforcement::Active
            && (!validated.rules.is_empty()
                || validated
                    .conditions
                    .as_ref()
                    .and_then(|conditions| conditions.repository_name.as_ref())
                    .is_some_and(|names| names.protected))
        {
            return Err(invalid(
                "enforcement",
                "Nonempty rulesets can be disabled or evaluated until native ref admission enforces them",
            )
            .into());
        }
        if validated.enforcement == Enforcement::Evaluate
            && validated
                .rules
                .iter()
                .any(|rule| !rule.supports_evaluation())
        {
            return Err(invalid(
                "rules",
                "Evaluate mode supports only ref creation, update, deletion, history and name rules",
            ).into());
        }
        Ok(serde_json::to_string(&validated)?)
    }

    pub fn parse(value: &Value) -> Result<Self, Invalid> {
        let size = serde_json::to_vec(value)
            .map_err(|_| invalid("ruleset", "The ruleset could not be decoded"))?
            .len();
        if size > MAX_DEFINITION_BYTES {
            return Err(invalid("ruleset", "Use a ruleset of at most 256 KiB"));
        }
        // Serde's internally tagged unit variants otherwise ignore extra fields.
        if let Some(rules) = value.get("rules").and_then(Value::as_array) {
            for rule in rules {
                if let Some(fields) = rule.as_object() {
                    let unit = matches!(
                        rule.get("type").and_then(Value::as_str),
                        Some(
                            "creation"
                                | "deletion"
                                | "required_linear_history"
                                | "non_fast_forward"
                        )
                    );
                    if fields
                        .keys()
                        .any(|key| key != "type" && (unit || key != "parameters"))
                    {
                        return Err(invalid("rules", "Unknown rule field"));
                    }
                }
            }
        }
        let mut result: Self = serde_json::from_value(value.clone())
            .map_err(|error| invalid("ruleset", error.to_string()))?;
        result.validate()?;
        Ok(result)
    }
    /// Merge PATCH-like PUT fields under the caller's native owner transaction, not before it.
    pub fn merge(&self, input: &Value) -> Result<Self, Invalid> {
        let fields = input
            .as_object()
            .ok_or_else(|| invalid("ruleset", "Use a JSON object"))?;
        let mut document = serde_json::to_value(self)
            .map_err(|_| invalid("ruleset", "Ruleset could not be read"))?;
        for (key, value) in fields {
            if !matches!(
                key.as_str(),
                "name" | "target" | "enforcement" | "bypass_actors" | "conditions" | "rules"
            ) {
                return Err(invalid("ruleset", format!("Unknown ruleset field: {key}")));
            }
            document[key] = value.clone();
        }
        Self::parse(&document)
    }
    fn validate(&mut self) -> Result<(), Invalid> {
        if self.name.trim().is_empty()
            || self.name.len() > 1024
            || self.name.chars().any(char::is_control)
        {
            return Err(invalid(
                "name",
                "Use a nonempty single-line name of at most 1024 bytes",
            ));
        }
        if self.rules.len() > MAX_RULES {
            return Err(invalid("rules", "Use at most 100 rules"));
        }
        let mut kinds = HashSet::new();
        for rule in &mut self.rules {
            if !kinds.insert(rule.name()) {
                return Err(invalid("rules", "A rule type may appear only once"));
            }
            rule.validate(self.target)?;
        }
        if self.bypass_actors.len() > MAX_BYPASS_ACTORS {
            return Err(invalid("bypass_actors", "Use at most 100 bypass actors"));
        }
        let mut actors = HashSet::new();
        for actor in &mut self.bypass_actors {
            match actor.actor_type {
                ActorType::OrganizationAdmin => actor.actor_id = None,
                ActorType::DeployKey => {
                    if actor.actor_id.is_some() || actor.bypass_mode == BypassMode::PullRequest {
                        return Err(invalid("bypass_actors","Deploy keys require a null actor ID and cannot use pull-request-only bypass"));
                    }
                }
                _ => {
                    if actor.actor_id.is_none_or(|id| id <= 0) {
                        return Err(invalid("bypass_actors", "A positive actor ID is required"));
                    }
                }
            }
            if actor.actor_type == ActorType::RepositoryRole
                && actor.actor_id.is_none_or(|id| !(1..=5).contains(&id))
            {
                return Err(invalid("bypass_actors", "Unknown native repository role"));
            }
            if actor.bypass_mode == BypassMode::PullRequest && self.target != Target::Branch {
                return Err(invalid(
                    "bypass_actors",
                    "Pull-request-only bypass applies only to branch rulesets",
                ));
            }
            if !actors.insert((actor.actor_type, actor.actor_id)) {
                return Err(invalid(
                    "bypass_actors",
                    "A bypass actor may appear only once",
                ));
            }
        }
        if let Some(conditions) = &self.conditions {
            let selected = usize::from(conditions.repository_name.is_some())
                + usize::from(conditions.repository_id.is_some())
                + usize::from(conditions.repository_property.is_some());
            if selected > 1 {
                return Err(invalid(
                    "conditions",
                    "Choose exactly one repository name, ID, or property selector",
                ));
            }
            if (self.target == Target::Push) == conditions.ref_name.is_some() {
                return Err(invalid(
                    "conditions.ref_name",
                    "Branch and tag rules require a ref selector; push rules do not accept one",
                ));
            }
            if let Some(refs) = &conditions.ref_name {
                validate_patterns(&refs.include, true)?;
                validate_patterns(&refs.exclude, true)?;
                if refs.include.is_empty() {
                    return Err(invalid(
                        "conditions.ref_name.include",
                        "Include at least one ref selector",
                    ));
                }
            }
            if let Some(names) = &conditions.repository_name {
                validate_patterns(&names.include, true)?;
                validate_patterns(&names.exclude, true)?;
                if names.include.is_empty()
                    || names
                        .include
                        .iter()
                        .chain(&names.exclude)
                        .any(|p| p == "~DEFAULT_BRANCH")
                {
                    return Err(invalid(
                        "conditions.repository_name",
                        "Include at least one repository name or ~ALL",
                    ));
                }
            }
            if let Some(ids) = &conditions.repository_id {
                let unique: HashSet<_> = ids.repository_ids.iter().collect();
                if ids.repository_ids.is_empty()
                    || ids.repository_ids.len() > 1000
                    || unique.len() != ids.repository_ids.len()
                    || ids.repository_ids.iter().any(|id| *id <= 0)
                {
                    return Err(invalid(
                        "conditions.repository_id",
                        "Use between 1 and 1000 unique positive repository IDs",
                    ));
                }
            }
            if let Some(properties) = &conditions.repository_property {
                if properties.include.len() + properties.exclude.len() > MAX_PATTERNS {
                    return Err(invalid(
                        "conditions.repository_property",
                        "Use at most 100 property selectors",
                    ));
                }
                for property in properties.include.iter().chain(&properties.exclude) {
                    if property.name.is_empty()
                        || property.name.len() > 75
                        || !property
                            .name
                            .bytes()
                            .all(|ch| ch.is_ascii_alphanumeric() || ch == b'_' || ch == b'-')
                        || property.property_values.is_empty()
                        || property.property_values.len() > 200
                        || property.property_values.iter().any(|s| s.len() > 1024)
                    {
                        return Err(invalid(
                            "conditions.repository_property",
                            "Use bounded property names and 1 to 200 bounded values",
                        ));
                    }
                    if property.source == PropertySource::System
                        && !matches!(
                            property.name.as_str(),
                            "visibility" | "name" | "default_branch" | "archived" | "fork"
                        )
                    {
                        return Err(invalid(
                            "conditions.repository_property",
                            "This system property has no native repository projection",
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

pub mod matching;

pub mod records;
