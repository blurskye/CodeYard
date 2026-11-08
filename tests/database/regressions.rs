//! Regression contracts for database effects and their current authority on both engines.
use crate::support::database::Database;

use codeyard_database::models::AccessLevel;
use codeyard_database::models::Repository;
use codeyard_database::queries::notifications::records::NewNotification;
use codeyard_db_repos::notifications::records::NotificationFilter;
use codeyard_db_repos::notifications::records::NotificationInboxAction;
use codeyard_db_repos::repositories::creation::RepositoryTarget;
use codeyard_db_repos::repositories::records;
use codeyard_identity as identity;
use codeyard_notifications as notifications;
use std::time::Duration;

async fn repo(f: &Database, owner: i64, name: &str) -> Repository {
    records::create_repository(
        f.pool(),
        owner,
        name,
        None,
        true,
        &uuid::Uuid::new_v4().to_string(),
    )
    .await
    .unwrap()
}

async fn organization(f: &Database, id: i64, owner: i64) {
    f.execute(
        &format!("INSERT INTO organizations(id,name) VALUES($1,'org{id}')"),
        &[id],
    )
    .await;
    f.execute(
        "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'owner')",
        &[id, owner],
    )
    .await;
}

async fn org_repo(f: &Database, owner: i64, org: i64, name: &str) -> Repository {
    let r = repo(f, owner, name).await;
    f.execute(
        "UPDATE repositories SET owner_kind='org',owner_user_id=NULL,owner_org_id=$1 WHERE id=$2",
        &[org, r.id],
    )
    .await;
    f.repository(r.id).await
}

async fn notice(
    pool: &codeyard_database::DbPool,
    user: i64,
    repository: Option<i64>,
    title: &str,
) -> codeyard_database::models::Notification {
    codeyard_db_repos::notifications::records::create_notification(
        pool,
        NewNotification {
            user_id: user,
            actor_id: None,
            repo_id: repository,
            kind: "issue_opened",
            title,
            body: Some("payload"),
            link: None,
            metadata: &serde_json::json!({}),
        },
    )
    .await
    .unwrap()
}

async fn session(pool: &codeyard_database::DbPool, user: i64) -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    codeyard_db_identity::users::sessions::create_session(
        pool,
        &id,
        user,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    id
}

#[tokio::test]
async fn archive_writes_recheck_personal_and_organization_authority() {
    for pg in [false, true] {
        let mut f = Database::new(pg).await;
        let owner = f.actor("archive_owner").await;
        let admin = f.actor("archive_admin").await;
        let r = repo(&f, owner, "archive").await;
        codeyard_db_repos::repositories::access::grant_repo_access(
            f.pool(),
            admin,
            r.id,
            AccessLevel::Admin,
        )
        .await
        .unwrap();
        assert!(
            records::set_repository_archived(f.pool(), admin, r.id, true)
                .await
                .unwrap()
        );
        codeyard_db_repos::repositories::access::revoke_repo_access(f.pool(), admin, r.id)
            .await
            .unwrap();
        let error = records::set_repository_archived(f.pool(), admin, r.id, false)
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<records::RepositorySettingsRefusal>(),
            Some(&records::RepositorySettingsRefusal::Denied)
        );
        assert!(f.repository(r.id).await.is_archived);
        assert!(
            records::set_repository_archived(f.pool(), owner, r.id, false)
                .await
                .unwrap()
        );
        organization(&f, 1001, owner).await;
        let r = org_repo(&f, owner, 1001, "org_archive").await;
        f.execute(
            "INSERT INTO org_memberships(org_id,user_id,role) VALUES(1001,$1,'admin')",
            &[admin],
        )
        .await;
        assert!(
            records::set_repository_archived(f.pool(), admin, r.id, true)
                .await
                .unwrap()
        );
        f.execute(
            "UPDATE org_memberships SET role='member' WHERE org_id=1001 AND user_id=$1",
            &[admin],
        )
        .await;
        assert!(
            records::set_repository_archived(f.pool(), admin, r.id, false)
                .await
                .is_err()
        );
        assert!(
            records::set_repository_archived(f.pool(), owner, r.id, false)
                .await
                .unwrap()
        );
        f.finish().await;
    }
}

