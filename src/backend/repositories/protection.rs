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
pub const BRANCH_LOCKED: &str = "This branch is locked and cannot be changed.";
pub const SIGNATURE_REQUIRED: &str =
    "Commits must have verified signatures on this protected branch.";
pub const MERGE_COMMIT_BLOCKED: &str =
    "Merge commits are not allowed on this protected branch, which requires a linear history.";

/// Stands in for a tip whose sha the caller has not read or built yet, such as the editor's
/// commit before it is prepared. The rules only ask whether a tip is there.
pub const ANY_TIP: &str = "any-tip";

/// How a branch write arrives. A merge has met the pull request rules by being one; the
/// rest of its gates are the merge policy's. A deploy key pushes directly, but never counts
/// as a repository administrator, so rules that exempt administrators still apply to it.
/// A fork sync is a direct write from the fork's upstream, which a locked branch may allow.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WriteVia {
    Direct,
    DeployKey,
    Merge,
    ForkSync,
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
    /// Every added commit must carry a signature that the signer's registered keys verify.
    /// The editor's own commits are unsigned.
    async fn has_unverified_commits(
        &self,
        pool: &db::DbPool,
        old: Option<&str>,
        new: &str,
    ) -> Result<bool> {
        let commits = match self {
            Self::SingleCommit => return Ok(true),
            Self::Push {
                repository,
                quarantine,
            } => {
                let quarantine = quarantine.ok_or_else(|| anyhow::anyhow!("no quarantine"))?;
                git::repository::introduced_signatures(repository, Some(quarantine), old, new)
                    .await?
            }
            Self::Stored { repository } => {
                git::repository::introduced_signatures(repository, None, old, new).await?
            }
        };
        for commit in &commits {
            let reason = crate::backend::identity::signatures::check(
                pool,
                crate::backend::identity::signatures::Signed {
                    signature: commit.signature.as_deref(),
                    payload: commit.payload.as_deref(),
                    email: Some(&commit.committer_email),
                },
            )
            .await?;
            if reason != "valid" {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) async fn is_fast_forward(&self, old: &str, new: &str) -> Result<bool> {
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

    pub(crate) async fn adds_merge_commit(&self, old: Option<&str>, new: &str) -> Result<bool> {
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

/// Whether any loaded rule protects this branch, using the same matching budget as writes.
pub fn branch_is_protected(
    rules: &[BranchProtectionRule],
    branch: &str,
    budget: &mut WorkflowMatchBudget,
) -> Result<bool, Refusal> {
    Ok(!rules_for_branch(rules, branch, budget)?.is_empty())
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
        // Checked before the administrator skip: a lock binds administrators too.
        if locks(rule, write) {
            return Err(Refusal::Blocked(BRANCH_LOCKED));
        }
        if is_admin && !rule.enforce_admins {
            continue;
        }
        let team_allowed = if rule.restrict_pushes
            && !rule.push_allowances.contains(&actor.id)
            && !rule.push_team_allowances.is_empty()
        {
            db::repositories::push_teams::contains_actor(
                pool,
                repository.id,
                actor.id,
                &rule.push_team_allowances,
            )
            .await
            .map_err(|_| Refusal::Unavailable)?
        } else {
            false
        };
        let app_allowed = if rule.restrict_pushes
            && !rule.push_allowances.contains(&actor.id)
            && !team_allowed
        {
            app_allows_actor(
                pool,
                repository.id,
                actor.id,
                &rule.github_settings.push_apps,
            )
            .await
            .map_err(|_| Refusal::Unavailable)?
        } else {
            false
        };
        let bypass_allowed = write.new.is_some()
            && write.via.is_direct()
            && needs_pull_request(rule)
            && bypass_allows(
                pool,
                repository.id,
                actor.id,
                rule.github_settings.bypass_pull_request_allowances.as_ref(),
            )
            .await
            .map_err(|_| Refusal::Unavailable)?;
        if let Some(reason) = refusal(
            rule,
            actor.id,
            write,
            team_allowed || app_allowed,
            bypass_allowed,
        ) {
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
        if enforced
            .iter()
            .any(|rule| rule.github_settings.required_signatures)
            && history
                .has_unverified_commits(pool, write.old, new)
                .await
                .map_err(|_| Refusal::Unavailable)?
        {
            return Err(Refusal::Blocked(SIGNATURE_REQUIRED));
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

async fn app_allows_actor(pool: &db::DbPool, repo: i64, actor: i64, ids: &[i64]) -> Result<bool> {
    for id in ids {
        let Some(app) = db::github_apps::app(pool, &id.to_string()).await? else {
            continue;
        };
        if app.bot_id != actor {
            continue;
        }
        if db::github_apps::repository_installation(pool, app.id, repo, true, None)
            .await?
            .is_some()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `actor_id` may not force push to a branch whose matching rules are `matching`. A rule
/// that blocks force pushes refuses everyone who is not in its `force_push_allowances`.
fn blocks_force_push(matching: &[&BranchProtectionRule], actor_id: i64) -> bool {
    matching
        .iter()
        .any(|rule| rule.block_force_pushes && !rule.force_push_allowances.contains(&actor_id))
}

/// Whether a direct content change to a branch with this rule must arrive as a pull request.
/// Reviews, checks and resolved conversations only exist on a pull request.
fn needs_pull_request(rule: &BranchProtectionRule) -> bool {
    rule.require_pull_request
        || rule.required_approving_reviews > 0
        || rule.require_status_checks
        || rule.require_conversation_resolution
        || rule.require_code_owner_review
}

/// Whether the actor is named by a rule's pull request bypass list. The stored document keeps
/// each user, team and app as an object with its `id`. A team or app counts only while the actor
/// still belongs to it.
async fn bypass_allows(
    pool: &db::DbPool,
    repo: i64,
    actor: i64,
    document: Option<&serde_json::Value>,
) -> Result<bool> {
    let Some(document) = document else {
        return Ok(false);
    };
    let ids = |kind: &str| -> Vec<i64> {
        document
            .get(kind)
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.get("id").and_then(serde_json::Value::as_i64))
            .collect()
    };
    if ids("users").contains(&actor) {
        return Ok(true);
    }
    let teams = ids("teams");
    if !teams.is_empty()
        && db::repositories::push_teams::contains_actor(pool, repo, actor, &teams).await?
    {
        return Ok(true);
    }
    app_allows_actor(pool, repo, actor, &ids("apps")).await
}

/// A locked branch is read-only for everyone, apart from syncing a fork with its upstream when
/// the rule allows that.
fn locks(rule: &BranchProtectionRule, write: &BranchWrite<'_>) -> bool {
    rule.github_settings.lock_branch
        && !(write.via == WriteVia::ForkSync && rule.github_settings.allow_fork_syncing)
}

/// The refusal that needs no Git: a locked branch, a rule that wants a pull request for content
/// changes, a rule that blocks deleting the branch, or one that limits who may push.
fn refusal(
    rule: &BranchProtectionRule,
    actor_id: i64,
    write: &BranchWrite<'_>,
    team_allowed: bool,
    bypass_allowed: bool,
) -> Option<&'static str> {
    if locks(rule, write) {
        return Some(BRANCH_LOCKED);
    }
    if write.via.is_direct() {
        // Deleting a branch needs no pull request, so only content changes wait for one. A
        // bypass skips the pull request but not required checks, which a pushed commit has not
        // passed yet.
        let bypassed = bypass_allowed && !rule.require_status_checks;
        if write.new.is_some() && needs_pull_request(rule) && !bypassed {
            return Some(PULL_REQUEST_REQUIRED);
        }
        if write.new.is_none() && rule.block_deletions {
            return Some(DELETION_BLOCKED);
        }
    }
    // Creating a branch is limited by push restrictions only when the rule blocks creations.
    let creation_allowed =
        write.old.is_none() && write.new.is_some() && !rule.github_settings.block_creations;
    if rule.restrict_pushes
        && !creation_allowed
        && !rule.push_allowances.contains(&actor_id)
        && !team_allowed
    {
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
            push_team_allowances: vec![],
            force_push_allowances: vec![],
            enforce_admins: true,
            require_conversation_resolution: false,
            require_code_owner_review: false,
            github_settings: Default::default(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn deletion_needs_no_pull_request_and_a_lock_refuses_every_write() {
        let mut restricted = rule();
        restricted.require_pull_request = true;
        restricted.block_deletions = false;
        assert_eq!(
            refusal(
                &restricted,
                1,
                &write(OLD, None, WriteVia::Direct),
                false,
                false
            ),
            None
        );
        assert_eq!(
            refusal(
                &restricted,
                1,
                &write(OLD, NEW, WriteVia::Direct),
                false,
                false
            ),
            Some(PULL_REQUEST_REQUIRED)
        );
        assert_eq!(
            refusal(
                &restricted,
                1,
                &write(OLD, NEW, WriteVia::Direct),
                false,
                true
            ),
            None
        );
        restricted.require_status_checks = true;
        assert_eq!(
            refusal(
                &restricted,
                1,
                &write(OLD, NEW, WriteVia::Direct),
                false,
                true
            ),
            Some(PULL_REQUEST_REQUIRED)
        );
        restricted.github_settings.lock_branch = true;
        assert_eq!(
            refusal(
                &restricted,
                1,
                &write(OLD, NEW, WriteVia::Merge),
                false,
                true
            ),
            Some(BRANCH_LOCKED)
        );
        assert_eq!(
            refusal(
                &restricted,
                1,
                &write(OLD, None, WriteVia::Direct),
                false,
                false
            ),
            Some(BRANCH_LOCKED)
        );
        // A fork sync the rule allows passes the lock. It is a direct write, so the rule
        // must not want a pull request or checks either.
        restricted.github_settings.allow_fork_syncing = true;
        restricted.require_pull_request = false;
        restricted.require_status_checks = false;
        assert_eq!(
            refusal(
                &restricted,
                1,
                &write(OLD, NEW, WriteVia::ForkSync),
                false,
                false
            ),
            None
        );
    }

    #[test]
    fn a_team_allowance_does_not_bypass_other_branch_requirements() {
        let mut restricted = rule();
        restricted.restrict_pushes = true;
        restricted.push_team_allowances = vec![9];
        assert_eq!(
            refusal(
                &restricted,
                7,
                &write(OLD, NEW, WriteVia::Direct),
                true,
                false
            ),
            None
        );
        assert_eq!(
            refusal(
                &restricted,
                7,
                &write(OLD, NEW, WriteVia::Direct),
                false,
                false
            ),
            Some(PUSHER_NOT_ALLOWED)
        );
        restricted.require_pull_request = true;
        assert_eq!(
            refusal(
                &restricted,
                7,
                &write(OLD, NEW, WriteVia::Direct),
                true,
                false
            ),
            Some(PULL_REQUEST_REQUIRED)
        );
        assert_eq!(
            refusal(
                &restricted,
                7,
                &write(OLD, NEW, WriteVia::Merge),
                true,
                false
            ),
            None
        );
    }

    #[test]
    fn users_and_teams_share_the_push_allowance_bound() {
        use db::repositories::protection::{
            apply_protection_update, ActorListUpdate, ProtectionUpdate, ProtectionWriteRefusal,
        };
        let mut rule = rule();
        rule.restrict_pushes = true;
        rule.push_allowances = (1..=100).collect();
        let error = apply_protection_update(
            &mut rule,
            ProtectionUpdate::PushTeams(ActorListUpdate::Add(vec![1])),
        )
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<ProtectionWriteRefusal>(),
            Some(&ProtectionWriteRefusal::InvalidPushTeams)
        );
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
                refusal(&rule, 1, &write(OLD, NEW, WriteVia::Direct), false, false),
                Some(PULL_REQUEST_REQUIRED)
            );
            assert_eq!(
                refusal(&rule, 1, &write(OLD, NEW, WriteVia::Merge), false, false),
                None
            );
        }
        // Force pushes are blocked by default, but a plain update passes the Git-free checks.
        assert_eq!(
            refusal(&rule(), 1, &write(OLD, NEW, WriteVia::Direct), false, false),
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
            refusal(
                &rule(),
                1,
                &write(OLD, None, WriteVia::Direct),
                false,
                false
            ),
            Some(DELETION_BLOCKED)
        );
        let mut allowing = rule();
        allowing.block_deletions = false;
        assert_eq!(
            refusal(
                &allowing,
                1,
                &write(OLD, None, WriteVia::Direct),
                false,
                false
            ),
            None
        );
    }

    #[test]
    fn restricted_pushes_need_an_allowance_for_direct_writes_and_merges() {
        let mut restricted = rule();
        restricted.restrict_pushes = true;
        restricted.push_allowances = vec![7];
        for via in [WriteVia::Direct, WriteVia::Merge] {
            assert_eq!(
                refusal(&restricted, 7, &write(OLD, NEW, via), false, false),
                None
            );
            assert_eq!(
                refusal(&restricted, 8, &write(OLD, NEW, via), false, false),
                Some(PUSHER_NOT_ALLOWED)
            );
        }
        // An allowance does not skip the pull request requirement.
        restricted.require_pull_request = true;
        assert_eq!(
            refusal(
                &restricted,
                7,
                &write(OLD, NEW, WriteVia::Direct),
                false,
                false
            ),
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
