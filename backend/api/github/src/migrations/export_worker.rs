use axum::http::HeaderMap;
use codeyard_app::app::AppState;
use codeyard_repos::migration_archive::MigrationArchive;
use codeyard_app::repositories::releases::ReleaseDetail;
use codeyard_app::repositories::store::repo_store;
use codeyard_storage::Upload;
use codeyard_db_repos::assignees::AssignedItem;
use codeyard_db_repos::repository_exports as exports;
use codeyard_db_repos::repository_exports::RepositoryExport;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde_json::json;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use std::sync::Arc;
use std::time::Duration;
mod projects;

pub(super) fn spawn(state: Arc<AppState>, headers: HeaderMap, row: RepositoryExport) -> bool {
    let tasks = state.git_tasks.clone();
    tasks.spawn(async move {
        let result = tokio::time::timeout(Duration::from_secs(900), async {
            let _permit = state.repository_downloads.clone().acquire_owned().await.map_err(Error::unavailable)?;
            exports::finish(&state.pool, row.id, "exporting").await.map_err(Error::unavailable)?;
            let bytes = build(&state, &headers, &row).await?;
            // Tokens, scope and selected repository ownership are rechecked before publication.
            let actor = codeyard_app::api_shared::github::auth::authenticate(&state, &headers, true).await?.ok_or_else(codeyard_app::api_shared::github::auth::required)?;
            actor.require_write()?;
            if Some(actor.user.id) != row.actor_id || exports::by_guid(&state.pool, actor.user.id, &row.guid).await.map_err(Error::unavailable)?.is_none() { return Err(Error::missing()); }
            let key = row.object_key();
            let digest = format!("{:x}", Sha256::digest(&bytes));
            let io = Arc::new(state.storage.try_acquire_io().map_err(Error::unavailable)?);
            state.storage.put_bytes(&Upload { key: &key, bytes: bytes.len() as i64, content_type: "application/gzip", sha256_hex: &digest }, bytes, io).await.map_err(Error::unavailable)?;
            exports::finish(&state.pool, row.id, "exported").await.map_err(Error::unavailable)
        }).await;
        if !matches!(result, Ok(Ok(()))) {
            tracing::warn!(migration_id=row.id, "migration export failed");
            if let Err(error) = exports::finish(&state.pool, row.id, "failed").await { tracing::error!(migration_id=row.id, error=%error, "record migration export failure"); }
        }
    })
}

