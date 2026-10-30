//! Branch protection rules and commit statuses.
use crate::backend::ci::workflows::{
    github_glob_matches_bounded, WorkflowMatchBudget, WorkflowMatchLimit,
};
use crate::backend::db::models::{BranchProtectionRule, CommitStatus};
use crate::backend::db::DbPool;
use anyhow::Result;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProtectionWriteRefusal {
    #[error("Repository protection authority is denied")]
    Denied,
    #[error("Branch protection not found")]
    NotFound,
    #[error("Use a branch name or a pattern with *, ** or ?")]
    InvalidPattern,
    #[error("Commit status metadata exceeds its limit")]
    InvalidStatusMetadata,
    #[error("A commit can have at most 1024 external status contexts")]
    StatusLimit,
    #[error("A commit can have at most 1000 statuses for one context")]
    StatusHistoryLimit,
    #[error("A rule can require at most 100 checks of 512 bytes each")]
    InvalidChecks,
    #[error("Push allowances must name at most 100 users with repository write access")]
    InvalidPushAllowances,
    #[error("Push allowances must name at most 100 teams with repository write access")]
    InvalidPushTeams,
    #[error("A repository can have at most 100 protection rules")]
    RuleLimit,
}

/// Room for the longest server-owned context, `gitserv/workflow/` and a workflow path.
const MAX_CHECK_BYTES: usize = 512;

pub fn validate_required_checks(checks: &[String]) -> Result<(), ProtectionWriteRefusal> {
    if checks.len() > 100 || checks.iter().any(|check| check.len() > MAX_CHECK_BYTES) {
        return Err(ProtectionWriteRefusal::InvalidChecks);
    }
    Ok(())
}

/// One rule as an administrator saves it. `Default` is the strictest rule, which is what
/// an omitted setting means.
pub struct BranchProtectionRuleUpsert<'a> {
    pub actor_id: i64,
    pub repo_id: i64,
    pub pattern: &'a str,
    pub require_pull_request: bool,
    pub required_approving_reviews: i32,
    pub dismiss_stale_reviews: bool,
    pub require_status_checks: bool,
    pub required_status_checks: Vec<String>,
    pub require_up_to_date_branch: bool,
    pub require_linear_history: bool,
    pub block_force_pushes: bool,
    pub block_deletions: bool,
    pub restrict_pushes: bool,
    pub push_allowances: Vec<i64>,
    pub push_team_allowances: Vec<i64>,
    pub force_push_allowances: Vec<i64>,
    pub enforce_admins: bool,
    pub require_conversation_resolution: bool,
    pub require_code_owner_review: bool,
    pub github_settings: Option<crate::backend::db::models::BranchProtectionSettings>,
}

impl Default for BranchProtectionRuleUpsert<'_> {
    fn default() -> Self {
        Self {
            actor_id: 0,
            repo_id: 0,
            pattern: "",
            require_pull_request: true,
            required_approving_reviews: 0,
            dismiss_stale_reviews: true,
            require_status_checks: false,
            required_status_checks: Vec::new(),
            require_up_to_date_branch: false,
            require_linear_history: false,
            block_force_pushes: true,
            block_deletions: true,
            restrict_pushes: false,
            push_allowances: Vec::new(),
            push_team_allowances: Vec::new(),
            force_push_allowances: Vec::new(),
            enforce_admins: true,
            require_conversation_resolution: false,
            require_code_owner_review: false,
            github_settings: None,
        }
    }
}

pub fn valid_commit_status_state(state: &str) -> bool {
    matches!(state, "pending" | "success" | "failure" | "error")
}

pub fn validate_status_metadata(
    input: &CommitStatusUpsert<'_>,
) -> std::result::Result<(), ProtectionWriteRefusal> {
    if !valid_commit_status_state(input.state)
        || input.context.is_empty()
        || input.context.len() > 255
        || input.context.contains('\0')
        || input
            .description
            .is_some_and(|value| value.len() > 1024 || value.contains('\0'))
        || input
            .target_url
            .is_some_and(|value| value.len() > 2048 || value.contains('\0'))
    {
        return Err(ProtectionWriteRefusal::InvalidStatusMetadata);
    }
    Ok(())
}

