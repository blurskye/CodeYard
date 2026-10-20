//! Settings authority contracts on disposable PostgreSQL and SQLite databases:
//! who may update, rename or delete a repository, and what each of those does.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database;
mod fixture;

use codeyard::backend::db::{self, repositories::records::RepositorySettingsRefusal as Refusal};
use database::Database;
use fixture::Fixture;
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
enum Mutation {
    Update,
    Rename,
    Delete,
    MergeMethods,
}
const MUTATIONS: [Mutation; 4] = [
    Mutation::Update,
    Mutation::Rename,
    Mutation::Delete,
    Mutation::MergeMethods,
];
async fn mutate(
    pool: &db::DbPool,
    actor: i64,
    repo: i64,
    mutation: Mutation,
) -> anyhow::Result<()> {
    match mutation {
        Mutation::Update => {
            db::repositories::records::update_repository(
                pool,
                actor,
                repo,
                Some("attempt"),
                false,
                db::repositories::records::MergeMethodUpdate {
                    merge: None,
                    squash: None,
                    rebase: None,
                },
            )
            .await?
        }
        Mutation::Rename => {
            db::repositories::records::rename_repository(pool, actor, repo, "attempt").await?;
        }
        Mutation::Delete => {
            db::repositories::records::delete_repository(pool, actor, repo).await?;
        }
        Mutation::MergeMethods => {
            db::repositories::records::set_merge_methods(
                pool,
                actor,
                repo,
                db::repositories::records::MergeMethodUpdate {
                    merge: Some(false),
                    squash: None,
                    rebase: Some(true),
                },
            )
            .await?;
        }
    }
    Ok(())
}
fn refusal<T: std::fmt::Debug>(result: anyhow::Result<T>, expected: Refusal) {
    let error = result.expect_err("mutation must refuse");
    assert_eq!(error.downcast_ref::<Refusal>(), Some(&expected));
    assert_eq!(error.to_string(), expected.to_string());
}
async fn denied(f: &Fixture, actor: i64, repo: i64) {
    denied_mutations(f, actor, repo, &MUTATIONS).await;
}
async fn denied_mutations(f: &Fixture, actor: i64, repo: i64, mutations: &[Mutation]) {
    let before = f.snapshot(repo).await;
    let files = f.files();
    for &mutation in mutations {
        refusal(
            mutate(f.database.pool(), actor, repo, mutation).await,
            Refusal::Denied,
        );
        assert_eq!(
            f.snapshot(repo).await,
            before,
            "denied {mutation:?} actor={actor}"
        );
        assert_eq!(f.files(), files, "denial changed filesystem");
    }
}
async fn accepted(
    f: &Fixture,
    actor: i64,
    owner: i64,
    org: Option<i64>,
    repo: i64,
    direct_admin: bool,
) {
    let original = db::repositories::records::get_repository_by_id(f.database.pool(), repo)
        .await
        .unwrap()
        .unwrap();
    let files = f.files();
    db::repositories::records::update_repository(
        f.database.pool(),
        actor,
        repo,
        Some("accepted"),
        false,
        db::repositories::records::MergeMethodUpdate {
            merge: None,
            squash: None,
            rebase: None,
        },
    )
    .await
    .unwrap();
    let updated = db::repositories::records::get_repository_by_id(f.database.pool(), repo)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.description.as_deref(), Some("accepted"));
    assert!(!updated.is_private);
    assert_eq!(updated.disk_path, original.disk_path);
    db::repositories::records::update_repository(
        f.database.pool(),
        actor,
        repo,
        Some("original"),
        true,
        db::repositories::records::MergeMethodUpdate {
            merge: None,
            squash: None,
            rebase: None,
        },
    )
    .await
    .unwrap();
    let alias = format!("accepted_{repo}");
    let renamed =
        db::repositories::records::rename_repository(f.database.pool(), actor, repo, &alias)
            .await
            .unwrap();
    assert_eq!(renamed.name, alias);
    assert_eq!(renamed.disk_path, original.disk_path);
    db::repositories::records::rename_repository(f.database.pool(), actor, repo, &original.name)
        .await
        .unwrap();
    assert_eq!(f.files(), files, "settings must never rename physical data");

    // Fresh deletion for each accepted principal, never fixture resurrection.
    let target = f.repo(owner, org, &format!("delete_{actor}_{repo}")).await;
    if direct_admin {
        db::repositories::access::grant_repo_access(
            f.database.pool(),
            actor,
            target,
            db::models::AccessLevel::Admin,
        )
        .await
        .unwrap();
    }
    let child = db::issues::create_issue(
        f.database.pool(),
        target,
        owner,
        "Retained child",
        "Original data",
    )
    .await
    .unwrap();
    let original = db::repositories::records::get_repository_by_id(f.database.pool(), target)
        .await
        .unwrap()
        .unwrap();
    let files = f.files();
    let deleted = db::repositories::records::delete_repository(f.database.pool(), actor, target)
        .await
        .unwrap();
    assert_eq!(deleted.disk_path, original.disk_path);
    // The row and everything that hangs off it are really gone.
    assert!(
        db::repositories::records::get_repository_by_id(f.database.pool(), target)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(f.snapshot(target).await, b"null");
    assert!(
        db::issues::get_issue(f.database.pool(), target, child.number)
            .await
            .unwrap()
            .is_none()
    );
    // Removing the directory is the caller's job; the database layer leaves it alone.
    assert_eq!(
        f.files(),
        files,
        "deleting the row must not touch the files"
    );
    denied(f, actor, target).await;
    // The deleted repository's name is free again.
    let contender = f
        .repo(owner, org, &format!("contender_{actor}_{repo}"))
        .await;
    let files = f.files();
    let renamed = db::repositories::records::rename_repository(
        f.database.pool(),
        owner,
        contender,
        &original.name,
    )
    .await
    .unwrap();
    assert_eq!(renamed.name, original.name);
    assert_eq!(f.files(), files);
}