async fn person(state: &AppState, id: i64) -> Result<Value> {
    let user = codeyard_db_identity::users::accounts::get_user_by_id(&state.pool, id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    Ok(codeyard_app::webhooks::payloads::user(&user))
}

async fn build(state: &AppState, headers: &HeaderMap, row: &RepositoryExport) -> Result<Vec<u8>> {
    let actor = codeyard_app::api_shared::github::auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    actor.require_write()?;
    if exports::by_guid(&state.pool, actor.user.id, &row.guid)
        .await
        .map_err(Error::unavailable)?
        .is_none()
    {
        return Err(Error::missing());
    }
    let mut options = row.options().map_err(Error::unavailable)?;
    if options.org_metadata_only {
        options.exclude_metadata = false;
        options.exclude_owner_projects = false;
    }
    let ids = row.repository_ids().map_err(Error::unavailable)?;
    let mut archive = MigrationArchive::default();
    if let Some(org) = row.owner_org_id {
        if !options.exclude_metadata {
            organization(state, actor.user.id, org, &mut archive).await?;
        }
    }
    if !options.exclude_metadata && !options.exclude_owner_projects {
        crate::migrations::export_worker::projects::append(state, actor.user.id, row, &mut archive)
            .await?;
    }
    for id in ids {
        let repo = codeyard_db_repos::repositories::records::get_repository_by_id(&state.pool, id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        let owner =
            codeyard_db_repos::repositories::records::repository_owner_name(&state.pool, id)
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
        let value =
            codeyard_app::api_shared::github::repos::document(state, &repo, Some(&actor)).await?;
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
            metadata(state, headers, actor.user.id, &repo, &owner, &mut archive).await?;
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
    actor: i64,
    repo: &codeyard_db_core::models::Repository,
    owner: &str,
    archive: &mut MigrationArchive,
) -> Result<()> {
    let repository = codeyard_app::webhooks::payloads::repository(repo, owner)["html_url"].clone();
    for (kind, record) in [("type", "issue_type"), ("field", "issue_field")] {
        let definitions =
            codeyard_db_repos::issue_attributes::catalog(&state.pool, repo.id, Some(actor), kind)
                .await
                .map_err(Error::unavailable)?;
        let mut values = Vec::new();
        for definition in definitions {
            let mut value = serde_json::to_value(definition).map_err(Error::unavailable)?;
            value["repository"] = repository.clone();
            values.push(value);
        }
        archive
            .records(record, values)
            .map_err(Error::unavailable)?;
    }
    let mut offset = 0;
    loop {
        let labels = codeyard_db_repos::labels::list_repo_labels_page(
            &state.pool,
            repo.id,
            "",
            "oldest",
            100,
            offset,
        )
        .await
        .map_err(Error::unavailable)?;
        let count = labels.len();
        let values = labels
            .iter()
            .map(|label| {
                let mut value = codeyard_app::webhooks::payloads::label(label, owner, &repo.name);
                value["repository"] = repository.clone();
                value
            })
            .collect();
        archive
            .records("label", values)
            .map_err(Error::unavailable)?;
        if count < 100 {
            break;
        }
        offset += 100;
    }
    let mut offset = 0;
    loop {
        let rows = codeyard_db_repos::milestones::list_milestones_page(
            &state.pool,
            repo.id,
            None,
            "due_on",
            100,
            offset,
        )
        .await
        .map_err(Error::unavailable)?;
        let count = rows.len();
        let mut values = Vec::new();
        for row in rows {
            let mut value =
                crate::issues::milestones::document(&state.pool, &row, owner, &repo.name).await?;
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
    let mut before = i64::MAX;
    loop {
        let rows = exports::discussion_ids(&state.pool, repo.id, false, before, 100)
            .await
            .map_err(Error::unavailable)?;
        let count = rows.len();
        let next = rows.last().copied();
        let mut values = Vec::new();
        for id in rows {
            let mut value = codeyard_app::api_shared::github::issues::document(
                &state.pool,
                codeyard_app::api_shared::github::issues::Reader::Repository,
                repo,
                owner,
                AssignedItem::Issue(id),
            )
            .await?;
            value["issue_type"] = value["type"].take();
            value["issue_field_values"] = serde_json::to_value(
                codeyard_db_repos::issue_attributes::values(&state.pool, repo.id, id, Some(actor))
                    .await
                    .map_err(Error::unavailable)?,
            )
            .map_err(Error::unavailable)?;
            value["repository"] = repository.clone();
            values.push(value);
            issue_comments(state, repo, owner, id, archive).await?;
        }
        archive
            .records("issue", values)
            .map_err(Error::unavailable)?;
        if count < 100 {
            break;
        }
        before = next.ok_or_else(|| Error::unavailable(()))?;
    }
    let mut before = i64::MAX;
    loop {
        let rows = exports::discussion_ids(&state.pool, repo.id, true, before, 100)
            .await
            .map_err(Error::unavailable)?;
        let count = rows.len();
        let next = rows.last().copied();
        let mut values = Vec::new();
        for id in rows {
            let pull =
                codeyard_db_repos::pull_requests::records::get_pull_request_by_id(&state.pool, id)
                    .await
                    .map_err(Error::unavailable)?
                    .ok_or_else(Error::missing)?;
            let mut value =
                crate::pulls::document(state, headers, repo, owner, &pull, false).await?;
            value["repository"] = repository.clone();
            values.push(value);
            pull_comments(state, repo, owner, &pull, archive).await?;
            let mut offset = 0;
            loop {
                let rows = codeyard_db_repos::pull_requests::reviews::list_pull_request_reviews(
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
                    let author = codeyard_db_identity::users::accounts::get_user_by_id(
                        &state.pool,
                        row.review.author_id,
                    )
                    .await
                    .map_err(Error::unavailable)?
                    .ok_or_else(Error::missing)?;
                    let mut value = codeyard_app::webhooks::payloads::pull_request_review_json(
                        &row.review,
                        &codeyard_app::webhooks::payloads::Person::of(&author),
                        &pull,
                        repo,
                        owner,
                    );
                    value["user"] = codeyard_app::webhooks::payloads::user(&author);
                    value["pull_request"] = json!(format!(
                        "{}/pull/{}",
                        codeyard_app::webhooks::payloads::repository(repo, owner)["html_url"]
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
        before = next.ok_or_else(|| Error::unavailable(()))?;
    }
    Ok(())
}

async fn issue_comments(
    state: &AppState,
    repo: &codeyard_db_core::models::Repository,
    owner: &str,
    id: i64,
    archive: &mut MigrationArchive,
) -> Result<()> {
    let mut offset = 0;
    loop {
        let rows = codeyard_db_repos::issues::list_issue_comments(&state.pool, id, 100, offset)
            .await
            .map_err(Error::unavailable)?;
        let count = rows.len();
        let mut values = Vec::new();
        for row in rows {
            let comment = codeyard_app::api_shared::github::comments::find(
                &state.pool,
                repo.id,
                codeyard_model::webhook_ids::conversation_comment_id(row.comment.id, false),
            )
            .await?;
            values.push(
                codeyard_app::api_shared::github::comments::document(
                    &state.pool,
                    repo,
                    owner,
                    &comment,
                )
                .await?,
            );
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
    repo: &codeyard_db_core::models::Repository,
    owner: &str,
    pull: &codeyard_db_core::models::PullRequest,
    archive: &mut MigrationArchive,
) -> Result<()> {
    let mut offset = 0;
    loop {
        let rows = codeyard_db_repos::pull_requests::reviews::list_pull_request_comments(
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
                let author = codeyard_db_identity::users::accounts::get_user_by_id(
                    &state.pool,
                    row.comment.author_id,
                )
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
                let mut value = codeyard_app::webhooks::payloads::pull_request_review_comment_json(
                    &row.comment,
                    pull,
                    &codeyard_app::webhooks::payloads::Person::of(&author),
                    repo,
                    owner,
                );
                value["user"] = codeyard_app::webhooks::payloads::user(&author);
                inline.push(value);
            } else {
                let comment = codeyard_app::api_shared::github::comments::find(
                    &state.pool,
                    repo.id,
                    codeyard_model::webhook_ids::conversation_comment_id(row.comment.id, true),
                )
                .await?;
                conversation.push(
                    codeyard_app::api_shared::github::comments::document(
                        &state.pool,
                        repo,
                        owner,
                        &comment,
                    )
                    .await?,
                );
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
    repo: &codeyard_db_core::models::Repository,
    owner: &str,
    attachments: bool,
    archive: &mut MigrationArchive,
) -> Result<()> {
    let mut offset = 0;
    loop {
        let rows =
            codeyard_db_repos::releases::list_releases(&state.pool, repo.id, true, 100, offset)
                .await
                .map_err(Error::unavailable)?;
        let count = rows.len();
        let mut values = Vec::new();
        for row in rows {
            let assets =
                codeyard_db_repos::releases::list_release_assets(&state.pool, row.release.id)
                    .await
                    .map_err(Error::unavailable)?;
            let detail = ReleaseDetail {
                repository: repo.clone(),
                can_write: true,
                release: row,
                assets,
            };
            let mut value =
                crate::repos::releases::document(state, owner, &repo.name, &detail).await?;
            value["repository"] =
                codeyard_app::webhooks::payloads::repository(repo, owner)["html_url"].clone();
            let mut asset_values = Vec::new();
            if attachments {
                for asset in &detail.assets {
                    let file = codeyard_db_repos::files::get_file_by_id(&state.pool, asset.file_id)
                        .await
                        .map_err(Error::unavailable)?
                        .filter(|file| file.is_active)
                        .ok_or_else(Error::missing)?;
                    let path = format!("attachments/release-assets/{}", asset.id);
                    let mut bytes = Vec::new();
                    let size = usize::try_from(file.file_size).map_err(Error::unavailable)?;
                    if size > codeyard_support::object_limits::MAX_OBJECT_BYTES as usize {
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
                    })
                    .await
                    .map_err(Error::unavailable)?
                    .map_err(Error::unavailable)?;
                    let mut asset_value = crate::repos::releases::asset_document(
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
    use codeyard_db_core::queries::organizations::OrganizationOutcome;
    use codeyard_db_orgs::organizations::OrganizationPage;
    let record =
        codeyard_db_orgs::organizations::profiles::get_public_organization_by_id(&state.pool, org)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
    let origin = codeyard_model::http_config::link_origin();
    let url = format!("{origin}{}", codeyard_webui::profile_path(&record.name));
    archive.person(json!({"id":record.id,"login":record.name,"html_url":url,"url":url,"type":"Organization","name":record.display_name,"description":record.description,"created_at":record.created_at,"updated_at":record.updated_at}));
    let mut after = 0;
    loop {
        let page = OrganizationPage::new(after, 100).map_err(Error::unavailable)?;
        let OrganizationOutcome::Applied(rows) =
            codeyard_db_orgs::organizations::membership::list_organization_members(
                &state.pool,
                actor,
                org,
                page,
            )
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
    use codeyard_db_core::queries::organizations::OrganizationOutcome;
    use codeyard_db_orgs::organizations::OrganizationPage;
    let mut after = 0;
    loop {
        let page = OrganizationPage::new(after, 100).map_err(Error::unavailable)?;
        let OrganizationOutcome::Applied(rows) =
            codeyard_db_orgs::organizations::teams::list_organization_teams(
                &state.pool,
                actor,
                org,
                page,
            )
            .await
            .map_err(Error::unavailable)?
        else {
            return Err(Error::missing());
        };
        let count = rows.len();
        let mut values = Vec::new();
        for row in rows {
            after = row.team.id;
            let mut value = codeyard_app::webhooks::payloads::team(
                &codeyard_db_repos::repositories::push_teams::TeamAccess {
                    team: row.team.clone(),
                    permission: 0,
                    org_slug: slug.to_owned(),
                },
            );
            value["organization"] = json!(format!(
                "{}{}",
                codeyard_model::http_config::link_origin(),
                codeyard_webui::profile_path(slug)
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
                    codeyard_db_orgs::organizations::teams::list_direct_organization_team_members(
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
                    codeyard_db_orgs::organizations::teams::list_direct_organization_team_repositories(
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
                    grants.push(json!({"id":repository.id,"team":team_url,"repository":format!("{}/{}/{}",codeyard_model::http_config::link_origin(),codeyard_webui::encode_component(&repository.owner),codeyard_webui::encode_component(&repository.name)),"permission":repository.permission}));
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
