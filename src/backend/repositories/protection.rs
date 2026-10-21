//! The one decision about who may change which branch.
//!
//! Git pushes, the file editor, the contents and branch APIs, and pull request merges all
//! ask `check_branch_write`. Every matching rule must pass, so the most restrictive one wins.
//! A rule with `enforce_admins` off does not apply to repository administrators, except that
//! blocked force pushes stay blocked for them too, unless the rule lists them: they are easy
//! mistakes.
use crate::backend::ci::workflows::WorkflowMatchBudget;
use crate::backend::db;
use crate::backend::db::models::{BranchProtectionRule, Repository, User};
use crate::backend::git;
use crate::backend::repositories::access;
use anyhow::Result;
use std::path::Path;

pub const PULL_REQUEST_REQUIRED: &str =
    "Changes to this protected branch must be made through a pull request.";
pub const DELETION_BLOCKED: &str = "Deleting this protected branch is blocked.";
pub const PUSHER_NOT_ALLOWED: &str = "This actor is not permitted to update the protected branch.";
pub const FORCE_PUSH_BLOCKED: &str = "Force pushes to this protected branch are blocked.";
pub const MERGE_COMMIT_BLOCKED: &str =
    "Merge commits are not allowed on this protected branch, which requires a linear history.";

/// Stands in for a tip whose sha the caller has not read or built yet, such as the editor's
/// commit before it is prepared. The rules only ask whether a tip is there.
pub const ANY_TIP: &str = "any-tip";

/// How a branch write arrives. A merge has met the pull request rules by being one; the
/// rest of its gates are the merge policy's. A deploy key pushes directly, but never counts
/// as a repository administrator, so rules that exempt administrators still apply to it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WriteVia {
    Direct,
    DeployKey,
    Merge,
}

impl WriteVia {
    fn is_direct(self) -> bool {
        self != WriteVia::Merge
    }
}

/// One branch move. Creating a branch has no `old` and deleting one has no `new`.
pub struct BranchWrite<'a> {
    pub branch: &'a str,
    pub old: Option<&'a str>,
    pub new: Option<&'a str>,
    pub via: WriteVia,
}

/// What the new commits of a write look like, asked only when a rule needs the answer.
pub enum History<'a> {
    /// The write adds one commit with the old tip as its only parent, or no commit at all:
    /// the editor's save and a branch delete.
    SingleCommit,
    /// A push whose objects sit in `quarantine` of the repository at `repository`. Without a
    /// quarantine the history cannot be read and the question is refused.
    Push {
        repository: &'a Path,
        quarantine: Option<&'a Path>,
    },
    /// Commits already stored in `repository`, which is not quarantined: a prepared merge, or
    /// a branch created at an existing commit.
    Stored { repository: &'a Path },
}