pub enum ActorListUpdate {
    Add(Vec<i64>),
    Set(Vec<i64>),
    Remove(Vec<i64>),
}

#[derive(Default)]
pub struct ReviewProtectionUpdate {
    pub dismiss_stale_reviews: Option<bool>,
    pub require_code_owner_reviews: Option<bool>,
    pub required_approving_review_count: Option<i32>,
    pub require_last_push_approval: Option<bool>,
    pub dismissal_restrictions: Option<serde_json::Value>,
    pub bypass_pull_request_allowances: Option<serde_json::Value>,
}

pub enum ProtectionUpdate {
    RequiredSignatures(bool),
    Reviews(ReviewProtectionUpdate),
    PushApps(ActorListUpdate),
    EnforceAdmins(bool),
    PushUsers(ActorListUpdate),
    PushTeams(ActorListUpdate),
    RemovePushRestrictions,
    RemovePullRequestReviews,
    StatusChecks {
        strict: Option<bool>,
        contexts: Option<Vec<String>>,
        /// Producer bindings for the named contexts; a binding for a context replaces its old one.
        bindings: Vec<serde_json::Value>,
    },
    /// Adds required contexts; `bindings` pins producers for contexts that have none yet.
    AddContexts {
        contexts: Vec<String>,
        bindings: Vec<serde_json::Value>,
    },
    /// Replaces the required contexts; `bindings` as for `AddContexts`.
    SetContexts {
        contexts: Vec<String>,
        bindings: Vec<serde_json::Value>,
    },
    RemoveContexts(Vec<String>),
    RemoveStatusChecks,
}

impl ProtectionUpdate {
    pub(crate) fn new_push_apps(&self) -> Result<Option<Vec<i64>>> {
        let Self::PushApps(ActorListUpdate::Add(ids) | ActorListUpdate::Set(ids)) = self else {
            return Ok(None);
        };
        anyhow::ensure!(
            ids.len() <= 100,
            ProtectionWriteRefusal::InvalidPushAllowances
        );
        Ok(Some(ids.clone()))
    }

    pub(crate) fn new_push_teams(&self) -> Result<Option<Vec<i64>>> {
        let Self::PushTeams(ActorListUpdate::Add(ids) | ActorListUpdate::Set(ids)) = self else {
            return Ok(None);
        };
        anyhow::ensure!(ids.len() <= 100, ProtectionWriteRefusal::InvalidPushTeams);
        let mut ids = ids.clone();
        ids.sort_unstable();
        ids.dedup();
        Ok(Some(ids))
    }

    /// Removing an obsolete allowance remains possible after its user's write access is revoked.
    pub(crate) fn new_push_users(&self) -> Result<Option<Vec<i64>>> {
        let Self::PushUsers(ActorListUpdate::Add(ids) | ActorListUpdate::Set(ids)) = self else {
            return Ok(None);
        };
        anyhow::ensure!(
            ids.len() <= 100,
            ProtectionWriteRefusal::InvalidPushAllowances
        );
        let mut ids = ids.clone();
        ids.sort_unstable();
        ids.dedup();
        Ok(Some(ids))
    }
}

pub fn has_review_requirements(rule: &BranchProtectionRule) -> bool {
    rule.require_pull_request
        || rule.required_approving_reviews > 0
        || rule.require_code_owner_review
}

