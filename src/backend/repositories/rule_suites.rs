//! Rule suites: how each pushed ref fared against the rulesets in evaluate mode.
//!
//! Saving refuses an active ruleset that has rules, so evaluation never blocks a push; it
//! records what would have happened, as GitHub's evaluate mode does. Ref, history and name
//! rules are judged. Commit-metadata and file rules are not judged yet and are left out.
use super::{
    git_sessions::validation::{RefUpdate, ZERO_SHA},
    protection::History,
};
use crate::backend::{
    ci::workflows::WorkflowMatchBudget,
    db::{
        self,
        models::{Repository, User},
        rulesets::{
            Enforcement, Evaluation, Owner, Rule, RuleResult, RuleSource, SourceType, Suite,
            SuiteEnforcement, SuiteResult,
        },
    },
};
use anyhow::Result;

enum Outcome {
    Skipped,
    Pass,
    Fail(&'static str),
}

fn outcome(failed: bool, message: &'static str) -> Outcome {
    if failed {
        Outcome::Fail(message)
    } else {
        Outcome::Pass
    }
}

async fn evaluate(rule: &Rule, update: &RefUpdate, history: &History<'_>) -> Result<Outcome> {
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
        _ => Outcome::Skipped,
    })
}

/// Records one rule suite for each pushed ref that an evaluate-mode ruleset targets.
pub(crate) async fn record_push(
    pool: &db::DbPool,
    repository: &Repository,
    actor: &User,
    updates: &[RefUpdate],
    history: &History<'_>,
) -> Result<()> {
    let mut rulesets = db::rulesets::push_rulesets(pool, repository.id).await?;
    rulesets.retain(|ruleset| ruleset.definition.enforcement == Enforcement::Evaluate);
    if rulesets.is_empty() {
        return Ok(());
    }
    let mut budget = WorkflowMatchBudget::new();
    for update in updates {
        let mut targeted = false;
        let mut evaluations = Vec::new();
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
                let (result, details) = match evaluate(rule, update, history).await? {
                    Outcome::Skipped => continue,
                    Outcome::Pass => (RuleResult::Pass, None),
                    Outcome::Fail(message) => (RuleResult::Fail, Some(message.to_string())),
                };
                evaluations.push(Evaluation {
                    rule_source: source.clone(),
                    enforcement: SuiteEnforcement::Evaluate,
                    result,
                    rule_type: rule.name().to_string(),
                    details,
                    metadata: None,
                });
            }
        }
        if !targeted {
            continue;
        }
        let failed = evaluations
            .iter()
            .any(|evaluation| evaluation.result == RuleResult::Fail);
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
                evaluation_result: Some(if failed {
                    SuiteResult::Fail
                } else {
                    SuiteResult::Pass
                }),
                evaluations,
                created_at: chrono::Utc::now(),
            },
        )
        .await?;
    }
    Ok(())
}