#[tokio::test]
async fn obsolete_language_refresh_does_not_replace_current_default_statistics() {
    for pg in [false, true] {
        let mut f = Database::new(pg).await;
        let owner = f.actor("language_owner").await;
        let r = repo(&f, owner, "language").await;
        records::set_repository_default_branch(f.pool(), owner, r.id, "topic")
            .await
            .unwrap();
        let python = vec![("Python".to_owned(), 100, 100.0, "#abcabc".to_owned())];
        let mut source = codeyard_db_repos::repository_stats::language_source(f.pool(), r.id)
            .await
            .unwrap()
            .unwrap();
        codeyard_db_repos::repository_stats::upsert_repo_languages(
            f.pool(),
            &source,
            "python",
            &python,
        )
        .await
        .unwrap();
        for stale in [
            vec![],
            vec![("Rust".to_owned(), 200, 100.0, "#123123".to_owned())],
        ] {
            source.default_branch = "main".into();
            codeyard_db_repos::repository_stats::upsert_repo_languages(
                f.pool(),
                &source,
                "rust",
                &stale,
            )
            .await
            .unwrap();
            let rows = codeyard_db_repos::repository_stats::get_repo_languages(f.pool(), r.id)
                .await
                .unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].language, "Python");
        }
        f.finish().await;
    }
}

#[tokio::test]
async fn milestone_titles_use_ascii_case_folding_and_choices_keep_their_fields() {
    for pg in [false, true] {
        let mut f = Database::new(pg).await;
        let owner = f.actor("milestone_owner").await;
        let r = repo(&f, owner, "milestones").await;
        let first = codeyard_db_repos::milestones::create_milestone(
            f.pool(),
            owner,
            r.id,
            "ÉTÉ",
            Some("Summer"),
            Some("2030-07-01"),
        )
        .await
        .unwrap();
        codeyard_db_repos::milestones::create_milestone(f.pool(), owner, r.id, "été", None, None)
            .await
            .unwrap();
        codeyard_db_repos::milestones::create_milestone(f.pool(), owner, r.id, "ASCII", None, None)
            .await
            .unwrap();
        assert!(codeyard_db_repos::milestones::create_milestone(
            f.pool(),
            owner,
            r.id,
            "ascii",
            None,
            None
        )
        .await
        .is_err());
        codeyard_db_repos::milestones::update_milestone(
            f.pool(),
            owner,
            r.id,
            first.number,
            "ÉTÉ",
            Some("Summer"),
            Some("2030-07-01"),
        )
        .await
        .unwrap();
        let issue = codeyard_db_repos::issues::create_issue(f.pool(), r.id, owner, "Progress", "")
            .await
            .unwrap();
        codeyard_db_repos::milestones::set_issue_milestone(
            f.pool(),
            owner,
            r.id,
            issue.id,
            Some(first.number),
        )
        .await
        .unwrap();
        let choices = codeyard_db_repos::milestones::list_milestone_choices(f.pool(), r.id)
            .await
            .unwrap();
        assert_eq!(choices.len(), 3);
        assert_eq!(choices[0].id, first.id);
        // Choices leave out descriptions so a long list stays small.
        assert_eq!(choices[0].description, None);
        assert_eq!(choices[0].due_date.as_deref(), Some("2030-07-01"));
        assert_eq!(
            codeyard_db_repos::milestones::get_milestone(f.pool(), r.id, first.number)
                .await
                .unwrap()
                .unwrap()
                .open_items,
            1
        );
        f.finish().await;
    }
}