pub(crate) fn apply_protection_update(
    rule: &mut BranchProtectionRule,
    update: ProtectionUpdate,
) -> Result<()> {
    match update {
        ProtectionUpdate::RequiredSignatures(enabled) => {
            rule.github_settings.required_signatures = enabled
        }
        ProtectionUpdate::Reviews(input) => {
            anyhow::ensure!(
                has_review_requirements(rule),
                ProtectionWriteRefusal::NotFound
            );
            if let Some(value) = input.dismiss_stale_reviews {
                rule.dismiss_stale_reviews = value;
            }
            if let Some(value) = input.require_code_owner_reviews {
                rule.require_code_owner_review = value;
            }
            if let Some(value) = input.required_approving_review_count {
                anyhow::ensure!(
                    (0..=6).contains(&value),
                    ProtectionWriteRefusal::InvalidChecks
                );
                rule.required_approving_reviews = value;
            }
            if let Some(value) = input.require_last_push_approval {
                rule.github_settings.require_last_push_approval = value;
            }
            if let Some(value) = input.dismissal_restrictions {
                rule.github_settings.dismissal_restrictions = Some(value);
            }
            if let Some(value) = input.bypass_pull_request_allowances {
                rule.github_settings.bypass_pull_request_allowances = Some(value);
            }
        }
        ProtectionUpdate::PushApps(update) => {
            anyhow::ensure!(rule.restrict_pushes, ProtectionWriteRefusal::NotFound);
            match update {
                ActorListUpdate::Add(ids) => rule.github_settings.push_apps.extend(ids),
                ActorListUpdate::Set(ids) => rule.github_settings.push_apps = ids,
                ActorListUpdate::Remove(ids) => rule
                    .github_settings
                    .push_apps
                    .retain(|id| !ids.contains(id)),
            }
            rule.github_settings.push_apps.sort_unstable();
            rule.github_settings.push_apps.dedup();
        }
        ProtectionUpdate::EnforceAdmins(enabled) => rule.enforce_admins = enabled,
        ProtectionUpdate::PushUsers(update) => {
            anyhow::ensure!(rule.restrict_pushes, ProtectionWriteRefusal::NotFound);
            match update {
                ActorListUpdate::Add(users) => rule.push_allowances.extend(users),
                ActorListUpdate::Set(users) => rule.push_allowances = users,
                ActorListUpdate::Remove(users) => {
                    rule.push_allowances.retain(|id| !users.contains(id))
                }
            }
            rule.push_allowances.sort_unstable();
            rule.push_allowances.dedup();
            anyhow::ensure!(
                rule.push_allowances
                    .len()
                    .saturating_add(rule.push_team_allowances.len())
                    <= 100
                    && rule.push_allowances.iter().all(|id| *id > 0),
                ProtectionWriteRefusal::InvalidPushAllowances
            );
        }
        ProtectionUpdate::PushTeams(update) => {
            anyhow::ensure!(rule.restrict_pushes, ProtectionWriteRefusal::NotFound);
            match update {
                ActorListUpdate::Add(ids) => rule.push_team_allowances.extend(ids),
                ActorListUpdate::Set(ids) => rule.push_team_allowances = ids,
                ActorListUpdate::Remove(ids) => {
                    rule.push_team_allowances.retain(|id| !ids.contains(id))
                }
            }
            rule.push_team_allowances.sort_unstable();
            rule.push_team_allowances.dedup();
            anyhow::ensure!(
                rule.push_team_allowances
                    .len()
                    .saturating_add(rule.push_allowances.len())
                    <= 100
                    && rule.push_team_allowances.iter().all(|id| *id > 0),
                ProtectionWriteRefusal::InvalidPushTeams
            );
        }
        ProtectionUpdate::RemovePushRestrictions => {
            anyhow::ensure!(rule.restrict_pushes, ProtectionWriteRefusal::NotFound);
            rule.restrict_pushes = false;
            rule.push_allowances.clear();
            rule.push_team_allowances.clear();
            rule.github_settings.push_apps.clear();
        }
        ProtectionUpdate::RemovePullRequestReviews => {
            anyhow::ensure!(
                has_review_requirements(rule),
                ProtectionWriteRefusal::NotFound
            );
            rule.require_pull_request = false;
            rule.required_approving_reviews = 0;
            rule.dismiss_stale_reviews = false;
            rule.require_code_owner_review = false;
            rule.github_settings.require_last_push_approval = false;
            rule.github_settings.dismissal_restrictions = None;
            rule.github_settings.bypass_pull_request_allowances = None;
        }
        _ if !rule.require_status_checks => return Err(ProtectionWriteRefusal::NotFound.into()),
        ProtectionUpdate::StatusChecks {
            strict,
            contexts,
            bindings,
        } => {
            if let Some(strict) = strict {
                rule.require_up_to_date_branch = strict;
            }
            if let Some(contexts) = contexts {
                rule.required_status_checks = contexts;
            }
            rule.github_settings
                .checks
                .retain(|old| !bindings.iter().any(|new| new["context"] == old["context"]));
            rule.github_settings.checks.extend(bindings);
        }
        ProtectionUpdate::AddContexts { contexts, bindings } => {
            rule.required_status_checks.extend(contexts);
            add_unbound(&mut rule.github_settings.checks, bindings);
        }
        ProtectionUpdate::SetContexts { contexts, bindings } => {
            rule.required_status_checks = contexts;
            add_unbound(&mut rule.github_settings.checks, bindings);
        }
        ProtectionUpdate::RemoveContexts(contexts) => rule
            .required_status_checks
            .retain(|context| !contexts.contains(context)),
        ProtectionUpdate::RemoveStatusChecks => {
            rule.require_status_checks = false;
            rule.required_status_checks.clear();
            rule.require_up_to_date_branch = false;
        }
    }
    anyhow::ensure!(
        rule.push_allowances.len()
            + rule.push_team_allowances.len()
            + rule.github_settings.push_apps.len()
            <= 100,
        ProtectionWriteRefusal::InvalidPushAllowances
    );
    rule.required_status_checks.sort();
    rule.required_status_checks.dedup();
    validate_required_checks(&rule.required_status_checks)?;
    Ok(())
}

