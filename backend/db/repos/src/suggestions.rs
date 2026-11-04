use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    SetType,
    AddLabel,
    AddField,
    AddAssignee,
    CloseIssue,
}
impl Action {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "set_type" => Self::SetType,
            "add_label" => Self::AddLabel,
            "add_field" => Self::AddField,
            "add_assignee" => Self::AddAssignee,
            "close_issue" => Self::CloseIssue,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::SetType => "set_type",
            Self::AddLabel => "add_label",
            Self::AddField => "add_field",
            Self::AddAssignee => "add_assignee",
            Self::CloseIssue => "close_issue",
        }
    }
    pub fn slot(self, target: Option<i64>) -> String {
        match self {
            Self::SetType | Self::CloseIssue => self.name().into(),
            _ => format!("{}:{}", self.name(), target.unwrap_or(0)),
        }
    }
}
pub struct Proposal {
    pub action: Action,
    pub target_id: Option<i64>,
    pub value: Value,
    pub rationale: Option<String>,
    pub confidence: Option<String>,
}
impl Proposal {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.target_id.is_none_or(|id| id > 0)
                && self
                    .rationale
                    .as_ref()
                    .is_none_or(|s| s.len() <= 16384 && !s.contains('\0'))
                && self
                    .confidence
                    .as_deref()
                    .is_none_or(|s| matches!(s, "LOW" | "MEDIUM" | "HIGH")),
            Refusal::Invalid
        );
        if matches!(
            self.action,
            Action::AddLabel | Action::AddAssignee | Action::AddField
        ) {
            anyhow::ensure!(self.target_id.is_some(), Refusal::Invalid);
        }
        anyhow::ensure!(self.value.to_string().len() <= 65536, Refusal::Invalid);
        Ok(())
    }
}
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Suggestion {
    pub id: i64,
    pub issue_id: i64,
    pub action: String,
    pub slot: String,
    pub state: String,
    pub target_id: Option<i64>,
    pub target_name: String,
    pub value_json: String,
    pub rationale: Option<String>,
    pub confidence: Option<String>,
    pub actor_id: Option<i64>,
    pub issue_event_id: Option<i64>,
    pub resolved_by: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
pub struct ListFilter<'a> {
    pub state: &'a str,
    pub action: Option<&'a str>,
    pub own: bool,
    pub descending: bool,
    pub offset: i64,
    pub limit: i64,
}
impl ListFilter<'_> {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            valid_state(self.state)
                && self.action.is_none_or(|a| Action::parse(a).is_some())
                && self.offset >= 0
                && (1..=101).contains(&self.limit),
            Refusal::Invalid
        );
        Ok(())
    }
}
pub fn valid_state(state: &str) -> bool {
    matches!(
        state,
        "pending" | "applied" | "approved" | "dismissed" | "replaced" | "invalidated" | "all"
    )
}
pub struct Resolution {
    pub issue: codeyard_db_core::models::Issue,
    pub suggestion: Suggestion,
    pub assignment: Option<i64>,
    pub closed_issue: Option<codeyard_db_core::models::Issue>,
    pub invalidated: bool,
}
#[derive(Clone, Copy, Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Issue suggestion operation denied")]
    Denied,
    #[error("Issue or suggestion not found, or suggestions disabled")]
    Missing,
    #[error("Invalid issue suggestion")]
    Invalid,
    #[error("Only pending suggestions can be resolved")]
    NotPending,
    #[error("The proposed change is no longer valid")]
    Invalidated,
    #[error("Too many pending suggestions")]
    Full,
}
/// Classify shared target-store refusals without turning permission errors into invalidations.
pub fn refusal(error: &anyhow::Error) -> Option<Refusal> {
    if let Some(reason) = error.downcast_ref::<Refusal>() {
        return Some(*reason);
    }
    if let Some(reason) = error.downcast_ref::<crate::issue_attributes::Refusal>() {
        return Some(
            if matches!(reason, crate::issue_attributes::Refusal::Denied) {
                Refusal::Denied
            } else {
                Refusal::Invalid
            },
        );
    }
    if let Some(reason) = error.downcast_ref::<crate::labels::LabelWriteRefusal>() {
        return Some(
            if matches!(reason, crate::labels::LabelWriteRefusal::Denied) {
                Refusal::Denied
            } else {
                Refusal::Invalid
            },
        );
    }
    if let Some(reason) = error.downcast_ref::<crate::assignees::AssignmentRefusal>() {
        return Some(
            if matches!(reason, crate::assignees::AssignmentRefusal::Denied) {
                Refusal::Denied
            } else {
                Refusal::Invalid
            },
        );
    }
    if error
        .downcast_ref::<codeyard_db_core::queries::discussions::DiscussionWriteRefusal>()
        .is_some()
    {
        return Some(Refusal::Denied);
    }
    None
}

native_operation!(crate::postgres::suggestions::enabled,crate::sqlite::suggestions::enabled,
    fn enabled(pool:&DbPool,repo:i64)->Result<bool>);
native_operation!(crate::postgres::suggestions::can_triage,crate::sqlite::suggestions::can_triage,
    fn can_triage(pool:&DbPool,repo:i64,actor:i64)->Result<bool>);
native_operation!(crate::postgres::suggestions::set_enabled,crate::sqlite::suggestions::set_enabled,
    fn set_enabled(pool:&DbPool,repo:i64,actor:i64,credential:&codeyard_db_core::queries::users::accounts::AccountCredential<'_>,enabled:bool)->Result<()>);
native_operation!(crate::postgres::suggestions::list,crate::sqlite::suggestions::list,
    fn list(pool:&DbPool,repo:i64,actor:i64,issue:i64,filter:&ListFilter<'_>)->Result<Vec<Suggestion>>);
native_operation!(crate::postgres::suggestions::propose,crate::sqlite::suggestions::propose,
    fn propose(pool:&DbPool,repo:i64,actor:i64,credential:&codeyard_db_core::queries::users::accounts::AccountCredential<'_>,issue:i64,proposal:&Proposal,target_repository:Option<i64>)->Result<Suggestion>);
native_operation!(crate::postgres::suggestions::resolve,crate::sqlite::suggestions::resolve,
    fn resolve(pool:&DbPool,repo:i64,actor:i64,credential:&codeyard_db_core::queries::users::accounts::AccountCredential<'_>,issue:i64,id:i64,approve:bool)->Result<Resolution>);
