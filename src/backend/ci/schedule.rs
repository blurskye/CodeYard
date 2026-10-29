//! UTC schedules are discovered on the default branch; enqueue serializes each minute across nodes.
use super::{admission::AdmissionError, CiRunRequest, CiRuns};
use crate::backend::{db, repositories::store::repo_store, webhooks::payloads};
use anyhow::{ensure, Context, Result};
use chrono::{DateTime, TimeZone, Timelike, Utc};
use croner::Cron;
use serde_json::json;

pub(super) fn parse(expression: &str) -> Result<Cron> {
    ensure!(expression.len() <= 256, "cron expression is too long");
    let fields: Vec<_> = expression.split_whitespace().collect();
    ensure!(fields.len() == 5, "cron requires five fields");
    for (index, field) in fields.iter().enumerate() {
        let mut field = field.to_ascii_uppercase();
        let names: &[&str] = match index {
            3 => &[
                "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
            ],
            4 => &["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"],
            _ => &[],
        };
        for name in names {
            field = field.replace(name, "1");
        }
        ensure!(
            field
                .chars()
                .all(|ch| ch.is_ascii_digit() || "*,-/".contains(ch)),
            "unsupported cron syntax"
        );
    }
    let cron: Cron = expression.parse()?;
    let minutes: Cron = format!("{} * * * *", fields[0]).parse()?;
    let start = Utc
        .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
        .single()
        .context("invalid cron date")?;
    let mut previous = None;
    let mut first = None;
    for minute in 0..60 {
        if minutes.is_time_matching(&(start + chrono::Duration::minutes(minute)))? {
            ensure!(
                previous.is_none_or(|previous| minute - previous >= 5),
                "cron interval is less than five minutes"
            );
            first.get_or_insert(minute);
            previous = Some(minute);
        }
    }
    if let (Some(first), Some(last)) = (first, previous) {
        ensure!(
            60 + first - last >= 5,
            "cron interval is less than five minutes"
        );
    }
    Ok(cron)
}

impl CiRuns {
    /// Runtime and deterministic tests use the same UTC minute admission path.
    pub async fn queue_schedules(&self, now: DateTime<Utc>) -> Result<()> {
        let now = now
            .with_second(0)
            .and_then(|now| now.with_nanosecond(0))
            .context("invalid schedule time")?;
        let mut after = 0;
        loop {
            let ids = db::ci::scheduled_repositories(&self.pool, after).await?;
            if ids.is_empty() {
                break;
            }
            for id in ids {
                after = id;
                if let Err(error) = self.queue_repository_schedules(id, now).await {
                    tracing::warn!(repo_id=id, %error, "scheduled workflows were not queued");
                }
            }
        }
        Ok(())
    }

    async fn queue_repository_schedules(
        &self,
        id: i64,
        now: DateTime<Utc>,
    ) -> Result<(), AdmissionError> {
        let Some(repo) = db::repositories::records::get_repository_by_id(&self.pool, id).await?
        else {
            return Ok(());
        };
        // With Actions turned off, nothing would be queued, so Git is not read either.
        if !db::action_settings::actions_enabled(&self.pool, repo.id).await? {
            return Ok(());
        }
        let store = repo_store(&repo, self.cluster.as_ref());
        let reference = format!("refs/heads/{}", repo.default_branch);
        let commit = match store.commit(&reference).await {
            Ok(commit) => commit,
            Err(error)
                if error.kind == crate::backend::repositories::store::ErrorKind::NotFound =>
            {
                return Ok(())
            }
            Err(error) => return Err(error.into()),
        };
        let Some(proof) = store.branch_publication(repo.id, &reference).await? else {
            return Ok(());
        };
        if proof.publication.new != commit.sha {
            return Ok(());
        }
        let Some(actor) = proof.publication.actor.user_id() else {
            return Ok(());
        };
        if !self.actor_may_discover_workflows(repo.id, actor).await? {
            return Ok(());
        }
        let definitions = super::list_workflows_for_ref(&store, &commit.sha).await?;
        let owner = db::ci::ci_repository_owner(&self.pool, repo.id)
            .await?
            .ok_or(AdmissionError::Unavailable)?;
        let sender = db::users::accounts::get_user_by_id(&self.pool, actor)
            .await?
            .ok_or(AdmissionError::Unavailable)?;
        for definition in definitions {
            for expression in &definition.triggers.schedules {
                if !parse(expression)
                    .and_then(|cron| Ok(cron.is_time_matching(&now)?))
                    .unwrap_or(false)
                {
                    continue;
                }
                let payload = json!({"schedule": expression, "scheduled_minute": now.timestamp().div_euclid(60), "repository": payloads::repository(&repo, &owner), "sender":payloads::user(&sender)});
                match self
                    .trigger_event_run(
                        CiRunRequest {
                            repo_id: repo.id,
                            repo_owner: owner.clone(),
                            repo_name: repo.name.clone(),
                            branch: reference.clone(),
                            head_sha: Some(commit.sha.clone()),
                            event: "schedule".into(),
                            pull_request_id: None,
                            pipeline: super::infer_pipeline_from_workflow_path(&definition.path),
                            workflow_paths: vec![definition.path.clone()],
                            environment: None,
                            inputs: None,
                            triggered_by_id: actor,
                            manual: false,
                        },
                        db::models::AccessLevel::Owner,
                        payload,
                    )
                    .await
                {
                    Ok(_) | Err(AdmissionError::WorkflowDisabled) => {}
                    // One refused workflow must not stop the other workflows due at this minute.
                    Err(error) => {
                        tracing::warn!(repo_id = repo.id, workflow = %definition.path, %error, "scheduled workflow was not queued");
                    }
                }
            }
        }
        Ok(())
    }
}

pub(crate) async fn run(ci: std::sync::Arc<CiRuns>, mut stop: tokio::sync::watch::Receiver<bool>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            _ = interval.tick() => {},
        }
        if let Err(error) = ci.queue_schedules(Utc::now()).await {
            tracing::warn!(%error, "workflow schedule discovery failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schedules_use_posix_utc_and_at_least_five_minutes() {
        for expression in [
            "*/5 * * * *",
            "0 4 * * MON-FRI",
            "0,30 4 1 JAN MON",
            "0 0 * * 7",
        ] {
            assert!(parse(expression).is_ok(), "{expression}");
        }
        for expression in [
            "* * * * *",
            "1,2 * * * *",
            "59,0 * * * *",
            "0 0 0 * * *",
            "@daily",
            "0 0 L * *",
            "0 0 * * MON#2",
        ] {
            assert!(parse(expression).is_err(), "{expression}");
        }
        let cron = parse("0 4 1 JAN MON").unwrap();
        // POSIX matches either restricted day field, not their intersection.
        assert!(cron
            .is_time_matching(&Utc.with_ymd_and_hms(2026, 1, 1, 4, 0, 0).unwrap())
            .unwrap());
        assert!(cron
            .is_time_matching(&Utc.with_ymd_and_hms(2026, 1, 5, 4, 0, 0).unwrap())
            .unwrap());
    }
}
