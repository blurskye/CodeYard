//! Label authority, scope and atomicity on disposable PostgreSQL and SQLite
//! databases. Setup failure fails the test; nothing is skipped or mocked.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
mod contention;
use crate::support::database::Database as Fixture;

use codeyard::backend::db::{self, labels::LabelWriteRefusal};
use serde_json::{json, Value};

impl Fixture {
    async fn label_repo(&self, owner: i64, org: Option<i64>) -> i64 {
        if let Some(org) = org {
            self.scalar("INSERT INTO repositories(owner_kind,owner_org_id,name,is_private,disk_path) VALUES('org',$1,'labels',TRUE,'/owned/not-executed.git') RETURNING id", &[org]).await
        } else {
            self.scalar("INSERT INTO repositories(owner_kind,owner_user_id,name,is_private,disk_path) VALUES('user',$1,'labels',TRUE,'/owned/not-executed.git') RETURNING id", &[owner]).await
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Mutation {
    Create,
    Update,
    Delete,
    AddSingle,
    AddBatch,
    Remove,
    Replace,
}
const MUTATIONS: [Mutation; 7] = [
    Mutation::Create,
    Mutation::Update,
    Mutation::Delete,
    Mutation::AddSingle,
    Mutation::AddBatch,
    Mutation::Remove,
    Mutation::Replace,
];

async fn mutate(
    pool: &db::DbPool,
    actor: i64,
    repo: i64,
    issue: i64,
    label: i64,
    mutation: Mutation,
) -> anyhow::Result<()> {
    match mutation {
        Mutation::Create => {
            db::labels::create_label(pool, actor, repo, "attempt", "#123456", None).await?;
        }
        Mutation::Update => {
            db::labels::update_label(pool, actor, repo, label, Some("attempt"), None, None).await?;
        }
        Mutation::Delete => {
            db::labels::delete_label(pool, actor, repo, label).await?;
        }
        Mutation::AddSingle => {
            db::labels::add_issue_label(pool, actor, repo, issue, label).await?;
        }
        Mutation::AddBatch => {
            db::labels::add_issue_labels(pool, actor, repo, issue, &[label, label]).await?;
        }
        Mutation::Remove => {
            db::labels::remove_issue_label(pool, actor, repo, issue, label).await?;
        }
        Mutation::Replace => {
            db::labels::set_issue_labels(pool, actor, repo, issue, &[label]).await?;
        }
    }
    Ok(())
}

fn refusal<T: std::fmt::Debug>(result: anyhow::Result<T>, expected: LabelWriteRefusal) {
    let error = result.expect_err("mutation must refuse");
    assert_eq!(
        error.downcast_ref::<LabelWriteRefusal>(),
        Some(&expected),
        "unexpected error: {error:?}"
    );
    assert_eq!(
        error.to_string(),
        expected.to_string(),
        "no private names/driver details"
    );
}

async fn snapshot(pool: &db::DbPool, repos: &[i64]) -> Value {
    let mut rows = Vec::new();
    for repo in repos {
        let issue = db::issues::get_issue(pool, *repo, 1)
            .await
            .unwrap()
            .unwrap();
        rows.push(json!({
            "repo": db::repositories::records::get_repository_by_id(pool, *repo).await.unwrap(),
            "issue": &issue,
            "labels": db::labels::list_repo_labels(pool, *repo).await.unwrap(),
            "associations": db::labels::get_issue_labels(pool, issue.id).await.unwrap(),
        }));
    }
    json!(rows)
}

async fn seed(pool: &db::DbPool, owner: i64, repo: i64) -> (i64, i64) {
    let issue = db::issues::create_issue(pool, repo, owner, "Labels", "Fixture")
        .await
        .unwrap();
    let label =
        db::labels::create_label(pool, owner, repo, "baseline", "#123456", Some("preserved"))
            .await
            .unwrap();
    db::labels::set_issue_labels(pool, owner, repo, issue.id, &[label.id])
        .await
        .unwrap();
    (issue.id, label.id)
}

async fn denied(f: &Fixture, actor: i64, repo: i64, issue: i64, label: i64) {
    let before = snapshot(f.pool(), &[repo]).await;
    for mutation in MUTATIONS {
        refusal(
            mutate(f.pool(), actor, repo, issue, label, mutation).await,
            LabelWriteRefusal::Denied,
        );
        assert_eq!(
            snapshot(f.pool(), &[repo]).await,
            before,
            "denied {mutation:?} actor={actor}"
        );
    }
}

async fn accepted(f: &Fixture, actor: i64, repo: i64, issue: i64, baseline: i64) {
    let pool = f.pool();
    let label = db::labels::create_label(pool, actor, repo, "accepted", "#654321", None)
        .await
        .unwrap();
    let updated =
        db::labels::update_label(pool, actor, repo, label.id, Some("updated"), None, None)
            .await
            .unwrap();
    assert_eq!(updated.name, "updated");
    db::labels::add_issue_label(pool, actor, repo, issue, label.id)
        .await
        .unwrap();
    let labels =
        db::labels::add_issue_labels(pool, actor, repo, issue, &[label.id, baseline, label.id])
            .await
            .unwrap();
    assert_eq!(labels.len(), 2);
    assert!(labels.iter().all(|l| l.repo_id == repo));
    db::labels::set_issue_labels(pool, actor, repo, issue, &[label.id, label.id])
        .await
        .unwrap();
    assert_eq!(
        db::labels::get_issue_labels(pool, issue)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        db::labels::remove_issue_label(pool, actor, repo, issue, label.id)
            .await
            .unwrap()
    );
    assert!(
        !db::labels::remove_issue_label(pool, actor, repo, issue, label.id)
            .await
            .unwrap()
    );
    assert!(db::labels::delete_label(pool, actor, repo, label.id)
        .await
        .unwrap());
    db::labels::set_issue_labels(pool, actor, repo, issue, &[baseline])
        .await
        .unwrap();
}

async fn authority_matrix(pg: bool) {
    let mut f = Fixture::new(pg).await;
    let owner = f.actor("label_owner").await;
    let admin = f.actor("label_admin").await;
    let member = f.actor("label_member").await;
    let writer = f.actor("label_writer").await;
    let direct_admin = f.actor("label_direct_admin").await;
    let site = f.actor("label_siteadmin").await;
    let unknown = f.actor("label_unknown").await;
    let outsider = f.actor("label_outsider").await;
    f.execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[site])
        .await;
    let org = f
        .scalar(
            "INSERT INTO organizations(name) VALUES('label_org') RETURNING id",
            &[],
        )
        .await;
    for (actor, role) in [
        (owner, "owner"),
        (admin, "admin"),
        (member, "member"),
        (unknown, "unknown"),
    ] {
        f.execute(
            &format!("INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'{role}')"),
            &[org, actor],
        )
        .await;
    }
    let personal = f.label_repo(owner, None).await;
    let repo = f.label_repo(owner, Some(org)).await;
    let (personal_issue, personal_label) = seed(f.pool(), owner, personal).await;
    let (issue, label) = seed(f.pool(), owner, repo).await;
    for target in [personal, repo] {
        db::repositories::access::grant_repo_access(
            f.pool(),
            direct_admin,
            target,
            db::models::repositories::AccessLevel::Admin,
        )
        .await
        .unwrap();
        db::repositories::access::grant_repo_access(
            f.pool(),
            writer,
            target,
            db::models::repositories::AccessLevel::Write,
        )
        .await
        .unwrap();
    }
    // Author status is not association authority: member authors a real issue.
    db::repositories::access::grant_repo_access(
        f.pool(),
        member,
        repo,
        db::models::repositories::AccessLevel::Read,
    )
    .await
    .unwrap();
    let authored = db::issues::create_issue(f.pool(), repo, member, "Authored", "Fixture")
        .await
        .unwrap();
    refusal(
        db::labels::add_issue_label(f.pool(), member, repo, authored.id, label).await,
        LabelWriteRefusal::Denied,
    );
    assert!(db::labels::get_issue_labels(f.pool(), authored.id)
        .await
        .unwrap()
        .is_empty());
    for actor in [owner, direct_admin, site] {
        accepted(&f, actor, personal, personal_issue, personal_label).await;
    }
    for actor in [owner, admin, direct_admin] {
        accepted(&f, actor, repo, issue, label).await;
    }
    for actor in [member, writer, site, unknown, outsider, -1] {
        denied(&f, actor, repo, issue, label).await;
    }
    denied(&f, writer, personal, personal_issue, personal_label).await;
    // Team Admin is canonical Admin; team Write is not enough.
    let team = f
        .scalar(
            "INSERT INTO teams(org_id,name,slug) VALUES($1,'Label team','label-team') RETURNING id",
            &[org],
        )
        .await;
    f.execute(
        "INSERT INTO team_memberships(team_id,user_id) VALUES($1,$2)",
        &[team, member],
    )
    .await;
    f.execute(
        "INSERT INTO team_repo_access(team_id,repo_id,permission) VALUES($1,$2,3)",
        &[team, repo],
    )
    .await;
    denied(&f, member, repo, issue, label).await;
    f.execute(
        "UPDATE team_repo_access SET permission=5 WHERE team_id=$1 AND repo_id=$2",
        &[team, repo],
    )
    .await;
    accepted(&f, member, repo, issue, label).await;
    f.execute(
        "UPDATE team_memberships SET role='unknown' WHERE team_id=$1 AND user_id=$2",
        &[team, member],
    )
    .await;
    denied(&f, member, repo, issue, label).await;
    f.execute(
        "UPDATE repo_access SET access_level=6 WHERE user_id=$1 AND repo_id=$2",
        &[direct_admin, repo],
    )
    .await;
    denied(&f, direct_admin, repo, issue, label).await;
    db::repositories::access::grant_repo_access(
        f.pool(),
        direct_admin,
        repo,
        db::models::repositories::AccessLevel::Admin,
    )
    .await
    .unwrap();
    for actor in [owner, admin, direct_admin, site] {
        for flag in ["is_banned", "must_change_password"] {
            f.execute(
                &format!("UPDATE users SET {flag}=TRUE WHERE id=$1"),
                &[actor],
            )
            .await;
            denied(&f, actor, personal, personal_issue, personal_label).await;
            denied(&f, actor, repo, issue, label).await;
            f.execute(
                &format!("UPDATE users SET {flag}=FALSE WHERE id=$1"),
                &[actor],
            )
            .await;
        }
    }
    f.execute(
        "UPDATE org_memberships SET role='member' WHERE org_id=$1 AND user_id=$2",
        &[org, admin],
    )
    .await;
    denied(&f, admin, repo, issue, label).await;
    f.execute(
        "UPDATE org_memberships SET role='admin' WHERE org_id=$1 AND user_id=$2",
        &[org, admin],
    )
    .await;
    // Only original usable governance; no owner recovery or site-admin conversion.
    for flag in ["is_banned", "must_change_password"] {
        f.execute(
            &format!("UPDATE users SET {flag}=TRUE WHERE id=$1"),
            &[owner],
        )
        .await;
        for actor in [owner, admin, direct_admin, site] {
            denied(&f, actor, repo, issue, label).await;
        }
        f.execute(
            &format!("UPDATE users SET {flag}=FALSE WHERE id=$1"),
            &[owner],
        )
        .await;
    }
    f.execute(
        "UPDATE org_memberships SET role='unknown' WHERE org_id=$1 AND user_id=$2",
        &[org, owner],
    )
    .await;
    for actor in [owner, admin, direct_admin, site] {
        denied(&f, actor, repo, issue, label).await;
    }
    f.execute(
        "UPDATE org_memberships SET role='owner' WHERE org_id=$1 AND user_id=$2",
        &[org, owner],
    )
    .await;
    for target in [personal, repo] {
        let (i, l) = if target == personal {
            (personal_issue, personal_label)
        } else {
            (issue, label)
        };
        f.execute(
            "UPDATE repositories SET is_archived=TRUE WHERE id=$1",
            &[target],
        )
        .await;
        for actor in [owner, admin, direct_admin, site] {
            denied(&f, actor, target, i, l).await;
        }
        f.execute(
            "UPDATE repositories SET is_archived=FALSE WHERE id=$1",
            &[target],
        )
        .await;
        // A deleted repository refuses every label write.
        f.execute("DELETE FROM repositories WHERE id=$1", &[target])
            .await;
        for actor in [owner, admin, direct_admin, site] {
            for mutation in MUTATIONS {
                refusal(
                    mutate(f.pool(), actor, target, i, l, mutation).await,
                    LabelWriteRefusal::Denied,
                );
            }
        }
    }
    f.finish().await;
}

async fn scope_and_batches(pg: bool) {
    let mut f = Fixture::new(pg).await;
    let owner = f.actor("scope_owner").await;
    let foreign_owner = f.actor("foreign_owner").await;
    let repo = f.label_repo(owner, None).await;
    let foreign = f.label_repo(foreign_owner, None).await;
    let (issue, label) = seed(f.pool(), owner, repo).await;
    let (foreign_issue, foreign_label) = seed(f.pool(), foreign_owner, foreign).await;
    let valid = db::labels::create_label(f.pool(), owner, repo, "valid", "#123456", None)
        .await
        .unwrap()
        .id;
    let before = snapshot(f.pool(), &[repo, foreign]).await;
    for bad in [foreign_label, -1] {
        refusal(
            db::labels::update_label(
                f.pool(),
                owner,
                repo,
                bad,
                Some("private overwrite"),
                None,
                None,
            )
            .await,
            LabelWriteRefusal::InvalidLabels,
        );
        refusal(
            db::labels::delete_label(f.pool(), owner, repo, bad).await,
            LabelWriteRefusal::InvalidLabels,
        );
        refusal(
            db::labels::add_issue_label(f.pool(), owner, repo, issue, bad).await,
            LabelWriteRefusal::InvalidLabels,
        );
        refusal(
            db::labels::remove_issue_label(f.pool(), owner, repo, issue, bad).await,
            LabelWriteRefusal::InvalidLabels,
        );
        for batch in [vec![valid, bad], vec![bad, valid]] {
            refusal(
                db::labels::add_issue_labels(f.pool(), owner, repo, issue, &batch).await,
                LabelWriteRefusal::InvalidLabels,
            );
            assert_eq!(
                snapshot(f.pool(), &[repo, foreign]).await,
                before,
                "no partial add"
            );
            refusal(
                db::labels::set_issue_labels(f.pool(), owner, repo, issue, &batch).await,
                LabelWriteRefusal::InvalidLabels,
            );
            assert_eq!(
                snapshot(f.pool(), &[repo, foreign]).await,
                before,
                "no partial remove/replace"
            );
        }
    }
    for bad_issue in [foreign_issue, -1] {
        refusal(
            db::labels::add_issue_label(f.pool(), owner, repo, bad_issue, label).await,
            LabelWriteRefusal::InvalidLabels,
        );
        refusal(
            db::labels::add_issue_labels(f.pool(), owner, repo, bad_issue, &[label, valid]).await,
            LabelWriteRefusal::InvalidLabels,
        );
        refusal(
            db::labels::remove_issue_label(f.pool(), owner, repo, bad_issue, label).await,
            LabelWriteRefusal::InvalidLabels,
        );
        refusal(
            db::labels::set_issue_labels(f.pool(), owner, repo, bad_issue, &[]).await,
            LabelWriteRefusal::InvalidLabels,
        );
        assert_eq!(snapshot(f.pool(), &[repo, foreign]).await, before);
    }
    // Symmetric scope mismatch even if the actor administers both repositories.
    db::repositories::access::grant_repo_access(
        f.pool(),
        owner,
        foreign,
        db::models::repositories::AccessLevel::Admin,
    )
    .await
    .unwrap();
    refusal(
        db::labels::set_issue_labels(f.pool(), owner, foreign, issue, &[foreign_label]).await,
        LabelWriteRefusal::InvalidLabels,
    );
    refusal(
        db::labels::remove_issue_label(f.pool(), owner, foreign, foreign_issue, label).await,
        LabelWriteRefusal::InvalidLabels,
    );
    assert_eq!(snapshot(f.pool(), &[repo, foreign]).await, before);
    let oversized = vec![valid; db::labels::MAX_ISSUE_LABELS + 1];
    refusal(
        db::labels::add_issue_labels(f.pool(), owner, repo, issue, &oversized).await,
        LabelWriteRefusal::InvalidLabels,
    );
    refusal(
        db::labels::set_issue_labels(f.pool(), owner, repo, issue, &oversized).await,
        LabelWriteRefusal::InvalidLabels,
    );
    assert_eq!(snapshot(f.pool(), &[repo, foreign]).await, before);
    let labels = db::labels::add_issue_labels(
        f.pool(),
        owner,
        repo,
        issue,
        &vec![valid; db::labels::MAX_ISSUE_LABELS],
    )
    .await
    .unwrap();
    assert_eq!(labels.len(), 2);
    db::labels::set_issue_labels(f.pool(), owner, repo, issue, &[valid, valid])
        .await
        .unwrap();
    assert_eq!(
        db::labels::get_issue_labels(f.pool(), issue)
            .await
            .unwrap()
            .len(),
        1
    );
    db::labels::set_issue_labels(f.pool(), owner, repo, issue, &[])
        .await
        .unwrap();
    assert!(db::labels::get_issue_labels(f.pool(), issue)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        snapshot(f.pool(), &[foreign]).await,
        json!([before[1].clone()])
    );
    f.finish().await;
}

#[tokio::test]
async fn postgres_label_authority_matrix() {
    authority_matrix(true).await;
}
#[tokio::test]
async fn sqlite_label_authority_matrix() {
    authority_matrix(false).await;
}
#[tokio::test]
async fn postgres_label_scope_and_atomic_batches() {
    scope_and_batches(true).await;
}
#[tokio::test]
async fn sqlite_label_scope_and_atomic_batches() {
    scope_and_batches(false).await;
}