#[tokio::test]
async fn shared_selectors_include_scoped_inherited_grants_once() {
    for pg in [false, true] {
        let mut f = Database::new(pg).await;
        let owner = f.actor("selector_owner").await;
        let admin = f.actor("selector_admin").await;
        let writer = f.actor("selector_writer").await;
        let member = f.actor("selector_member").await;
        organization(&f, 1001, owner).await;
        organization(&f, 1002, owner).await;
        for (actor, role) in [(admin, "admin"), (writer, "member"), (member, "member")] {
            f.execute(
                &format!(
                    "INSERT INTO org_memberships(org_id,user_id,role) VALUES(1001,$1,'{role}')"
                ),
                &[actor],
            )
            .await;
        }
        let r = org_repo(&f, owner, 1001, "inherited").await;
        let foreign = org_repo(&f, owner, 1002, "foreign").await;
        f.execute(
            "INSERT INTO teams(id,org_id,name,slug) VALUES(2001,1001,'writers','writers')",
            &[],
        )
        .await;
        f.execute(
            "INSERT INTO team_memberships(team_id,user_id,role) VALUES(2001,$1,'member')",
            &[writer],
        )
        .await;
        f.execute(
            "INSERT INTO team_repo_access(team_id,repo_id,permission) VALUES(2001,$1,3)",
            &[r.id],
        )
        .await;
        // An incorrect cross-tenant grant must not leak the other organization's repository.
        f.execute(
            "INSERT INTO team_repo_access(team_id,repo_id,permission) VALUES(2001,$1,5)",
            &[foreign.id],
        )
        .await;
        codeyard_db_repos::repositories::access::grant_repo_access(
            f.pool(),
            writer,
            r.id,
            AccessLevel::Read,
        )
        .await
        .unwrap();
        for (actor, level) in [
            (owner, AccessLevel::Owner),
            (admin, AccessLevel::Admin),
            (writer, AccessLevel::Write),
        ] {
            let rows = records::list_shared_repositories(f.pool(), actor)
                .await
                .unwrap();
            let matching: Vec<_> = rows
                .iter()
                .filter(|row| row.repository.id == r.id)
                .collect();
            assert_eq!(matching.len(), 1);
            assert_eq!(matching[0].access_level, level as i16);
            if actor == writer {
                assert!(!rows.iter().any(|row| row.repository.id == foreign.id));
            }
        }
        assert!(records::list_shared_repositories(f.pool(), member)
            .await
            .unwrap()
            .is_empty());
        f.execute("UPDATE users SET is_banned=TRUE WHERE id=$1", &[writer])
            .await;
        assert!(records::list_shared_repositories(f.pool(), writer)
            .await
            .unwrap()
            .is_empty());
        f.finish().await;
    }
}

#[tokio::test]
async fn profiles_filter_private_repositories_before_loading() {
    for pg in [false, true] {
        let mut f = Database::new(pg).await;
        let owner = f.actor("profile_owner").await;
        let public = repo(&f, owner, "public").await;
        let private = repo(&f, owner, "private").await;
        f.execute(
            "UPDATE repositories SET is_private=FALSE WHERE id=$1",
            &[public.id],
        )
        .await;
        let (_, anonymous, _) =
            identity::profiles::repositories_on_profile(f.pool(), "profile_owner", None, "", 1)
                .await
                .unwrap();
        assert_eq!(
            anonymous.iter().map(|r| r.id).collect::<Vec<_>>(),
            [public.id]
        );
        let session = uuid::Uuid::new_v4().to_string();
        codeyard_db_identity::users::sessions::create_session(
            f.pool(),
            &session,
            owner,
            None,
            None,
            chrono::Utc::now() + chrono::Duration::days(1),
        )
        .await
        .unwrap();
        let (_, own, _) = identity::profiles::repositories_on_profile(
            f.pool(),
            "profile_owner",
            Some(&session),
            "",
            1,
        )
        .await
        .unwrap();
        assert!(own.iter().any(|r| r.id == private.id));
        assert_eq!(
            records::list_user_repositories(f.pool(), owner)
                .await
                .unwrap()
                .len(),
            2
        );
        f.finish().await;
    }
}