pub struct CommitStatusUpsert<'a> {
    pub repo_id: i64,
    pub sha: &'a str,
    pub context: &'a str,
    pub state: &'a str,
    pub description: Option<&'a str>,
    pub target_url: Option<&'a str>,
    pub creator_id: Option<i64>,
}

/// The longest pattern a rule may have. The stored column has the same limit.
const MAX_PATTERN_CHARS: usize = 255;

/// A pattern is a branch name that may hold `*`, `**` and `?`. Git must accept the name
/// once each wildcard is a letter. A leading `!` is reserved for negated patterns.
/// Adds the producer bindings for contexts that have none; an existing binding stays.
fn add_unbound(checks: &mut Vec<serde_json::Value>, bindings: Vec<serde_json::Value>) {
    for binding in bindings {
        if !checks.iter().any(|old| old["context"] == binding["context"]) {
            checks.push(binding);
        }
    }
}

pub fn validate_branch_pattern(pattern: &str) -> Result<(), ProtectionWriteRefusal> {
    let candidate = pattern.replace(['*', '?'], "x");
    if pattern.is_empty()
        || pattern.chars().count() > MAX_PATTERN_CHARS
        || pattern.starts_with('!')
        || !git2::Reference::is_valid_name(&format!("refs/heads/{candidate}"))
    {
        return Err(ProtectionWriteRefusal::InvalidPattern);
    }
    Ok(())
}

/// Whether `pattern` names `branch`. A pattern without `*`, `?` or `[` is a plain name and is only
/// compared (Git ref names cannot contain those characters). A wildcard pattern is matched within `budget`, and running out of it is an error,
/// never a non-match.
pub(crate) fn branch_pattern_matches(
    pattern: &str,
    branch: &str,
    budget: &mut WorkflowMatchBudget,
) -> std::result::Result<bool, WorkflowMatchLimit> {
    if !pattern.contains(['*', '?', '[']) {
        return Ok(pattern == branch);
    }
    github_glob_matches_bounded(pattern, branch, budget)
}