impl History<'_> {
    async fn is_fast_forward(&self, old: &str, new: &str) -> Result<bool> {
        match self {
            Self::SingleCommit => Ok(true),
            Self::Push {
                repository,
                quarantine,
            } => {
                let quarantine = quarantine.ok_or_else(|| anyhow::anyhow!("no quarantine"))?;
                git::repository::quarantined_is_fast_forward(repository, quarantine, old, new).await
            }
            Self::Stored { repository } => {
                git::repository::commit_is_ancestor(repository, old, new).await
            }
        }
    }

    async fn adds_merge_commit(&self, old: Option<&str>, new: &str) -> Result<bool> {
        match self {
            Self::SingleCommit => Ok(false),
            Self::Push {
                repository,
                quarantine,
            } => {
                let quarantine = quarantine.ok_or_else(|| anyhow::anyhow!("no quarantine"))?;
                git::repository::adds_merge_commit(repository, Some(quarantine), old, new).await
            }
            Self::Stored { repository } => {
                git::repository::adds_merge_commit(repository, None, old, new).await
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A rule forbids the write. The text says which.
    Blocked(&'static str),
    /// The rules or the history could not be read, so nothing is allowed.
    Unavailable,
}

/// The branch protection rules of `repository`. A push reads them once and judges each of its
/// branches against that one read.
pub async fn load_rules(
    pool: &db::DbPool,
    repository: &Repository,
) -> Result<Vec<BranchProtectionRule>> {
    db::repositories::protection::list_branch_protection_rules(pool, repository.id).await
}

/// The rules of `repository` that match `branch`.
pub async fn matching_rules(
    pool: &db::DbPool,
    repository: &Repository,
    branch: &str,
) -> Result<Vec<BranchProtectionRule>> {
    let rules = load_rules(pool, repository).await?;
    let matching = rules_for_branch(&rules, branch, &mut WorkflowMatchBudget::new())
        .map_err(|_| anyhow::anyhow!("branch protection could not be checked"))?;
    Ok(matching.into_iter().cloned().collect())
}

/// The rules in `rules` whose pattern matches `branch`. Running out of `budget` is
/// `Unavailable`: the rules that apply are then unknown, not absent.
fn rules_for_branch<'a>(
    rules: &'a [BranchProtectionRule],
    branch: &str,
    budget: &mut WorkflowMatchBudget,
) -> Result<Vec<&'a BranchProtectionRule>, Refusal> {
    let mut matching = Vec::new();
    for rule in rules {
        let matches =
            db::repositories::protection::branch_pattern_matches(&rule.pattern, branch, budget)
                .map_err(|_| Refusal::Unavailable)?;
        if matches {
            matching.push(rule);
        }
    }
    Ok(matching)
}

/// Decides one branch write for `actor`, who already has write access. On success it returns
/// the rules the actor is still held to, so the merge policy can apply its own gates to
/// exactly those.
pub async fn check_branch_write(
    pool: &db::DbPool,
    repository: &Repository,
    actor: &User,
    write: &BranchWrite<'_>,
    history: &History<'_>,
) -> Result<Vec<BranchProtectionRule>, Refusal> {
    let rules = load_rules(pool, repository)
        .await
        .map_err(|_| Refusal::Unavailable)?;
    let mut budget = WorkflowMatchBudget::new();
    check_branch_write_with(pool, &rules, repository, actor, write, history, &mut budget).await
}

/// `check_branch_write` against rules that the caller read once for several writes. Writes
/// that share one `budget` share its matching work.
pub async fn check_branch_write_with(
    pool: &db::DbPool,
    rules: &[BranchProtectionRule],
    repository: &Repository,
    actor: &User,
    write: &BranchWrite<'_>,
    history: &History<'_>,
    budget: &mut WorkflowMatchBudget,
) -> Result<Vec<BranchProtectionRule>, Refusal> {
    let matching = rules_for_branch(rules, write.branch, budget)?;
    // Decided before the loop below takes the rules: a force push is refused for everyone the
    // rules do not allow, administrators included.
    let force_push_blocked = blocks_force_push(&matching, actor.id);
    let is_admin = write.via != WriteVia::DeployKey
        && matching.iter().any(|rule| !rule.enforce_admins)
        && access::can_admin(pool, actor, repository)
            .await
            .map_err(|_| Refusal::Unavailable)?;

    let mut enforced = Vec::new();
    for rule in matching {
        if is_admin && !rule.enforce_admins {
            continue;
        }
        if let Some(reason) = refusal(rule, actor.id, write) {
            return Err(Refusal::Blocked(reason));
        }
        enforced.push(rule.clone());
    }

    // Only a write that leaves a tip can force or add merge commits, and the history is read
    // last, only when a rule that is still enforced asks for it. A merge is never a force
    // push, but its result can still add merge commits.
    if let Some(new) = write.new {
        if write.via.is_direct() {
            if let Some(old) = write.old {
                if force_push_blocked
                    && !history
                        .is_fast_forward(old, new)
                        .await
                        .map_err(|_| Refusal::Unavailable)?
                {
                    return Err(Refusal::Blocked(FORCE_PUSH_BLOCKED));
                }
            }
        }
        if enforced.iter().any(|rule| rule.require_linear_history)
            && history
                .adds_merge_commit(write.old, new)
                .await
                .map_err(|_| Refusal::Unavailable)?
        {
            return Err(Refusal::Blocked(MERGE_COMMIT_BLOCKED));
        }
    }
    Ok(enforced)
}

/// Whether `actor_id` may not force push to a branch whose matching rules are `matching`. A rule
/// that blocks force pushes refuses everyone who is not in its `force_push_allowances`.
fn blocks_force_push(matching: &[&BranchProtectionRule], actor_id: i64) -> bool {
    matching
        .iter()
        .any(|rule| rule.block_force_pushes && !rule.force_push_allowances.contains(&actor_id))
}

/// The refusal that needs no Git: a rule that wants a pull request, that blocks deleting the
/// branch, or that limits who may push.
fn refusal(
    rule: &BranchProtectionRule,
    actor_id: i64,
    write: &BranchWrite<'_>,
) -> Option<&'static str> {
    if write.via.is_direct() {
        // Reviews, checks and resolved conversations only exist on a pull request, so a rule
        // that asks for any of them leaves no direct path.
        if rule.require_pull_request
            || rule.required_approving_reviews > 0
            || rule.require_status_checks
            || rule.require_conversation_resolution
            || rule.require_code_owner_review
        {
            return Some(PULL_REQUEST_REQUIRED);
        }
        if write.new.is_none() && rule.block_deletions {
            return Some(DELETION_BLOCKED);
        }
    }
    if rule.restrict_pushes && !rule.push_allowances.contains(&actor_id) {
        return Some(PUSHER_NOT_ALLOWED);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule() -> BranchProtectionRule {
        BranchProtectionRule {
            id: 1,
            repo_id: 1,
            pattern: "main".into(),
            require_pull_request: false,
            required_approving_reviews: 0,
            dismiss_stale_reviews: true,
            require_status_checks: false,
            required_status_checks: vec![],
            require_up_to_date_branch: false,
            require_linear_history: false,
            block_force_pushes: true,
            block_deletions: true,
            restrict_pushes: false,
            push_allowances: vec![],
            force_push_allowances: vec![],
            enforce_admins: true,
            require_conversation_resolution: false,
            require_code_owner_review: false,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn write(
        old: Option<&'static str>,
        new: Option<&'static str>,
        via: WriteVia,
    ) -> BranchWrite<'static> {
        BranchWrite {
            branch: "main",
            old,
            new,
            via,
        }
    }

    const OLD: Option<&str> = Some("a");
    const NEW: Option<&str> = Some("b");

    #[test]
    fn every_pull_request_family_setting_blocks_direct_writes_only() {
        let mut approvals = rule();
        approvals.required_approving_reviews = 1;
        let mut checks = rule();
        checks.require_status_checks = true;
        let mut conversations = rule();
        conversations.require_conversation_resolution = true;
        let mut code_owners = rule();
        code_owners.require_code_owner_review = true;
        let mut pull_request = rule();
        pull_request.require_pull_request = true;
        for rule in [approvals, checks, conversations, code_owners, pull_request] {
            assert_eq!(
                refusal(&rule, 1, &write(OLD, NEW, WriteVia::Direct)),
                Some(PULL_REQUEST_REQUIRED)
            );
            assert_eq!(refusal(&rule, 1, &write(OLD, NEW, WriteVia::Merge)), None);
        }
        // Force pushes are blocked by default, but a plain update passes the Git-free checks.
        assert_eq!(
            refusal(&rule(), 1, &write(OLD, NEW, WriteVia::Direct)),
            None
        );
    }

    #[test]
    fn force_pushes_are_refused_outside_the_allowlist() {
        let mut allowlisted = rule();
        allowlisted.force_push_allowances = vec![7];
        assert!(!blocks_force_push(&[&allowlisted], 7));
        assert!(blocks_force_push(&[&allowlisted], 8));
        let mut open = rule();
        open.block_force_pushes = false;
        assert!(!blocks_force_push(&[&open], 8));
        assert!(!blocks_force_push(&[], 8));
    }

    #[test]
    fn deletion_is_refused_unless_the_rule_allows_it() {
        assert_eq!(
            refusal(&rule(), 1, &write(OLD, None, WriteVia::Direct)),
            Some(DELETION_BLOCKED)
        );
        let mut allowing = rule();
        allowing.block_deletions = false;
        assert_eq!(
            refusal(&allowing, 1, &write(OLD, None, WriteVia::Direct)),
            None
        );
    }

    #[test]
    fn restricted_pushes_need_an_allowance_for_direct_writes_and_merges() {
        let mut restricted = rule();
        restricted.restrict_pushes = true;
        restricted.push_allowances = vec![7];
        for via in [WriteVia::Direct, WriteVia::Merge] {
            assert_eq!(refusal(&restricted, 7, &write(OLD, NEW, via)), None);
            assert_eq!(
                refusal(&restricted, 8, &write(OLD, NEW, via)),
                Some(PUSHER_NOT_ALLOWED)
            );
        }
        // An allowance does not skip the pull request requirement.
        restricted.require_pull_request = true;
        assert_eq!(
            refusal(&restricted, 7, &write(OLD, NEW, WriteVia::Direct)),
            Some(PULL_REQUEST_REQUIRED)
        );
    }

    #[test]
    fn an_exhausted_match_budget_refuses_instead_of_matching_no_rule() {
        // One wildcard charge of about 35 million cells is more than the whole budget.
        let mut long = rule();
        long.pattern = format!("*{}", "a".repeat(5000));
        let rules = [long];
        let mut budget = WorkflowMatchBudget::new();
        assert_eq!(
            rules_for_branch(&rules, &"b".repeat(7000), &mut budget).err(),
            Some(Refusal::Unavailable)
        );
    }
}
