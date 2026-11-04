use crate::queries::rulesets::Definition;
use anyhow::bail;
use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
#[derive(Clone, Debug)]
pub struct View<T> {
    pub value: T,
    pub can_edit: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Page {
    pub offset: i64,
    pub limit: i64,
}

impl Page {
    pub fn new(offset: i64, limit: i64) -> Result<Self, crate::queries::rulesets::Invalid> {
        if !(0..=1_000_000).contains(&offset) || !(1..=101).contains(&limit) {
            return Err(crate::queries::rulesets::invalid(
                "page",
                "Use bounded pagination",
            ));
        }
        Ok(Self { offset, limit })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    Organization(i64),
    Repository(i64),
}

impl Owner {
    pub fn ids(self) -> (Option<i64>, Option<i64>) {
        match self {
            Self::Organization(id) => (Some(id), None),
            Self::Repository(id) => (None, Some(id)),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ListFilter {
    pub includes_parents: bool,
    pub targets: Option<Vec<crate::queries::rulesets::Target>>,
}

#[derive(Clone, Debug)]
pub struct Ruleset {
    pub id: i64,
    pub owner: Owner,
    pub definition: Definition,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
pub struct StoredRuleset {
    pub id: i64,
    pub owner_org_id: Option<i64>,
    pub owner_repo_id: Option<i64>,
    pub definition: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn definition(value: &str) -> Result<Definition> {
    let value = serde_json::from_str(value).context("Invalid stored ruleset document")?;
    Definition::parse(&value).map_err(|_| anyhow::anyhow!("Invalid stored ruleset policy"))
}

impl StoredRuleset {
    pub fn decode(self) -> Result<Ruleset> {
        let owner = match (self.owner_org_id, self.owner_repo_id) {
            (Some(id), None) if id > 0 => Owner::Organization(id),
            (None, Some(id)) if id > 0 => Owner::Repository(id),
            _ => bail!("Invalid stored ruleset owner"),
        };
        if self.id <= 0 {
            bail!("Invalid stored ruleset identifier");
        }
        let definition = definition(&self.definition)?;
        definition.validate_owner(owner)?;
        Ok(Ruleset {
            id: self.id,
            owner,
            definition,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Version {
    pub id: i64,
    pub ruleset_id: i64,
    pub definition: Definition,
    pub actor_id: Option<i64>,
    pub actor_type: String,
    pub created_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
pub struct StoredVersion {
    pub id: i64,
    pub ruleset_id: i64,
    pub definition: String,
    pub actor_id: Option<i64>,
    pub actor_type: String,
    pub created_at: DateTime<Utc>,
}

impl StoredVersion {
    pub fn decode(self) -> Result<Version> {
        if self.id <= 0
            || self.ruleset_id <= 0
            || self.actor_id.is_some_and(|id| id <= 0)
            || !matches!(
                self.actor_type.as_str(),
                "User" | "Integration" | "DeployKey" | "System"
            )
        {
            bail!("Invalid stored ruleset version metadata");
        }
        Ok(Version {
            id: self.id,
            ruleset_id: self.ruleset_id,
            definition: definition(&self.definition)?,
            actor_id: self.actor_id,
            actor_type: self.actor_type,
            created_at: self.created_at,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuiteResult {
    Pass,
    Fail,
    Bypass,
}

impl SuiteResult {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Bypass => "bypass",
        }
    }
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "pass" => Ok(Self::Pass),
            "fail" => Ok(Self::Fail),
            "bypass" => Ok(Self::Bypass),
            _ => bail!("Invalid stored ruleset evaluation result"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleResult {
    Pass,
    Fail,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceType {
    Organization,
    Repository,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuiteEnforcement {
    Active,
    Evaluate,
    #[serde(rename = "deleted ruleset")]
    DeletedRuleset,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuleSource {
    pub id: Option<i64>,
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub source_type: SourceType,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Evaluation {
    pub rule_source: RuleSource,
    pub enforcement: SuiteEnforcement,
    pub result: RuleResult,
    pub rule_type: String,
    pub details: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Clone, Debug)]
pub struct SuiteFilter {
    pub since: DateTime<Utc>,
    pub reference: Option<String>,
    pub repository_name: Option<String>,
    pub actor_name: Option<String>,
    pub result: Option<SuiteResult>,
    pub enforcement: Option<SuiteEnforcement>,
}

#[derive(Clone, Debug)]
pub struct Suite {
    pub id: i64,
    pub org_id: Option<i64>,
    pub repo_id: i64,
    pub repository_name: String,
    pub actor_id: Option<i64>,
    pub actor_name: Option<String>,
    pub reference: String,
    pub before_sha: String,
    pub after_sha: String,
    pub result: SuiteResult,
    pub evaluation_result: Option<SuiteResult>,
    pub evaluations: Vec<Evaluation>,
    pub created_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
pub struct StoredSuite {
    pub id: i64,
    pub org_id: Option<i64>,
    pub repo_id: i64,
    pub repository_name: String,
    pub actor_id: Option<i64>,
    pub actor_name: Option<String>,
    pub ref_name: String,
    pub before_sha: String,
    pub after_sha: String,
    pub result: String,
    pub evaluation_result: Option<String>,
    pub evaluations: String,
    pub created_at: DateTime<Utc>,
}

impl StoredSuite {
    pub(crate) const SUMMARY_COLUMNS: &'static str = "s.id,s.org_id,s.repo_id,s.repository_name,s.actor_id,s.actor_name,s.ref_name,s.before_sha,s.after_sha,s.result,s.evaluation_result,'[]' AS evaluations,s.created_at";

    pub fn decode(self) -> Result<Suite> {
        let evaluations = serde_json::from_str(&self.evaluations)
            .context("Invalid stored rule suite evaluations")?;
        Ok(Suite {
            id: self.id,
            org_id: self.org_id,
            repo_id: self.repo_id,
            repository_name: self.repository_name,
            actor_id: self.actor_id,
            actor_name: self.actor_name,
            reference: self.ref_name,
            before_sha: self.before_sha,
            after_sha: self.after_sha,
            result: SuiteResult::parse(&self.result)?,
            evaluation_result: self
                .evaluation_result
                .as_deref()
                .map(SuiteResult::parse)
                .transpose()?,
            evaluations,
            created_at: self.created_at,
        })
    }
}