/// Contexts compare without case, so the reserved prefix does too.
pub fn server_owned_status_context(context: &str) -> bool {
    context
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("gitserv/"))
}

native_operation!(crate::backend::db::postgres::repositories::protection::list_branch_protection_rules,
    crate::backend::db::sqlite::repositories::protection::list_branch_protection_rules,
    fn list_branch_protection_rules(
    pool: &DbPool,
    repo_id: i64,
) -> Result<Vec<BranchProtectionRule>>);

native_operation!(crate::backend::db::postgres::repositories::protection::upsert_branch_protection_rule,
    crate::backend::db::sqlite::repositories::protection::upsert_branch_protection_rule,
    fn upsert_branch_protection_rule(
    pool: &DbPool,
    input: BranchProtectionRuleUpsert<'_>,
) -> Result<BranchProtectionRule>);

native_operation!(crate::backend::db::postgres::repositories::protection::delete_branch_protection_rule,
    crate::backend::db::sqlite::repositories::protection::delete_branch_protection_rule,
    fn delete_branch_protection_rule(
    pool: &DbPool,
    actor_id: i64,
    repo_id: i64,
    pattern: &str,
) -> Result<bool>);

native_operation!(crate::backend::db::postgres::repositories::protection::update_branch_protection_rule,
    crate::backend::db::sqlite::repositories::protection::update_branch_protection_rule,
    fn update_branch_protection_rule(pool: &DbPool, actor_id: i64, repo_id: i64, pattern: &str, update: ProtectionUpdate) -> Result<BranchProtectionRule>);

native_operation!(crate::backend::db::postgres::repositories::protection::branch_redirect,
    crate::backend::db::sqlite::repositories::protection::branch_redirect,
    fn branch_redirect(pool: &DbPool, repo: i64, old: &str) -> Result<Option<String>>);

native_operation!(crate::backend::db::postgres::repositories::protection::rename_branch_settings,
    crate::backend::db::sqlite::repositories::protection::rename_branch_settings,
    fn rename_branch_settings(pool: &DbPool, actor: i64, repo: i64, old: &str, new: &str) -> Result<()>);

native_operation!(crate::backend::db::postgres::repositories::protection::upsert_commit_status,
    crate::backend::db::sqlite::repositories::protection::upsert_commit_status,
    fn upsert_commit_status(
    pool: &DbPool,
    input: CommitStatusUpsert<'_>,
) -> Result<CommitStatus>);

native_operation!(crate::backend::db::postgres::repositories::protection::list_commit_statuses,
    crate::backend::db::sqlite::repositories::protection::list_commit_statuses,
    fn list_commit_statuses(
    pool: &DbPool,
    repo_id: i64,
    sha: &str,
) -> Result<Vec<CommitStatus>>);

native_operation!(crate::backend::db::postgres::repositories::protection::list_commit_statuses_page,
    crate::backend::db::sqlite::repositories::protection::list_commit_statuses_page,
    fn list_commit_statuses_page(pool: &DbPool, repo_id: i64, sha: &str, offset: i64, limit: i64, newest_first: bool) -> Result<Vec<CommitStatus>>);

native_operation!(crate::backend::db::postgres::repositories::protection::commit_status_counts,
    crate::backend::db::sqlite::repositories::protection::commit_status_counts,
    fn commit_status_counts(pool: &DbPool, repo_id: i64, sha: &str) -> Result<Vec<(String, i64)>>);

native_operation!(crate::backend::db::postgres::repositories::protection::list_commit_status_history_page,
    crate::backend::db::sqlite::repositories::protection::list_commit_status_history_page,
    fn list_commit_status_history_page(pool: &DbPool, repo_id: i64, sha: &str, offset: i64, limit: i64) -> Result<Vec<CommitStatus>>);