async fn authority_matrix(pg: bool) {
    let mut f = Fixture::new(pg).await;
    let owner = f.actor("settings_owner").await;
    let admin = f.actor("settings_admin").await;
    let collaborator = f.actor("settings_collaborator").await;
    let writer = f.actor("settings_writer").await;
    let member = f.actor("settings_member").await;
    let unknown = f.actor("settings_unknown").await;
    let colliding = f.actor("settings_colliding").await;
    let site = f.actor("settings_site").await;
    f.database
        .execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[site])
        .await;
    // Deliberate user/org numeric ID collision; IDs never imply ownership.
    let org = colliding;
    f.database
        .execute(
            "INSERT INTO organizations(id,name) VALUES($1,'settings_org')",
            &[org],
        )
        .await;
    for (actor, role) in [
        (owner, "owner"),
        (admin, "admin"),
        (member, "member"),
        (unknown, "unknown"),
    ] {
        f.database
            .execute(
                &format!("INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'{role}')"),
                &[org, actor],
            )
            .await;
    }
    let personal = f.repo(owner, None, "personal").await;
    let organization = f.repo(owner, Some(org), "organization").await;
    for repo in [personal, organization] {
        db::repositories::access::grant_repo_access(
            f.database.pool(),
            collaborator,
            repo,
            db::models::AccessLevel::Admin,
        )
        .await
        .unwrap();
        db::repositories::access::grant_repo_access(
            f.database.pool(),
            writer,
            repo,
            db::models::AccessLevel::Write,
        )
        .await
        .unwrap();
    }
    for (actor, direct) in [(owner, false), (collaborator, true), (site, false)] {
        accepted(&f, actor, owner, None, personal, direct).await;
    }
    for (actor, direct) in [(owner, false), (admin, false), (collaborator, true)] {
        accepted(&f, actor, owner, Some(org), organization, direct).await;
    }
    for actor in [writer, member, unknown, colliding, -1] {
        denied(&f, actor, personal).await;
        denied(&f, actor, organization).await;
    }
    denied(&f, site, organization).await;
    for mutation in MUTATIONS {
        refusal(
            mutate(f.database.pool(), owner, -1, mutation).await,
            Refusal::Denied,
        );
    }
    // Current authority, not a cached access projection.
    db::repositories::access::revoke_repo_access(f.database.pool(), collaborator, organization)
        .await
        .unwrap();
    denied(&f, collaborator, organization).await;
    db::repositories::access::grant_repo_access(
        f.database.pool(),
        collaborator,
        organization,
        db::models::AccessLevel::Admin,
    )
    .await
    .unwrap();
    for role in ["member", "unknown"] {
        f.database
            .execute(
                &format!("UPDATE org_memberships SET role='{role}' WHERE org_id=$1 AND user_id=$2"),
                &[org, admin],
            )
            .await;
        denied(&f, admin, organization).await;
    }
    f.database
        .execute(
            "DELETE FROM org_memberships WHERE org_id=$1 AND user_id=$2",
            &[org, admin],
        )
        .await;
    denied(&f, admin, organization).await;
    f.database
        .execute(
            "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'admin')",
            &[org, admin],
        )
        .await;
    for level in [0, 6, 7, -1] {
        f.database
            .execute(
                &format!(
                    "UPDATE repo_access SET access_level={level} WHERE user_id=$1 AND repo_id=$2"
                ),
                &[collaborator, personal],
            )
            .await;
        denied(&f, collaborator, personal).await;
    }
    db::repositories::access::grant_repo_access(
        f.database.pool(),
        collaborator,
        personal,
        db::models::AccessLevel::Admin,
    )
    .await
    .unwrap();
    for flag in ["is_banned", "must_change_password"] {
        for actor in [owner, admin, collaborator, site] {
            f.database
                .execute(
                    &format!("UPDATE users SET {flag}=TRUE WHERE id=$1"),
                    &[actor],
                )
                .await;
            denied(&f, actor, personal).await;
            denied(&f, actor, organization).await;
            if actor == owner {
                // Suspended/restricted original governor also denies active admins.
                denied(&f, admin, organization).await;
                denied(&f, collaborator, organization).await;
                denied(&f, site, organization).await;
            }
            f.database
                .execute(
                    &format!("UPDATE users SET {flag}=FALSE WHERE id=$1"),
                    &[actor],
                )
                .await;
        }
    }
    for repo in [personal, organization] {
        // Archival refuses settings edits, but administrators may still delete the repository.
        f.database
            .execute(
                "UPDATE repositories SET is_archived=TRUE WHERE id=$1",
                &[repo],
            )
            .await;
        for actor in [owner, admin, collaborator, site] {
            denied_mutations(
                &f,
                actor,
                repo,
                &[Mutation::Update, Mutation::Rename, Mutation::MergeMethods],
            )
            .await;
            let may_delete = if repo == personal {
                actor != admin
            } else {
                actor != site
            };
            if !may_delete {
                refusal(
                    mutate(f.database.pool(), actor, repo, Mutation::Delete).await,
                    Refusal::Denied,
                );
            } else {
                let target = f
                    .repo(
                        owner,
                        (repo == organization).then_some(org),
                        &format!("archived-delete-{repo}-{actor}"),
                    )
                    .await;
                if actor == collaborator {
                    db::repositories::access::grant_repo_access(
                        f.database.pool(),
                        actor,
                        target,
                        db::models::AccessLevel::Admin,
                    )
                    .await
                    .unwrap();
                }
                f.database
                    .execute(
                        "UPDATE repositories SET is_archived=TRUE WHERE id=$1",
                        &[target],
                    )
                    .await;
                mutate(f.database.pool(), actor, target, Mutation::Delete)
                    .await
                    .unwrap();
                assert!(
                    db::repositories::records::get_repository_by_id(f.database.pool(), target)
                        .await
                        .unwrap()
                        .is_none()
                );
            }
        }
        f.database
            .execute(
                "UPDATE repositories SET is_archived=FALSE WHERE id=$1",
                &[repo],
            )
            .await;
    }
    f.database
        .execute(
            "UPDATE org_memberships SET role='unknown' WHERE org_id=$1 AND user_id=$2",
            &[org, owner],
        )
        .await;
    for actor in [owner, admin, collaborator, site] {
        denied(&f, actor, organization).await;
    }
    f.database
        .execute(
            "DELETE FROM org_memberships WHERE org_id=$1 AND user_id=$2",
            &[org, owner],
        )
        .await;
    for actor in [owner, admin, collaborator, site] {
        denied(&f, actor, organization).await;
    }
    f.database
        .execute(
            "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'owner')",
            &[org, owner],
        )
        .await;
    f.database
        .execute("DELETE FROM users WHERE id=$1", &[collaborator])
        .await;
    denied(&f, collaborator, personal).await;
    denied(&f, collaborator, organization).await;
    for repo in [personal, organization] {
        f.corrupt_unknown_kind(repo).await;
        for actor in [owner, admin, colliding, site] {
            denied(&f, actor, repo).await;
        }
    }
    f.database.finish().await;
}

