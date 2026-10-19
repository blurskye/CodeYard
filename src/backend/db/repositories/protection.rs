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
    #[error("Use a branch name or a pattern with *, ** or ?")]
    InvalidPattern,
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
    pub force_push_allowances: Vec<i64>,
    pub enforce_admins: bool,
    pub require_conversation_resolution: bool,
    pub require_code_owner_review: bool,
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
            force_push_allowances: Vec::new(),
            enforce_admins: true,
            require_conversation_resolution: false,
            require_code_owner_review: false,
        }
    }
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

/// Whether `pattern` names `branch`. A pattern without `*` or `?` is a plain name and is only
/// compared. A wildcard pattern is matched within `budget`, and running out of it is an error,
/// never a non-match.
pub(crate) fn branch_pattern_matches(
    pattern: &str,
    branch: &str,
    budget: &mut WorkflowMatchBudget,
) -> std::result::Result<bool, WorkflowMatchLimit> {
    if !pattern.contains(['*', '?']) {
        return Ok(pattern == branch);
    }
    github_glob_matches_bounded(pattern, branch, budget)
}

pub fn server_owned_status_context(context: &str) -> bool {
    context.starts_with("gitserv/")
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

#[cfg(test)]
mod tests {
    use super::*;

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