native_operation!(crate::backend::db::postgres::repositories::protection::commit_status_history_count,
    crate::backend::db::sqlite::repositories::protection::commit_status_history_count,
    fn commit_status_history_count(pool: &DbPool, repo_id: i64, sha: &str) -> Result<i64>);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_allowance_validation_deduplicates_new_users_but_does_not_revalidate_removals() {
        assert_eq!(
            ProtectionUpdate::PushUsers(ActorListUpdate::Add(vec![2, 1, 2]))
                .new_push_users()
                .unwrap(),
            Some(vec![1, 2])
        );
        assert_eq!(
            ProtectionUpdate::PushUsers(ActorListUpdate::Remove(vec![2]))
                .new_push_users()
                .unwrap(),
            None
        );
        assert_eq!(
            ProtectionUpdate::EnforceAdmins(true)
                .new_push_users()
                .unwrap(),
            None
        );
        let error = ProtectionUpdate::PushUsers(ActorListUpdate::Set(vec![1; 101]))
            .new_push_users()
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<ProtectionWriteRefusal>(),
            Some(&ProtectionWriteRefusal::InvalidPushAllowances)
        );
    }

    fn pattern_matches(pattern: &str, branch: &str) -> bool {
        branch_pattern_matches(pattern, branch, &mut WorkflowMatchBudget::new()).unwrap()
    }

    #[test]
    fn protection_patterns_accept_names_with_wildcards() {
        for pattern in [
            "main",
            "release/*",
            "release*",
            "*",
            "**",
            "release/**",
            "*release",
            "release/*/stable",
            "release/?",
            "feature/雪",
        ] {
            assert_eq!(validate_branch_pattern(pattern), Ok(()), "{pattern}");
        }
        for pattern in [
            "",
            "!main",
            "release/[ab]",
            "release\\*",
            "bad..name",
            " main ",
            "refs//main",
            "release/*.lock",
        ] {
            assert_eq!(
                validate_branch_pattern(pattern),
                Err(ProtectionWriteRefusal::InvalidPattern),
                "{pattern}"
            );
        }
    }

    #[test]
    fn single_star_stops_at_a_slash_and_double_star_does_not() {
        assert!(pattern_matches("main", "main"));
        assert!(!pattern_matches("main", "main-two"));
        assert!(pattern_matches("release/*", "release/1.0"));
        assert!(!pattern_matches("release/*", "release/1.0/rc"));
        assert!(pattern_matches("release/**", "release/1.0/rc"));
        assert!(pattern_matches("release*", "release-2"));
        assert!(!pattern_matches("release*", "release/2"));
        assert!(pattern_matches("release**", "release/2"));
        assert!(pattern_matches("*", "main"));
        assert!(!pattern_matches("*", "any/branch"));
        assert!(pattern_matches("**", "any/branch"));
        assert!(pattern_matches("v?", "v1"));
        assert!(!pattern_matches("v?", "v/"));
        assert!(pattern_matches("*/hotfix", "team/hotfix"));
    }

    #[test]
    fn protection_patterns_stop_at_the_stored_length() {
        assert_eq!(validate_branch_pattern(&"p".repeat(255)), Ok(()));
        assert_eq!(
            validate_branch_pattern(&"p".repeat(256)),
            Err(ProtectionWriteRefusal::InvalidPattern)
        );
    }

    #[test]
    fn literal_patterns_never_spend_the_match_budget() {
        // One wildcard charge of about 35 million cells is more than the whole budget. After it,
        // no wildcard can match, while a plain name still answers: it never reaches the budget.
        let mut budget = WorkflowMatchBudget::new();
        let wildcard = format!("*{}", "a".repeat(5000));
        assert!(branch_pattern_matches(&wildcard, &"b".repeat(7000), &mut budget).is_err());
        assert!(branch_pattern_matches("main", "main", &mut budget).unwrap());
        assert!(!branch_pattern_matches("main", "main-two", &mut budget).unwrap());
    }
}