async fn alias_and_visibility(pg: bool) {
    let mut f = Fixture::new(pg).await;
    let owner = f.actor("alias_owner").await;
    let other = f.actor("alias_other").await;
    let org = other;
    f.database
        .execute(
            "INSERT INTO organizations(id,name) VALUES($1,'alias_org')",
            &[org],
        )
        .await;
    f.database
        .execute(
            "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'owner')",
            &[org, owner],
        )
        .await;
    for (kind, organization) in [("user", None), ("org", Some(org))] {
        let repo = f
            .repo(owner, organization, &format!("original_{kind}"))
            .await;
        let reserved = f
            .repo(owner, organization, &format!("reserved_{kind}"))
            .await;
        let original = f.snapshot(repo).await;
        let files = f.files();
        let invalid = [
            "".to_owned(),
            ".".to_owned(),
            "..".to_owned(),
            "x".repeat(101),
            "é".repeat(51),
            "a/b".to_owned(),
            "a\\b".to_owned(),
            "white space".to_owned(),
            "a\0b".to_owned(),
            "😀".to_owned(),
        ];
        for name in invalid {
            refusal(
                db::repositories::records::rename_repository(f.database.pool(), owner, repo, &name)
                    .await,
                Refusal::InvalidName,
            );
            assert_eq!(f.snapshot(repo).await, original);
            assert_eq!(f.files(), files);
        }
        let reserved_name =
            db::repositories::records::get_repository_by_id(f.database.pool(), reserved)
                .await
                .unwrap()
                .unwrap()
                .name;
        let reserved_before = f.snapshot(reserved).await;
        refusal(
            db::repositories::records::rename_repository(
                f.database.pool(),
                owner,
                repo,
                &reserved_name,
            )
            .await,
            Refusal::NameConflict,
        );
        assert_eq!(f.snapshot(repo).await, original);
        assert_eq!(f.snapshot(reserved).await, reserved_before);
        assert_eq!(f.files(), files);
        for name in [
            "a".to_owned(),
            "x".repeat(100),
            "é".repeat(50),
            "文庫-._9".to_owned(),
            ".hidden".to_owned(),
        ] {
            let row =
                db::repositories::records::rename_repository(f.database.pool(), owner, repo, &name)
                    .await
                    .unwrap();
            assert_eq!(row.name, name);
            assert_eq!(f.files(), files);
        }
        // Admit real private-source ancestry through the existing native floor
        // triggers, then keep that history floor across a rename and the source's deletion.
        f.database
            .execute(
                "UPDATE repositories SET is_fork=TRUE,forked_from_id=$1 WHERE id=$2",
                &[reserved, repo],
            )
            .await;
        db::repositories::records::rename_repository(
            f.database.pool(),
            owner,
            repo,
            "private_history",
        )
        .await
        .unwrap();
        db::repositories::records::delete_repository(f.database.pool(), owner, reserved)
            .await
            .unwrap();
        let before = f.snapshot(repo).await;
        // The merge methods belong to each refused save, so neither may change.
        refusal(
            db::repositories::records::update_repository(
                f.database.pool(),
                owner,
                repo,
                Some("must not change"),
                false,
                db::repositories::records::MergeMethodUpdate {
                    merge: Some(false),
                    squash: None,
                    rebase: Some(false),
                },
            )
            .await,
            Refusal::VisibilityLocked,
        );
        assert_eq!(f.snapshot(repo).await, before);
        refusal(
            db::repositories::records::update_repository(
                f.database.pool(),
                owner,
                repo,
                Some("must not change"),
                true,
                db::repositories::records::MergeMethodUpdate {
                    merge: Some(false),
                    squash: Some(false),
                    rebase: Some(false),
                },
            )
            .await,
            Refusal::NoMergeMethod,
        );
        assert_eq!(f.snapshot(repo).await, before);
        db::repositories::records::update_repository(
            f.database.pool(),
            owner,
            repo,
            Some("private allowed"),
            true,
            db::repositories::records::MergeMethodUpdate {
                merge: None,
                squash: None,
                rebase: None,
            },
        )
        .await
        .unwrap();
        let after: serde_json::Value = serde_json::from_slice(&f.snapshot(repo).await).unwrap();
        assert_eq!(after["visibility_locked"], true);
        assert_eq!(after["repository"]["is_private"], true);
        assert_eq!(f.files(), files);
    }
    // An unrelated unique index is NOT an owner/name conflict classification.
    let repo = f.repo(owner, None, "unknown_error_source").await;
    let foreign = f.repo(other, None, "unknown_error_target").await;
    // Other repositories may share names across owner namespaces; isolate this index.
    f.database.execute("CREATE UNIQUE INDEX settings_unrelated_unique ON repositories(name) WHERE name IN ('unknown_error_source','unknown_error_target')", &[]).await;
    let before = f.snapshot(repo).await;
    let foreign_before = f.snapshot(foreign).await;
    let files = f.files();
    let error = db::repositories::records::rename_repository(
        f.database.pool(),
        owner,
        repo,
        "unknown_error_target",
    )
    .await
    .expect_err("unrelated unique violation");
    assert!(
        error.downcast_ref::<Refusal>().is_none(),
        "unknown error must remain unknown"
    );
    assert!(error.downcast_ref::<sqlx::Error>().is_some());
    assert_eq!(f.snapshot(repo).await, before);
    assert_eq!(f.snapshot(foreign).await, foreign_before);
    assert_eq!(f.files(), files);
    f.database.finish().await;
}