#[tokio::test]
async fn deleting_repositories_retires_only_unreferenced_release_uploads() {
    for pg in [false, true] {
        let mut f = Database::new(pg).await;
        let owner = f.actor("asset_owner").await;
        let file = codeyard_db_repos::files::reserve_upload(
            f.pool(),
            codeyard_db_repos::files::ReserveUpload {
                user_id: owner,
                filename: "asset.zip",
                content_type: "application/zip",
                file_size: 100,
                reason: codeyard_database::models::FileReason::ReleaseAsset,
                visibility: codeyard_database::models::FileVisibility::Owner,
                sha256_hex: &"a".repeat(64),
            },
        )
        .await
        .unwrap();
        codeyard_db_repos::files::verify_pending_upload(f.pool(), file.id, owner)
            .await
            .unwrap();
        let mut ids = Vec::new();
        for name in ["one", "two"] {
            let r = repo(&f, owner, name).await;
            let release = codeyard_db_repos::releases::create_release(
                f.pool(),
                owner,
                r.id,
                "v1",
                "main",
                &codeyard_db_repos::releases::ReleaseFields {
                    name: Some("First".to_owned()),
                    body: Some("".to_owned()),
                    is_draft: false,
                    is_prerelease: false,
                    make_latest: true.into(),
                },
            )
            .await
            .unwrap();
            let added = codeyard_db_repos::releases::add_release_asset(
                f.pool(),
                owner,
                &codeyard_database::queries::users::accounts::AccountCredential::Session(
                    &crate::support::live_session(f.pool(), owner).await,
                ),
                r.id,
                release.id,
                &codeyard_db_repos::releases::NewReleaseAsset {
                    file_id: file.id,
                    name: "asset.zip",
                    label: None,
                    content_type: "application/zip",
                    size: 100,
                },
            )
            .await;
            if ids.is_empty() {
                added.unwrap();
            } else {
                assert!(added.is_err(), "an active upload cannot be attached again");
                // Existing data can share a file even though new attachments may not.
                let sql = "INSERT INTO release_assets(release_id,file_id,name,content_type,size) VALUES($1,$2,'asset.zip','application/zip',100)";
                match f.pool() {
                    codeyard_database::DbPool::Postgres(pool) => {
                        sqlx::query(sqlx::AssertSqlSafe(sql))
                            .bind(release.id)
                            .bind(file.id)
                            .execute(pool)
                            .await
                            .unwrap();
                    }
                    codeyard_database::DbPool::Sqlite(pool) => {
                        sqlx::query(sqlx::AssertSqlSafe(sql))
                            .bind(release.id)
                            .bind(file.id)
                            .execute(pool)
                            .await
                            .unwrap();
                    }
                };
            }
            ids.push(r.id);
        }
        records::delete_repository(f.pool(), owner, ids[0])
            .await
            .unwrap();
        assert!(codeyard_db_repos::files::get_file_by_id(f.pool(), file.id)
            .await
            .unwrap()
            .is_some());
        records::delete_repository(f.pool(), owner, ids[1])
            .await
            .unwrap();
        assert!(codeyard_db_repos::files::get_file_by_id(f.pool(), file.id)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            codeyard_db_repos::files::get_user_total_storage(f.pool(), owner)
                .await
                .unwrap(),
            0
        );
        // Advance only the disposable fixture's cleanup deadline, then verify durable work exists.
        f.execute(
            "UPDATE pending_object_deletions SET delete_after='2000-01-01T00:00:00.000000Z'",
            &[],
        )
        .await;
        assert!(
            codeyard_db_repos::files::due_object_deletions(f.pool(), 100)
                .await
                .unwrap()
                .contains(&file.object_key)
        );
        f.finish().await;
    }
}

