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
    let mut normalized = Vec::new();
    for (index, field) in fields.iter().enumerate() {
        ensure!(
            field.split(',').all(|term| !term.is_empty()),
            "empty cron term"
        );
        let mut field = field.to_ascii_uppercase();
        let names: &[&str] = match index {
            3 => &[
                "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
            ],
            4 => &["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"],
            _ => &[],
        };
        for (number, name) in names.iter().enumerate() {
            field = field.replace(name, &(number + usize::from(index == 3)).to_string());
        }
        ensure!(
            field
                .chars()
                .all(|ch| ch.is_ascii_digit() || "*,-/".contains(ch)),
            "unsupported cron syntax"
        );
        normalized.push(field);
    }
    let cron: Cron = normalized.join(" ").parse()?;
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
        if 60 + first - last < 5 {
            let hours: Cron = format!("0 {} * * *", fields[1]).parse()?;
            let selected: Vec<_> = (0..24)
                .map(|hour| hours.is_time_matching(&(start + chrono::Duration::hours(hour))))
                .collect::<std::result::Result<_, _>>()?;
            ensure!(
                !selected.windows(2).any(|pair| pair[0] && pair[1]),
                "cron interval is less than five minutes"
            );
            if selected[23] && selected[0] {
                let days: Cron =
                    format!("0 0 {} {} {}", normalized[2], normalized[3], normalized[4]).parse()?;
                let mut previous_day = false;
                for day in 0..=146097 {
                    let matches = days.is_time_matching(&(start + chrono::Duration::days(day)))?;
                    ensure!(
                        !(matches && previous_day),
                        "cron interval is less than five minutes"
                    );
                    previous_day = matches;
                }
            }
        }
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
            after = *ids.last().expect("nonempty repository page");
            use futures_util::StreamExt;
            let mut pending = futures_util::stream::iter(ids.into_iter().map(|id| async move {
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    self.queue_repository_schedules(id, now),
                )
                .await;
                if !matches!(result, Ok(Ok(()))) {
                    tracing::warn!(
                        repo_id = id,
                        "scheduled workflow discovery timed out or was refused"
                    );
                }
            }))
            .buffer_unordered(32);
            while pending.next().await.is_some() {}
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
        let commit = match store.commit_from_primary(&reference).await {
            Ok(commit) => commit,
            Err(error)
                if error.kind == crate::backend::repositories::store::ErrorKind::NotFound =>
            {
                return Ok(())
            }
            Err(error) => return Err(error.into()),
        };
        // Schedules run only for a branch tip whose publication is recorded.
        let Some(proof) = store.branch_publication(repo.id, &reference).await? else {
            return Ok(());
        };
        if proof.publication.new != commit.sha {
            return Ok(());
        }
        let (actor, access_cap) = match proof.publication.actor {
            crate::backend::git::PublicationActor::User(actor) => {
                (actor, db::models::AccessLevel::Owner)
            }
            crate::backend::git::PublicationActor::DeployKey(key) => {
                let (actor, cap) = crate::backend::repositories::deploy_keys::authorize(
                    &self.pool, key, &repo, true,
                )
                .await
                .map_err(|_| AdmissionError::Denied("CI repository access denied"))?;
                (actor.id, cap)
            }
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
                        access_cap,
                        payload,
                        None,
                    )
                    .await
                {
                    Ok(_) | Err(AdmissionError::WorkflowDisabled(_)) => {}
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
