//! Rule suites: how each pushed ref fared against the rulesets in evaluate mode.
//!
//! Saving refuses an active ruleset that has rules, so evaluation never blocks a push; it
//! records what would have happened, as GitHub's evaluate mode does. Ref, history and name
//! rules are judged. Unsupported evaluate definitions cannot be saved; historical unsupported
//! rules produce an indeterminate verdict, never a successful evaluation.
use super::{
    git_sessions::{
        validation::{RefUpdate, ZERO_SHA},
        ReceiveCredential,
    },
    protection::History,
};
use crate::backend::{
    ci::workflows::WorkflowMatchBudget,
    db::{
        self,
        models::{AccessLevel, OrgRole, Repository, User},
        organizations::OrganizationOutcome,
        rulesets::{
            ActorType, BypassMode, Enforcement, Evaluation, Owner, Rule, RuleResult, RuleSource,
            Ruleset, SourceType, Suite, SuiteEnforcement, SuiteResult,
        },
    },
};
use anyhow::Result;
use std::collections::HashSet;

#[derive(Clone, Copy)]
enum Outcome {
    Skipped,
    Pass,
    Fail(&'static str),
    Unavailable(&'static str),
}

#[derive(Default)]
struct HistoryChecks {
    fast_forward: Option<Outcome>,
    linear: Option<Outcome>,
}

async fn exempted(
    pool: &db::DbPool,
    repository: &Repository,
    actor: &User,
    credential: Option<ReceiveCredential>,
    rulesets: &[Ruleset],
) -> Result<HashSet<i64>> {
    if actor.is_banned || actor.must_change_password {
        return Ok(HashSet::new());
    }
    let entries: Vec<_> = rulesets
        .iter()
        .flat_map(|r| &r.definition.bypass_actors)
        .filter(|entry| entry.bypass_mode == BypassMode::Exempt)
        .collect();
    let deploy_key = matches!(credential, Some(ReceiveCredential::DeployKey(_)));
    let has = |kind| !deploy_key && entries.iter().any(|entry| entry.actor_type == kind);
    let role = if has(ActorType::RepositoryRole) {
        super::access::user_access(pool, actor, repository).await?
    } else {
        AccessLevel::None
    };
    let org_admin = if has(ActorType::OrganizationAdmin) && repository.owner_kind == "org" {
        if let Some(org) = repository.owner_org_id {
            matches!(
                db::organizations::membership::get_organization_role(pool, actor.id, org).await?,
                OrganizationOutcome::Applied(OrgRole::Owner | OrgRole::Admin)
            )
        } else {
            false
        }
    } else {
        false
    };
    let mut teams = HashSet::new();
    if has(ActorType::Team) {
        let ids: HashSet<_> = entries
            .iter()
            .filter(|entry| entry.actor_type == ActorType::Team)
            .filter_map(|entry| entry.actor_id)
            .collect();
        let ids: Vec<_> = ids.into_iter().collect();
        for batch in ids.chunks(500) {
            teams.extend(db::rulesets::bypass_teams(pool, repository.id, actor.id, batch).await?);
        }
    }
    Ok(rulesets
        .iter()
        .filter(|ruleset| {
            ruleset.definition.bypass_actors.iter().any(|entry| {
                entry.bypass_mode == BypassMode::Exempt
                    && match entry.actor_type {
                        ActorType::DeployKey => deploy_key,
                        ActorType::User => !deploy_key && entry.actor_id == Some(actor.id),
                        ActorType::OrganizationAdmin => org_admin,
                        ActorType::RepositoryRole => {
                            !deploy_key
                                && entry
                                    .actor_id
                                    .is_some_and(|minimum| i64::from(role as i16) >= minimum)
                        }
                        ActorType::Team => {
                            !deploy_key && entry.actor_id.is_some_and(|id| teams.contains(&id))
                        }
                    }
            })
        })
        .map(|ruleset| ruleset.id)
        .collect())
}

fn outcome(failed: bool, message: &'static str) -> Outcome {
    if failed {
        Outcome::Fail(message)
    } else {
        Outcome::Pass
    }
}

async fn evaluate(
    rule: &Rule,
    update: &RefUpdate,
    history: &History<'_>,
    checks: &mut HistoryChecks,
) -> Outcome {
    let cached = match rule {
        Rule::NonFastForward => checks.fast_forward,
        Rule::RequiredLinearHistory => checks.linear,
        _ => None,
    };
    if let Some(cached) = cached {
        return cached;
    }
    let evaluated = evaluate_uncached(rule, update, history).await.unwrap_or_else(|error| {
        tracing::warn!(%error, reference = %update.reference, rule = rule.name(), "rule evaluation unavailable");
        Outcome::Unavailable("The rule could not be evaluated; no verdict is available")
    });
    match rule {
        Rule::NonFastForward => checks.fast_forward = Some(evaluated),
        Rule::RequiredLinearHistory => checks.linear = Some(evaluated),
        _ => {}
    }
    evaluated
}

async fn evaluate_uncached(
    rule: &Rule,
    update: &RefUpdate,
    history: &History<'_>,
) -> Result<Outcome> {
    let old = (update.old != ZERO_SHA).then_some(update.old.as_str());
    let new = (update.new != ZERO_SHA).then_some(update.new.as_str());
    Ok(match rule {
        Rule::Creation if old.is_none() => {
            Outcome::Fail("Cannot create ref due to creations being restricted")
        }
        Rule::Update { .. } if old.is_some() && new.is_some() => {
            Outcome::Fail("Cannot update this protected ref")
        }
        Rule::Deletion if new.is_none() => Outcome::Fail("Cannot delete this protected ref"),
        Rule::NonFastForward => match (old, new) {
            (Some(old), Some(new)) => outcome(
                !history.is_fast_forward(old, new).await?,
                "Cannot force-push to this ref",
            ),
            _ => Outcome::Skipped,
        },
        Rule::RequiredLinearHistory => match new {
            Some(new) => outcome(
                history.adds_merge_commit(old, new).await?,
                "This ref must not contain merge commits",
            ),
            None => Outcome::Skipped,
        },
        Rule::BranchNamePattern { parameters } => {
            match (update.reference.strip_prefix("refs/heads/"), new) {
                (Some(name), Some(_)) => outcome(
                    !parameters.matches(name)?,
                    "The branch name does not match the required pattern",
                ),
                _ => Outcome::Skipped,
            }
        }
        Rule::TagNamePattern { parameters } => {
            match (update.reference.strip_prefix("refs/tags/"), new) {
                (Some(name), Some(_)) => outcome(
                    !parameters.matches(name)?,
                    "The tag name does not match the required pattern",
                ),
                _ => Outcome::Skipped,
            }
        }
        Rule::Creation | Rule::Update { .. } | Rule::Deletion => Outcome::Skipped,
        _ => Outcome::Unavailable("This rule type is not implemented; no verdict is available"),
    })
}

/// Records one rule suite for each pushed ref that an evaluate-mode ruleset targets.
pub(crate) async fn record_push(
    pool: &db::DbPool,
    repository: &Repository,
    actor: &User,
    updates: &[RefUpdate],
    history: &History<'_>,
    credential: Option<ReceiveCredential>,
) -> Result<()> {
    let mut rulesets = db::rulesets::push_rulesets(pool, repository.id).await?;
    rulesets.retain(|ruleset| ruleset.definition.enforcement == Enforcement::Evaluate);
    if rulesets.is_empty() {
        return Ok(());
    }
    let exempted = exempted(pool, repository, actor, credential, &rulesets).await?;
    rulesets.retain(|ruleset| !exempted.contains(&ruleset.id));
    let mut budget = WorkflowMatchBudget::new();
    for update in updates {
        let mut targeted = false;
        let mut evaluations = Vec::new();
        let mut checks = HistoryChecks::default();
        let mut incomplete = false;
        let mut failed = false;
        for ruleset in &rulesets {
            if !ruleset.definition.matches_reference(
                &update.reference,
                &repository.default_branch,
                &mut budget,
            )? {
                continue;
            }
            targeted = true;
            let source = RuleSource {
                id: Some(ruleset.id),
                name: Some(ruleset.definition.name.clone()),
                source_type: match ruleset.owner {
                    Owner::Organization(_) => SourceType::Organization,
                    Owner::Repository(_) => SourceType::Repository,
                },
            };
            for rule in &ruleset.definition.rules {
                let (result, details, metadata) =
                    match evaluate(rule, update, history, &mut checks).await {
                        Outcome::Skipped => continue,
                        Outcome::Pass => (RuleResult::Pass, None, None),
                        Outcome::Fail(message) => {
                            failed = true;
                            (RuleResult::Fail, Some(message.to_string()), None)
                        }
                        Outcome::Unavailable(message) => {
                            incomplete = true;
                            (
                                RuleResult::Fail,
                                Some(message.to_string()),
                                Some(serde_json::json!({ "evaluation_error": true })),
                            )
                        }
                    };
                evaluations.push(Evaluation {
                    rule_source: source.clone(),
                    enforcement: SuiteEnforcement::Evaluate,
                    result,
                    rule_type: rule.name().to_string(),
                    details,
                    metadata,
                });
            }
        }
        if !targeted {
            continue;
        }
        db::rulesets::record_rule_suite(
            pool,
            &Suite {
                id: 0,
                org_id: repository
                    .owner_org_id
                    .filter(|_| repository.owner_kind == "org"),
                repo_id: repository.id,
                repository_name: repository.name.clone(),
                actor_id: Some(actor.id),
                actor_name: Some(actor.username.clone()),
                reference: update.reference.clone(),
                before_sha: update.old.clone(),
                after_sha: update.new.clone(),
                // Nothing is enforced, so the push itself passes; the evaluation says what
                // enforcement would have done.
                result: SuiteResult::Pass,
                evaluation_result: if failed {
                    Some(SuiteResult::Fail)
                } else if incomplete {
                    None
                } else {
                    Some(SuiteResult::Pass)
                },
                evaluations,
                created_at: chrono::Utc::now(),
            },
        )
        .await?;
    }
    Ok(())
}
