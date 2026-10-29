mod projects;
use super::super::{auth, comments, issues, milestones, pulls, releases, repos, Error, Result};
use crate::backend::{
    app::AppState,
    db::{
        self,
        assignees::AssignedItem,
        repository_exports::{self as exports, RepositoryExport},
    },
    repositories::{
        migration_archive::MigrationArchive, releases::ReleaseDetail, store::repo_store,
    },
    storage::Upload,
    webhooks::payloads,
};
use axum::http::HeaderMap;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};

pub(super) fn spawn(state: Arc<AppState>, headers: HeaderMap, row: RepositoryExport) -> bool {
    let tasks = state.git_tasks.clone();
    tasks.spawn(async move {
        let result = tokio::time::timeout(Duration::from_secs(900), async {
            let _permit = state.repository_downloads.clone().acquire_owned().await.map_err(Error::unavailable)?;
            exports::finish(&state.pool, row.id, "exporting").await.map_err(Error::unavailable)?;
            let bytes = build(&state, &headers, &row).await?;
            // Tokens, scope and selected repository ownership are rechecked before publication.
            let actor = auth::authenticate(&state, &headers, true).await?.ok_or_else(auth::required)?;
            actor.require_write()?;
            if Some(actor.user.id) != row.actor_id || exports::by_guid(&state.pool, actor.user.id, &row.guid).await.map_err(Error::unavailable)?.is_none() { return Err(Error::missing()); }
            let key = row.object_key();
            let digest = format!("{:x}", Sha256::digest(&bytes));
            state.storage.put_bytes(&Upload { key: &key, bytes: bytes.len() as i64, content_type: "application/gzip", sha256_hex: &digest }, bytes).await.map_err(Error::unavailable)?;
            exports::finish(&state.pool, row.id, "exported").await.map_err(Error::unavailable)
        }).await;
        if !matches!(result, Ok(Ok(()))) {
            tracing::warn!(migration_id=row.id, "migration export failed");
            if let Err(error) = exports::finish(&state.pool, row.id, "failed").await { tracing::error!(migration_id=row.id, error=%error, "record migration export failure"); }
        }
    })
}