#[tokio::test]
async fn transfers_drop_unmanageable_grants_and_stop_old_ci_authority() {
    for pg in [false, true] {
        let mut f = Database::new(pg).await;
        let owner = f.actor("transfer_owner").await;
        let recipient = f.actor("transfer_recipient").await;
        let outsider = f.actor("transfer_outsider").await;
        organization(&f, 1001, owner).await;
        f.execute(
            "INSERT INTO org_memberships(org_id,user_id,role) VALUES(1001,$1,'admin')",
            &[recipient],
        )
        .await;
        let r = repo(&f, owner, "transfer").await;
        codeyard_db_repos::repositories::access::grant_repo_access(
            f.pool(),
            outsider,
            r.id,
            AccessLevel::Admin,
        )
        .await
        .unwrap();
        f.execute(
            "INSERT INTO teams(id,org_id,name,slug) VALUES(2001,1001,'writers','writers')",
            &[],
        )
        .await;
        let team_grant =
            "INSERT INTO team_repo_access(team_id,repo_id,permission) VALUES(2001,$1,3)";
        f.execute(team_grant, &[r.id]).await;
        records::transfer_repository(f.pool(), owner, r.id, RepositoryTarget::Organization(1001))
            .await
            .unwrap();
        // The old team grant is gone: adding it again would break the unique key if it stayed.
        f.execute(team_grant, &[r.id]).await;
        assert!(
            codeyard_db_repos::repositories::access::list_repo_collaborators(f.pool(), r.id)
                .await
                .unwrap()
                .is_empty()
        );
        let mut run_ids = Vec::new();
        for path in [".github/workflows/a.yml", ".github/workflows/b.yml"] {
            let run = codeyard_db_actions::ci::enqueue_ci_run(
                f.pool(),
                codeyard_db_actions::ci::NewCiRun {
                    repo_id: r.id,
                    triggered_by_id: owner,
                    pipeline: "ci",
                    event: "workflow_dispatch",
                    pull_request_id: None,
                    branch: "main",
                    head_sha: &"a".repeat(40),
                    workflow_path: path,
                    workflow_blobs: &serde_json::json!({(path): "b".repeat(40)}),
                    workflow_inputs: None,
                    environment: None,
                    manual: true,
                },
                &[owner],
            )
            .await
            .unwrap();
            run_ids.push(run.run_id);
        }
        // Mark one admitted run as claimed. Transfer must flag it, not only cancel queued work.
        f.execute("UPDATE ci_runs SET status='running',worker='worker' WHERE repo_id=$1 AND workflow_path='.github/workflows/a.yml'", &[r.id]).await;
        records::transfer_repository(
            f.pool(),
            recipient,
            r.id,
            RepositoryTarget::Personal(recipient),
        )
        .await
        .unwrap();
        assert_eq!(
            codeyard_db_repos::repositories::access::get_user_repo_access(f.pool(), owner, r.id)
                .await
                .unwrap(),
            AccessLevel::None
        );
        let running = codeyard_db_actions::ci::get_ci_run_by_run_id(f.pool(), run_ids[0])
            .await
            .unwrap()
            .unwrap();
        let queued = codeyard_db_actions::ci::get_ci_run_by_run_id(f.pool(), run_ids[1])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(running.status, "running");
        assert_eq!(
            codeyard_db_actions::ci::heartbeat_ci_run(f.pool(), running.id, "worker")
                .await
                .unwrap(),
            Some(true)
        );
        assert_eq!(queued.status, "cancelled");
        f.finish().await;
    }
}

#[path = "regressions/assignment_and_search.rs"]
mod assignment_and_search;
#[path = "regressions/contention.rs"]
mod contention;
#[path = "regressions/infra.rs"]
mod infra;
#[path = "regressions/notifications.rs"]
mod notification_tests;
#[path = "regressions/session_and_delivery.rs"]
mod session_and_delivery;
