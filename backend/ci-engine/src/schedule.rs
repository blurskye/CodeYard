//! UTC schedules are discovered on the default branch; enqueue serializes each minute across nodes.
use crate::runs::CiRuns;
use anyhow::Context;
use anyhow::Result;
use chrono::DateTime;
use chrono::Timelike;
use chrono::Utc;
use codeyard_db_core::ci_admission::AdmissionError;
use codeyard_repos_core::receive_ci::CiRunRequest;
use codeyard_repos_core::repositories::store::repo_store;
use codeyard_repos_core::workflow_schedule::parse;
use codeyard_webhooks::payloads;
use serde_json::json;

impl CiRuns {
    /// Runtime and deterministic tests use the same UTC minute admission path.
    pub async fn queue_schedules(&self, now: DateTime<Utc>) -> Result<()> {
        let now = now
            .with_second(0)
            .and_then(|now| now.with_nanosecond(0))
            .context("invalid schedule time")?;
        let mut after = 0;
        loop {
            let ids = codeyard_db_actions::ci::scheduled_repositories(&self.pool, after).await?;
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
        let Some(repo) =
            codeyard_db_repos::repositories::records::get_repository_by_id(&self.pool, id).await?
        else {
            return Ok(());
        };
        // With Actions turned off, nothing would be queued, so Git is not read either.
        if !codeyard_db_actions::action_settings::actions_enabled(&self.pool, repo.id).await? {
            return Ok(());
        }
        let store = repo_store(&repo, self.cluster.as_ref());
        let reference = format!("refs/heads/{}", repo.default_branch);
        let commit = match store.commit_from_primary(&reference).await {
            Ok(commit) => commit,
            Err(error) if error.kind == codeyard_git::error::ErrorKind::NotFound => return Ok(()),
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
            codeyard_git::PublicationActor::User(actor) => {
                (actor, codeyard_db_core::models::AccessLevel::Owner)
            }
            codeyard_git::PublicationActor::DeployKey(key) => {
                let (actor, cap) =
                    codeyard_repos::deploy_keys::authorize(&self.pool, key, &repo, true)
                        .await
                        .map_err(|_| AdmissionError::Denied("CI repository access denied"))?;
                (actor.id, cap)
            }
        };
        if !self.actor_may_discover_workflows(repo.id, actor).await? {
            return Ok(());
        }
        let definitions =
            codeyard_repos_core::workflows::list_workflows_for_ref(&store, &commit.sha).await?;
        let owner = codeyard_db_actions::ci::ci_repository_owner(&self.pool, repo.id)
            .await?
            .ok_or(AdmissionError::Unavailable)?;
        let sender = codeyard_db_identity::users::accounts::get_user_by_id(&self.pool, actor)
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
                let payload = json!({"schedule": expression, "scheduled_minute": now.timestamp().div_euclid(60), "repository": codeyard_webhooks::payloads::repository(&repo, &owner), "sender":payloads::user(&sender)});
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
                            pipeline:
                                codeyard_repos_core::workflows::infer_pipeline_from_workflow_path(
                                    &definition.path,
                                ),
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

pub async fn run(ci: std::sync::Arc<CiRuns>, mut stop: tokio::sync::watch::Receiver<bool>) {
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