/// Waits until `waiters` backends are stuck behind the transaction of process
/// `holder`, directly or through each other.
async fn wait_until_blocked_by(raw: &sqlx::PgPool, holder: i32, waiters: i64) {
    for _ in 0..800 {
        let blocked: i64 = sqlx::query_scalar(
            "WITH RECURSIVE waiting(pid) AS (
                 SELECT pid FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))
                 UNION
                 SELECT a.pid FROM pg_stat_activity a JOIN waiting w ON w.pid = ANY(pg_blocking_pids(a.pid))
             ) SELECT COUNT(*) FROM waiting",
        )
        .bind(holder)
        .fetch_one(raw)
        .await
        .unwrap();
        if blocked >= waiters {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("the two requests are not waiting behind the lock the test holds");
}

/// Two requests change the same repository at once. They must queue behind each
/// other: neither may be picked as the victim of a lock deadlock.
async fn concurrent_mutations_queue_behind_each_other(pg: bool) {
    let mut f = Fixture::new(pg).await;
    let owner = f.actor("race_owner").await;
    for mutation in MUTATIONS {
        let repo = f.repo(owner, None, &format!("race_{mutation:?}")).await;
        // On PostgreSQL a reader holds the repository row until both requests have
        // reached it, so they start the race together: each one has asked for its lock
        // and none has it. (SQLite runs whole transactions one at a time.)
        let reader = match &f.database {
            Database::Postgres(fixture) => {
                let mut tx = fixture.raw.begin().await.unwrap();
                let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                    .fetch_one(&mut *tx)
                    .await
                    .unwrap();
                sqlx::query("SELECT id FROM repositories WHERE id = $1 FOR SHARE")
                    .bind(repo)
                    .fetch_one(&mut *tx)
                    .await
                    .unwrap();
                Some((tx, pid))
            }
            Database::Sqlite(_) => None,
        };
        let requests: Vec<_> = (0..2)
            .map(|_| {
                let pool = f.database.pool().clone();
                tokio::spawn(async move { mutate(&pool, owner, repo, mutation).await })
            })
            .collect();
        if let (Some((tx, pid)), Database::Postgres(fixture)) = (reader, &f.database) {
            wait_until_blocked_by(&fixture.raw, pid, 2).await;
            tx.commit().await.unwrap();
        }
        let mut results = Vec::new();
        for request in requests {
            let result = tokio::time::timeout(std::time::Duration::from_secs(30), request)
                .await
                .expect("a settings request waited forever")
                .unwrap();
            results.push(result);
        }
        match mutation {
            Mutation::Update | Mutation::Rename | Mutation::MergeMethods => {
                for result in results {
                    result.unwrap_or_else(|error| panic!("{mutation:?} failed: {error:#}"));
                }
            }
            Mutation::Delete => {
                // One request deletes the repository; the other finds it gone.
                let (done, refused): (Vec<_>, Vec<_>) =
                    results.into_iter().partition(|result| result.is_ok());
                assert_eq!(done.len(), 1, "exactly one delete wins");
                refusal(refused.into_iter().next().unwrap(), Refusal::Denied);
            }
        }
    }
    f.database.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_concurrent_settings_requests_do_not_deadlock() {
    concurrent_mutations_queue_behind_each_other(true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_concurrent_settings_requests_queue_up() {
    concurrent_mutations_queue_behind_each_other(false).await;
}

#[tokio::test]
async fn postgres_repository_settings_authority() {
    authority_matrix(true).await;
}
#[tokio::test]
async fn sqlite_repository_settings_authority() {
    authority_matrix(false).await;
}
#[tokio::test]
async fn postgres_repository_settings_alias_and_visibility() {
    alias_and_visibility(true).await;
}
#[tokio::test]
async fn sqlite_repository_settings_alias_and_visibility() {
    alias_and_visibility(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_collaborator_change_observes_a_ban_that_commits_first() {
    let mut f = Fixture::new(true).await;
    let owner = f.actor("collaborator_ban_owner").await;
    let target = f.actor("collaborator_ban_target").await;
    let repo = f.repo(owner, None, "collaborator_ban_repo").await;
    let Database::Postgres(postgres) = &f.database else {
        unreachable!("PostgreSQL fixture");
    };

    // Hold both rows the old split check/effect path crossed without retaining
    // account authority. The request must wait on the actor row, then recheck it.
    let mut ban = postgres.raw.begin().await.unwrap();
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *ban)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET is_banned=TRUE WHERE id=$1")
        .bind(owner)
        .execute(&mut *ban)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM repositories WHERE id=$1 FOR UPDATE")
        .bind(repo)
        .fetch_one(&mut *ban)
        .await
        .unwrap();

    let pool = f.database.pool().clone();
    let changing = tokio::spawn(async move {
        codeyard::backend::repositories::access::change_collaborator(
            &pool,
            owner,
            repo,
            "collaborator_ban_target",
            Some(db::models::AccessLevel::Write),
        )
        .await
    });
    wait_until_blocked_by(&postgres.raw, holder, 1).await;
    ban.commit().await.unwrap();

    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(30), changing)
            .await
            .expect("the collaborator request must finish after the ban")
            .unwrap()
            .unwrap_err(),
        codeyard::backend::repositories::access::AccessError::NotFound
    );
    let grants: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM repo_access WHERE repo_id=$1 AND user_id=$2")
            .bind(repo)
            .bind(target)
            .fetch_one(&postgres.raw)
            .await
            .unwrap();
    assert_eq!(grants, 0, "a banned owner cannot grant an accomplice");
    f.database.finish().await;
}

/// A fork's settings update holds the fork and then checks its source. The source
/// becoming private holds the source and then narrows the fork. Neither may hold
/// the row the other needs while it waits.
#[tokio::test]
async fn postgres_fork_settings_and_source_narrowing_do_not_deadlock() {
    let f = Fixture::new(true).await;
    let db::DbPool::Postgres(raw) = f.database.pool() else {
        unreachable!("PostgreSQL fixture");
    };
    let owner = f.actor("fork_settings_owner").await;
    let source_owner = f.actor("fork_settings_source").await;
    let source = f.repo(source_owner, None, "fork_settings_source").await;
    let fork = f.repo(owner, None, "fork_settings_fork").await;
    f.database
        .execute(
            "UPDATE repositories SET is_private=FALSE WHERE id=$1",
            &[source],
        )
        .await;
    f.database
        .execute(
            "UPDATE repositories SET is_fork=TRUE,forked_from_id=$1 WHERE id=$2",
            &[source, fork],
        )
        .await;
    // The source's settings update holds the source row while it narrows its forks.
    let mut source_update = raw.begin().await.unwrap();
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *source_update)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM repositories WHERE id = $1 FOR UPDATE")
        .bind(source)
        .execute(&mut *source_update)
        .await
        .unwrap();
    let pool = f.database.pool().clone();
    let fork_update = tokio::spawn(async move {
        db::repositories::records::update_repository(
            &pool,
            owner,
            fork,
            Some("fork settings"),
            true,
            db::repositories::records::MergeMethodUpdate {
                merge: None,
                squash: None,
                rebase: None,
            },
        )
        .await
    });
    wait_until_blocked_by(raw, holder, 1).await;
    sqlx::query("UPDATE repositories SET is_private = TRUE WHERE id = $1")
        .bind(source)
        .execute(&mut *source_update)
        .await
        .expect("narrowing the fork must not deadlock with its settings update");
    source_update.commit().await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), fork_update)
        .await
        .expect("the fork settings update must complete")
        .unwrap()
        .unwrap();
    let (is_private, locked): (bool, bool) =
        sqlx::query_as("SELECT is_private, visibility_locked FROM repositories WHERE id = $1")
            .bind(fork)
            .fetch_one(raw)
            .await
            .unwrap();
    assert!(
        is_private && locked,
        "the source's privacy reached the fork"
    );
}

/// PostgreSQL rejects a NUL byte in text. The merge methods are written before the description
/// in the same transaction, so that failed write must roll them back with it.
#[tokio::test]
async fn postgres_failed_settings_save_keeps_merge_methods() {
    let mut f = Fixture::new(true).await;
    let owner = f.actor("merge_rollback_owner").await;
    let repo = f.repo(owner, None, "merge_rollback").await;
    let before = f.snapshot(repo).await;
    let error = db::repositories::records::update_repository(
        f.database.pool(),
        owner,
        repo,
        Some("bad\0description"),
        true,
        db::repositories::records::MergeMethodUpdate {
            merge: None,
            squash: Some(false),
            rebase: None,
        },
    )
    .await
    .expect_err("PostgreSQL rejects a NUL byte in the description");
    assert!(error.downcast_ref::<Refusal>().is_none(), "{error}");
    assert_eq!(f.snapshot(repo).await, before);
    f.database.finish().await;
}