async fn person(state: &AppState, id: i64) -> Result<Value> {
    let user = db::users::accounts::get_user_by_id(&state.pool, id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    Ok(payloads::user(&user))
}

async fn build(state: &AppState, headers: &HeaderMap, row: &RepositoryExport) -> Result<Vec<u8>> {
    let actor = auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    if exports::by_guid(&state.pool, actor.user.id, &row.guid)
        .await
        .map_err(Error::unavailable)?
        .is_none()
    {
        return Err(Error::missing());
    }
    let options = row.options().map_err(Error::unavailable)?;
    let ids = row.repository_ids().map_err(Error::unavailable)?;
    let mut archive = MigrationArchive::default();
    if let Some(org) = row.owner_org_id {
        if !options.exclude_metadata {
            organization(state, actor.user.id, org, &mut archive).await?;
        }
    }
    if !options.exclude_metadata && !options.exclude_owner_projects {
        projects::append(state, actor.user.id, row, &mut archive).await?;
    }
    for id in ids {
        let repo = db::repositories::records::get_repository_by_id(&state.pool, id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        let owner = db::repositories::records::repository_owner_name(&state.pool, id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        let store = repo_store(&repo, state.cluster.as_ref());
        if options.lock_repositories {
            store
                .lock_migration_as(&state.pool, repo.id, actor.user.id, row.id)
                .await
                .map_err(Error::unavailable)?;
        }
        let value = repos::document(state, &repo, Some(&actor)).await?;
        archive
            .records("repository", vec![value])
            .map_err(Error::unavailable)?;
        if !options.exclude_git_data {
            let bytes = store
                .migration_archive_as(&state.pool, repo.id, actor.user.id)
                .await
                .map_err(Error::unavailable)?;
            let name = format!("{owner}/{}", repo.name);
            archive = tokio::task::spawn_blocking(move || {
                archive.repository(&name, &bytes)?;
                anyhow::Ok(archive)
            })
            .await
            .map_err(Error::unavailable)?
            .map_err(Error::unavailable)?;
        }
        if !options.exclude_metadata {
            metadata(state, headers, &repo, &owner, &mut archive).await?;
        }
        if !options.exclude_metadata && !options.exclude_releases {
            release_records(
                state,
                &repo,
                &owner,
                !options.exclude_attachments,
                &mut archive,
            )
            .await?;
        }
    }
    tokio::task::spawn_blocking(move || archive.finish())
        .await
        .map_err(Error::unavailable)?
        .map_err(Error::unavailable)
}

async fn metadata(
    state: &AppState,
    headers: &HeaderMap,
    repo: &db::models::Repository,
    owner: &str,
    archive: &mut MigrationArchive,
) -> Result<()> {
    let repository = payloads::repository(repo, owner)["html_url"].clone();
    let labels = db::labels::list_repo_labels(&state.pool, repo.id)
        .await
        .map_err(Error::unavailable)?;
    let values = labels
        .iter()
        .map(|label| {
            let mut value = payloads::label(label, owner, &repo.name);
            value["repository"] = repository.clone();
            value
        })
        .collect();
    archive
        .records("label", values)
        .map_err(Error::unavailable)?;
    let mut offset = 0;
    loop {
        let rows =
            db::milestones::list_milestones_page(&state.pool, repo.id, None, "due_on", 100, offset)
                .await
                .map_err(Error::unavailable)?;
        let count = rows.len();
        let mut values = Vec::new();
        for row in rows {
            let mut value = milestones::document(&state.pool, &row, owner, &repo.name).await?;
            value["repository"] = repository.clone();
            values.push(value);
        }
        archive
            .records("milestone", values)
            .map_err(Error::unavailable)?;
        if count < 100 {
            break;
        }
        offset += 100;
    }
    let mut offset = 0;
    loop {
        let filter = db::discussions::ListFilter {
            offset,
            ..Default::default()
        };
        let rows = db::issues::list_repo_issues(&state.pool, repo.id, &filter, 100)
            .await
            .map_err(Error::unavailable)?;
        let count = rows.len();
        let mut values = Vec::new();
        for row in rows {
            let mut value =
                issues::document(&state.pool, repo, owner, AssignedItem::Issue(row.issue.id))
                    .await?;
            value["repository"] = repository.clone();
            values.push(value);
            issue_comments(state, repo, owner, row.issue.id, archive).await?;
        }
        archive
            .records("issue", values)
            .map_err(Error::unavailable)?;
        if count < 100 {
            break;
        }
        offset += 100;
    }
    let mut offset = 0;
    loop {
        let filter = db::discussions::ListFilter {
            offset,
            ..Default::default()
        };
        let rows =
            db::pull_requests::records::list_repo_pull_requests(&state.pool, repo.id, &filter, 100)
                .await
                .map_err(Error::unavailable)?;
        let count = rows.len();
        let mut values = Vec::new();
        for row in rows {
            let pull = row.pull_request;
            let mut value = pulls::document(state, headers, repo, owner, &pull, false).await?;
            value["repository"] = repository.clone();
            values.push(value);
            pull_comments(state, repo, owner, &pull, archive).await?;
            let mut offset = 0;
            loop {
                let rows = db::pull_requests::reviews::list_pull_request_reviews(
                    &state.pool,
                    pull.id,
                    100,
                    offset,
                )
                .await
                .map_err(Error::unavailable)?;
                let count = rows.len();
                let mut reviews = Vec::new();
                for row in rows {
                    if row.review.state == "pending" {
                        continue;
                    }
                    let author =
                        db::users::accounts::get_user_by_id(&state.pool, row.review.author_id)
                            .await
                            .map_err(Error::unavailable)?
                            .ok_or_else(Error::missing)?;
                    let mut value = payloads::pull_request_review_json(
                        &row.review,
                        &payloads::Person::of(&author),
                        &pull,
                        repo,
                        owner,
                    );
                    value["user"] = payloads::user(&author);
                    value["pull_request"] = json!(format!(
                        "{}/pull/{}",
                        payloads::repository(repo, owner)["html_url"]
                            .as_str()
                            .unwrap_or_default(),
                        pull.number
                    ));
                    reviews.push(value);
                }
                archive
                    .records("pull_request_review", reviews)
                    .map_err(Error::unavailable)?;
                if count < 100 {
                    break;
                }
                offset += 100;
            }
        }
        archive
            .records("pull_request", values)
            .map_err(Error::unavailable)?;
        if count < 100 {
            break;
        }
        offset += 100;
    }
    Ok(())
}

async fn issue_comments(
    state: &AppState,
    repo: &db::models::Repository,
    owner: &str,
    id: i64,
    archive: &mut MigrationArchive,
) -> Result<()> {
    let mut offset = 0;
    loop {
        let rows = db::issues::list_issue_comments(&state.pool, id, 100, offset)
            .await
            .map_err(Error::unavailable)?;
        let count = rows.len();
        let mut values = Vec::new();
        for row in rows {
            let comment = comments::find(
                &state.pool,
                repo.id,
                payloads::conversation_comment_id(row.comment.id, false),
            )
            .await?;
            values.push(comments::document(&state.pool, repo, owner, &comment).await?);
        }
        archive
            .records("issue_comment", values)
            .map_err(Error::unavailable)?;
        if count < 100 {
            break;
        }
        offset += 100;
    }
    Ok(())
}

async fn pull_comments(
    state: &AppState,
    repo: &db::models::Repository,
    owner: &str,
    pull: &db::models::PullRequest,
    archive: &mut MigrationArchive,
) -> Result<()> {
    let mut offset = 0;
    loop {
        let rows = db::pull_requests::reviews::list_pull_request_comments(
            &state.pool,
            pull.id,
            100,
            offset,
        )
        .await
        .map_err(Error::unavailable)?;
        let count = rows.len();
        let mut conversation = Vec::new();
        let mut inline = Vec::new();
        for row in rows {
            if row.comment.path.is_some() || row.comment.review_id.is_some() {
                let author =
                    db::users::accounts::get_user_by_id(&state.pool, row.comment.author_id)
                        .await
                        .map_err(Error::unavailable)?
                        .ok_or_else(Error::missing)?;
                let mut value = payloads::pull_request_review_comment_json(
                    &row.comment,
                    pull,
                    &payloads::Person::of(&author),
                    repo,
                    owner,
                );
                value["user"] = payloads::user(&author);
                inline.push(value);
            } else {
                let comment = comments::find(
                    &state.pool,
                    repo.id,
                    payloads::conversation_comment_id(row.comment.id, true),
                )
                .await?;
                conversation.push(comments::document(&state.pool, repo, owner, &comment).await?);
            }
        }
        archive
            .records("issue_comment", conversation)
            .map_err(Error::unavailable)?;
        archive
            .records("pull_request_review_comment", inline)
            .map_err(Error::unavailable)?;
        if count < 100 {
            break;
        }
        offset += 100;
    }
    Ok(())
}

async fn release_records(
    state: &AppState,
    repo: &db::models::Repository,
    owner: &str,
    attachments: bool,
    archive: &mut MigrationArchive,
) -> Result<()> {
    let mut offset = 0;
    loop {
        let rows = db::releases::list_releases(&state.pool, repo.id, true, 100, offset)
            .await
            .map_err(Error::unavailable)?;
        let count = rows.len();
        let mut values = Vec::new();
        for row in rows {
            let assets = db::releases::list_release_assets(&state.pool, row.release.id)
                .await
                .map_err(Error::unavailable)?;
            let detail = ReleaseDetail {
                repository: repo.clone(),
                can_write: true,
                release: row,
                assets,
            };
            let mut value = releases::document(state, owner, &repo.name, &detail).await?;
            value["repository"] = payloads::repository(repo, owner)["html_url"].clone();
            let mut asset_values = Vec::new();
            if attachments {
                for asset in &detail.assets {
                    let file = db::files::get_file_by_id(&state.pool, asset.file_id)
                        .await
                        .map_err(Error::unavailable)?
                        .filter(|file| file.is_active)
                        .ok_or_else(Error::missing)?;
                    let path = format!("attachments/release-assets/{}", asset.id);
                    let mut bytes = Vec::new();
                    let size = usize::try_from(file.file_size).map_err(Error::unavailable)?;
                    if size > crate::backend::storage::MAX_OBJECT_BYTES as usize {
                        return Err(Error::unavailable(()));
                    }
                    while bytes.len() < size {
                        let chunk = state
                            .storage
                            .read_range(
                                &file.object_key,
                                bytes.len() as u64,
                                (size - bytes.len()).min(1024 * 1024),
                            )
                            .await
                            .map_err(Error::unavailable)?;
                        bytes.extend_from_slice(&chunk);
                    }
                    let mut pending = std::mem::take(archive);
                    let archive_path = path.clone();
                    *archive = tokio::task::spawn_blocking(move || {
                        pending.append(&archive_path, &bytes)?;
                        anyhow::Ok(pending)
                    }).await.map_err(Error::unavailable)?.map_err(Error::unavailable)?;
                    let mut asset_value = releases::asset_document(
                        state,
                        owner,
                        &repo.name,
                        &detail.release.release,
                        asset,
                    )
                    .await?;
                    asset_value["archive_path"] = json!(path);
                    asset_value["release"] = value["html_url"].clone();
                    asset_values.push(asset_value);
                }
            }
            archive
                .records("release_asset", asset_values)
                .map_err(Error::unavailable)?;
            values.push(value);
        }
        archive
            .records("release", values)
            .map_err(Error::unavailable)?;
        if count < 100 {
            break;
        }
        offset += 100;
    }
    Ok(())
}

async fn organization(
    state: &AppState,
    actor: i64,
    org: i64,
    archive: &mut MigrationArchive,
) -> Result<()> {
    use db::organizations::{OrganizationOutcome, OrganizationPage};
    let record = db::organizations::get_public_organization_by_id(&state.pool, org)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let origin = crate::backend::api::http::config::link_origin();
    let url = format!("{origin}/{}", crate::webui::encode_component(&record.name));
    archive.person(json!({"id":record.id,"login":record.name,"html_url":url,"url":url,"type":"Organization","name":record.display_name,"description":record.description,"created_at":record.created_at,"updated_at":record.updated_at}));
    let mut after = 0;
    loop {
        let page = OrganizationPage::new(after, 100).map_err(Error::unavailable)?;
        let OrganizationOutcome::Applied(rows) =
            db::organizations::list_organization_members(&state.pool, actor, org, page)
                .await
                .map_err(Error::unavailable)?
        else {
            return Err(Error::missing());
        };
        let count = rows.len();
        let mut values = Vec::new();
        for row in rows {
            after = row.id;
            values.push(json!({"id":row.id,"organization":url,"user":person(state,row.user_id).await?,"role":row.role,"created_at":row.created_at}));
        }
        archive
            .records("organization_membership", values)
            .map_err(Error::unavailable)?;
        if count < 100 {
            break;
        }
    }
    teams(state, actor, org, &record.name, archive).await?;
    Ok(())
}

async fn teams(
    state: &AppState,
    actor: i64,
    org: i64,
    slug: &str,
    archive: &mut MigrationArchive,
) -> Result<()> {
    use db::organizations::{OrganizationOutcome, OrganizationPage};
    let mut after = 0;
    loop {
        let page = OrganizationPage::new(after, 100).map_err(Error::unavailable)?;
        let OrganizationOutcome::Applied(rows) =
            db::organizations::list_organization_teams(&state.pool, actor, org, page)
                .await
                .map_err(Error::unavailable)?
        else {
            return Err(Error::missing());
        };
        let count = rows.len();
        let mut values = Vec::new();
        for row in rows {
            after = row.team.id;
            let mut value = payloads::team(&db::repositories::push_teams::TeamAccess {
                team: row.team.clone(),
                permission: 0,
                org_slug: slug.to_owned(),
            });
            value["organization"] = json!(format!(
                "{}/{}",
                crate::backend::api::http::config::link_origin(),
                crate::webui::encode_component(slug)
            ));
            value["parent_team_id"] = json!(row.team.parent_team_id);
            value["default_permission"] = json!(row.team.default_permission);
            value["notifications_enabled"] = json!(row.team.notifications_enabled);
            value["created_at"] = json!(row.team.created_at);
            value["updated_at"] = json!(row.team.updated_at);
            let team_url = value["html_url"].clone();
            values.push(value);
            let mut member_after = 0;
            loop {
                let page = OrganizationPage::new(member_after, 100).map_err(Error::unavailable)?;
                let OrganizationOutcome::Applied(members) =
                    db::organizations::list_direct_organization_team_members(
                        &state.pool,
                        actor,
                        org,
                        row.team.id,
                        page,
                    )
                    .await
                    .map_err(Error::unavailable)?
                else {
                    return Err(Error::missing());
                };
                let count = members.len();
                let mut memberships = Vec::new();
                for member in members {
                    member_after = member.id;
                    memberships.push(json!({"id":member.id,"team":team_url,"user":person(state,member.user_id).await?,"role":member.role,"created_at":member.created_at}));
                }
                archive
                    .records("team_membership", memberships)
                    .map_err(Error::unavailable)?;
                if count < 100 {
                    break;
                }
            }
            let mut repository_after = 0;
            loop {
                let page =
                    OrganizationPage::new(repository_after, 100).map_err(Error::unavailable)?;
                let OrganizationOutcome::Applied(repositories) =
                    db::organizations::list_direct_organization_team_repositories(
                        &state.pool,
                        actor,
                        org,
                        row.team.id,
                        page,
                    )
                    .await
                    .map_err(Error::unavailable)?
                else {
                    return Err(Error::missing());
                };
                let count = repositories.len();
                let mut grants = Vec::new();
                for repository in repositories {
                    repository_after = repository.id;
                    grants.push(json!({"id":repository.id,"team":team_url,"repository":format!("{}/{}/{}",crate::backend::api::http::config::link_origin(),crate::webui::encode_component(&repository.owner),crate::webui::encode_component(&repository.name)),"permission":repository.permission}));
                }
                archive
                    .records("team_repository", grants)
                    .map_err(Error::unavailable)?;
                if count < 100 {
                    break;
                }
            }
        }
        archive
            .records("team", values)
            .map_err(Error::unavailable)?;
        if count < 100 {
            break;
        }
    }
    Ok(())
}
