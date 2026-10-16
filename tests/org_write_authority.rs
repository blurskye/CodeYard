//! Who can write to organization repositories (edit issues/PRs, merge, fork,
//! run CI) as team grants, roles and account restrictions change.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
#[path = "support/native_database.rs"]
mod native;
#[path = "support/production_ci.rs"]
mod production;

use codeyard::backend::db::{
    self,
    discussions::DiscussionEdit,
    models::{AccessLevel, CiRun, OrgRole, Repository},
    organizations::{self as orgs, OrganizationOutcome},
};
use db::repositories::{insert_repository, NewRepository};
use uuid::Uuid;

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DISK: &str = "/owned-dao-fixture/not-executed.git";

enum Database {
    Postgres(Box<production::Fixture>),
    Sqlite(native::NativeDatabase),
}
impl Database {
    async fn new(pg: bool) -> Self {
        if pg {
            Self::Postgres(Box::new(production::Fixture::new().await))
        } else {
            Self::Sqlite(native::NativeDatabase::new().await)
        }
    }
    fn pool(&self) -> &db::DbPool {
        match self {
            Self::Postgres(f) => &f.pool,
            Self::Sqlite(f) => &f.pool,
        }
    }
    async fn execute(&self, sql: &str, ids: &[i64]) {
        match self {
            Self::Postgres(f) => {
                let mut q = sqlx::query(sql);
                for id in ids {
                    q = q.bind(*id);
                }
                q.execute(&f.raw).await.expect("owned PG fixture mutation");
            }
            Self::Sqlite(f) => {
                let mut q = sqlx::query(sql);
                for id in ids {
                    q = q.bind(*id);
                }
                q.execute(&f.raw)
                    .await
                    .expect("owned SQLite fixture mutation");
            }
        }
    }
    async fn actor(&self, name: &str) -> i64 {
        db::users::accounts::create_user(self.pool(), name, None)
            .await
            .unwrap()
            .id
    }
    async fn repo(&self, owner: i64, org: Option<i64>, name: &str) -> Repository {
        let repo = db::repositories::records::create_repository(
            self.pool(),
            owner,
            name,
            None,
            true,
            DISK,
        )
        .await
        .unwrap();
        if let Some(org) = org {
            self.execute("UPDATE repositories SET owner_kind='org',owner_user_id=NULL,owner_org_id=$1 WHERE id=$2", &[org, repo.id]).await;
        }
        db::repositories::records::get_repository_by_id(self.pool(), repo.id)
            .await
            .unwrap()
            .unwrap()
    }
    async fn close(&mut self) {
        match self {
            Self::Postgres(f) => f.finish().await,
            Self::Sqlite(f) => f.close().await,
        }
    }
}
fn applied<T: std::fmt::Debug>(outcome: OrganizationOutcome<T>) -> T {
    match outcome {
        OrganizationOutcome::Applied(value) => value,
        other => panic!("expected Applied: {other:?}"),
    }
}
async fn expect_writes(database: &Database, actor: i64, repo: i64, allowed: bool) {
    // Historical rows are constructed only in this owned schema. Creation has
    // separate contracts: suspended/banned actors cannot seed via production
    // creators just to test edit/merge refusal. No physical Git work is run.
    static NEXT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(900_000);
    let number = NEXT.fetch_add(2, std::sync::atomic::Ordering::Relaxed);
    database.execute(
        "INSERT INTO issues(repo_id,number,author_id,title,body) VALUES($1,$2,$3,'Authority','Fixture')",
        &[repo, number, actor],
    ).await;
    database.execute(
        "INSERT INTO pull_requests(repo_id,number,author_id,title,body,head_branch,head_sha,base_branch,base_sha,is_draft) VALUES($1,$2,$3,'Authority','Fixture','feature','aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa','main','aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',FALSE)",
        &[repo, number + 1, actor],
    ).await;
    let pool = database.pool();
    let issue = db::issues::get_issue(pool, repo, i32::try_from(number).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        db::issues::update_issue(
            pool,
            issue.id,
            DiscussionEdit {
                actor_id: actor,
                expected_state: &issue.state,
                expected_updated: issue.updated_at,
                title: Some("Authorized edit"),
                body: None,
                state: None,
            }
        )
        .await
        .unwrap()
        .is_some(),
        allowed,
        "issue actor={actor}"
    );
    assert_eq!(
        db::issues::get_issue(pool, repo, i32::try_from(number).unwrap())
            .await
            .unwrap()
            .unwrap()
            .title,
        if allowed {
            "Authorized edit"
        } else {
            "Authority"
        },
        "persisted issue actor={actor}"
    );
    let pull = db::pull_requests::records::get_pull_request(
        pool,
        repo,
        i32::try_from(number + 1).unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    let updated = db::pull_requests::records::update_pull_request(
        pool,
        pull.id,
        DiscussionEdit {
            actor_id: actor,
            expected_state: &pull.state,
            expected_updated: pull.updated_at,
            title: Some("Authorized edit"),
            body: None,
            state: None,
        },
        None,
    )
    .await
    .unwrap();
    assert_eq!(updated.is_some(), allowed, "PR actor={actor}");
    assert_eq!(
        db::pull_requests::records::get_pull_request(
            pool,
            repo,
            i32::try_from(number + 1).unwrap()
        )
        .await
        .unwrap()
        .unwrap()
        .title,
        if allowed {
            "Authorized edit"
        } else {
            "Authority"
        },
        "persisted PR actor={actor}"
    );
    let user = db::users::accounts::get_user_by_id(pool, actor)
        .await
        .unwrap();
    let repository = db::repositories::records::get_repository_by_id(pool, repo)
        .await
        .unwrap()
        .unwrap();
    let can_merge = match user {
        Some(user) => codeyard::backend::repositories::access::can_write(pool, &user, &repository)
            .await
            .unwrap(),
        None => false,
    };
    assert_eq!(can_merge, allowed, "merge actor={actor}");
}
async fn fork(pool: &db::DbPool, actor: i64, source: i64) -> anyhow::Result<Option<Repository>> {
    let name = format!("fork_{}", Uuid::new_v4().simple());
    insert_repository(
        pool,
        NewRepository {
            owner: db::repositories::RepositoryCreationOwner::personal(actor),
            name: &name,
            description: None,
            is_private: false,
            disk_path: DISK,
            storage_node_id: None,
            source_id: Some(source),
        },
    )
    .await
}
async fn enqueue(
    pool: &db::DbPool,
    actor: i64,
    repo: i64,
    cd: bool,
    trusted: &[i64],
) -> anyhow::Result<CiRun> {
    db::ci::enqueue_ci_run(
        pool,
        db::ci::NewCiRun {
            repo_id: repo,
            triggered_by_id: actor,
            pipeline: if cd { "cd" } else { "ci" },
            event: "workflow_dispatch",
            pull_request_id: None,
            branch: "main",
            head_sha: SHA,
            workflow_path: ".github/workflows/ci.yml",
            workflow_blobs: &serde_json::json!({".github/workflows/ci.yml": SHA}),
            workflow_inputs: None,
            environment: None,
        },
        trusted,
    )
    .await
}

async fn authority_contract(pg: bool) {
    let mut database = Database::new(pg).await;
    let pool = database.pool();
    let owner = database.actor("write_owner").await;
    let admin = database.actor("write_admin").await;
    let member = database.actor("write_member").await;
    let direct = database.actor("write_direct").await;
    let outsider = database.actor("write_outsider").await;
    let site = database.actor("write_site_admin").await;
    database
        .execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[site])
        .await;
    let org = applied(
        orgs::create_organization(
            pool,
            owner,
            &orgs::NewOrganization::new(
                "write_org",
                orgs::OrganizationProfile::new(None, None).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    for (actor, role) in [(admin, OrgRole::Admin), (member, OrgRole::Member)] {
        applied(
            orgs::add_organization_member(pool, owner, org, actor, role)
                .await
                .unwrap(),
        );
    }
    let team = applied(
        orgs::create_organization_team(
            pool,
            owner,
            org,
            &orgs::NewTeam::new(
                "write_team",
                orgs::TeamProfile::new("Write", None, orgs::TeamPrivacy::Secret).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    applied(
        orgs::add_organization_team_member(pool, owner, org, team, member, orgs::TeamRole::Member)
            .await
            .unwrap(),
    );
    let repo = database.repo(owner, Some(org), "write_private").await;
    let personal = database.repo(owner, None, "write_personal").await;
    applied(
        orgs::grant_organization_team_repository(
            pool,
            owner,
            org,
            team,
            repo.id,
            orgs::TeamRepositoryPermission::new(3).unwrap(),
        )
        .await
        .unwrap(),
    );
    db::repositories::access::grant_repo_access(pool, direct, repo.id, AccessLevel::Write)
        .await
        .unwrap();
    for (actor, allowed) in [
        (owner, true),
        (admin, true),
        (member, true),
        (direct, true),
        (outsider, false),
        (site, false),
    ] {
        expect_writes(&database, actor, repo.id, allowed).await;
        assert_eq!(
            enqueue(pool, actor, repo.id, false, &[actor]).await.is_ok(),
            allowed,
            "CI actor={actor}"
        );
        if allowed {
            assert!(fork(pool, actor, repo.id).await.unwrap().is_some());
        } else {
            assert!(fork(pool, actor, repo.id).await.is_err());
        }
    }
    // Personal site-admin policy is deliberately retained, not generalized to orgs.
    expect_writes(&database, site, personal.id, true).await;
    assert!(fork(pool, site, personal.id).await.unwrap().is_some());
    assert!(enqueue(pool, site, personal.id, false, &[]).await.is_ok());
    assert!(enqueue(pool, owner, repo.id, false, &[]).await.is_err()); // host allowlist
    assert!(enqueue(pool, member, repo.id, true, &[member])
        .await
        .is_err()); // CD requires Admin
    let cd_repo = database.repo(owner, Some(org), "write_cd").await;
    assert!(enqueue(pool, admin, cd_repo.id, true, &[admin])
        .await
        .is_ok());

    // Cancelling: the person who started the run and repository admins may;
    // other writers, outsiders and site admins (without repo access) may not.
    for (actor, allowed) in [
        (outsider, false),
        (site, false),
        (direct, false),
        (admin, true),
        (owner, true),
        (member, true),
    ] {
        let run = enqueue(pool, member, repo.id, false, &[member])
            .await
            .unwrap();
        let result = db::ci::request_ci_cancellation(pool, run.run_id, actor).await;
        assert_eq!(result.is_ok(), allowed, "cancel actor={actor}: {result:?}");
        if allowed {
            assert!(
                result.unwrap().0,
                "cancellation must be applied actor={actor}"
            );
        }
        let saved = db::ci::get_ci_run_by_run_id(pool, run.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.status, if allowed { "cancelled" } else { "queued" });
    }
    let valid = enqueue(pool, member, repo.id, false, &[member])
        .await
        .unwrap();
    // Admission cannot be reused after direct/team grant revocation at either start API.
    let direct_run = enqueue(pool, direct, repo.id, false, &[direct])
        .await
        .unwrap();
    // The direct grant goes behind the revocation code's back, so that run stays queued
    // and only the starter's own right to cancel it is left to observe. The team grant
    // goes through the revocation, which cancels the member's queued run on the spot.
    database
        .execute(
            "DELETE FROM repo_access WHERE user_id=$1 AND repo_id=$2",
            &[direct, repo.id],
        )
        .await;
    applied(
        orgs::revoke_organization_team_repository(pool, owner, org, team, repo.id)
            .await
            .unwrap(),
    );
    let revoked = db::ci::get_ci_run_by_run_id(pool, valid.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(revoked.status, "cancelled");
    for (actor, run, still_queued) in [(direct, &direct_run, true), (member, &valid, false)] {
        expect_writes(&database, actor, repo.id, false).await;
        // Whoever started a run may cancel it after losing access; once a revocation
        // has cancelled it there is nothing left to cancel, and asking is no error.
        assert_eq!(
            db::ci::request_ci_cancellation(pool, run.run_id, actor)
                .await
                .unwrap()
                .0,
            still_queued
        );
    }
    db::repositories::access::grant_repo_access(pool, direct, repo.id, AccessLevel::Admin)
        .await
        .unwrap();
    applied(
        orgs::grant_organization_team_repository(
            pool,
            owner,
            org,
            team,
            repo.id,
            orgs::TeamRepositoryPermission::new(5).unwrap(),
        )
        .await
        .unwrap(),
    );
    // Suspension applies to direct nonmembers too. Sweep catches account/role
    // changes and recovery uses current canonical facts rather than stale admission.
    for flag in ["is_banned", "must_change_password"] {
        let a = enqueue(pool, direct, repo.id, false, &[direct])
            .await
            .unwrap();
        let _b = enqueue(pool, member, repo.id, false, &[member])
            .await
            .unwrap();
        database
            .execute(
                &format!("UPDATE users SET {flag}=TRUE WHERE id=$1"),
                &[owner],
            )
            .await;
        for actor in [admin, member, direct, site] {
            expect_writes(&database, actor, repo.id, false).await;
            assert!(enqueue(pool, actor, repo.id, false, &[actor])
                .await
                .is_err());
        }
        assert!(db::ci::request_ci_cancellation(pool, a.run_id, admin)
            .await
            .is_err());
        assert!(
            db::ci::request_ci_cancellation(pool, a.run_id, direct)
                .await
                .unwrap()
                .0
        );
        // The explicit recovery DAO restores governance, not site-admin repo authority.
        applied(
            orgs::recover_suspended_organization_owner(pool, site, org, admin)
                .await
                .unwrap(),
        );
        assert!(enqueue(pool, direct, repo.id, false, &[direct])
            .await
            .is_ok());
        expect_writes(&database, site, repo.id, false).await;
        database
            .execute(
                &format!("UPDATE users SET {flag}=FALSE WHERE id=$1"),
                &[owner],
            )
            .await;
        applied(
            orgs::update_organization_member_role(pool, owner, org, admin, OrgRole::Admin)
                .await
                .unwrap(),
        );
    }
    // Restricted actors fail even with team/direct Admin; private grants do not
    // repair a banned/restricted actor and original-trigger cancellation rechecks it.
    for actor in [member, direct] {
        let run = enqueue(pool, actor, repo.id, false, &[actor])
            .await
            .unwrap();
        for flag in ["is_banned", "must_change_password"] {
            database
                .execute(
                    &format!("UPDATE users SET {flag}=TRUE WHERE id=$1"),
                    &[actor],
                )
                .await;
            expect_writes(&database, actor, repo.id, false).await;
            assert!(enqueue(pool, actor, repo.id, false, &[actor])
                .await
                .is_err());
            assert!(db::ci::request_ci_cancellation(pool, run.run_id, actor)
                .await
                .is_err());
            database
                .execute(
                    &format!("UPDATE users SET {flag}=FALSE WHERE id=$1"),
                    &[actor],
                )
                .await;
        }
    }
    // Unknown direct collaborator Owner is not a write grant.
    database
        .execute(
            "UPDATE repo_access SET access_level=6 WHERE user_id=$1 AND repo_id=$2",
            &[direct, repo.id],
        )
        .await;
    expect_writes(&database, direct, repo.id, false).await;
    assert!(enqueue(pool, direct, repo.id, false, &[direct])
        .await
        .is_err());
    db::repositories::access::grant_repo_access(pool, direct, repo.id, AccessLevel::Admin)
        .await
        .unwrap();
    let _role_run = enqueue(pool, admin, repo.id, false, &[admin])
        .await
        .unwrap();
    database
        .execute(
            "UPDATE org_memberships SET role='member' WHERE org_id=$1 AND user_id=$2",
            &[org, admin],
        )
        .await;
    applied(
        orgs::update_organization_member_role(pool, owner, org, admin, OrgRole::Admin)
            .await
            .unwrap(),
    );
    // Parented teams have no effective contribution, including the contribution API.
    let _parented_run = enqueue(pool, member, repo.id, false, &[member])
        .await
        .unwrap();
    database
        .execute("UPDATE teams SET parent_team_id=$1 WHERE id=$1", &[team])
        .await;
    assert_eq!(
        orgs::get_organization_repo_access(pool, member, &repo)
            .await
            .unwrap(),
        AccessLevel::None
    );
    expect_writes(&database, member, repo.id, false).await;
    assert!(enqueue(pool, member, repo.id, false, &[member])
        .await
        .is_err());
    database
        .execute("UPDATE teams SET parent_team_id=NULL WHERE id=$1", &[team])
        .await;
    database
        .execute(
            "UPDATE repositories SET is_archived=TRUE WHERE id=$1",
            &[repo.id],
        )
        .await;
    for actor in [owner, admin, member, direct, site] {
        expect_writes(&database, actor, repo.id, false).await;
        assert!(enqueue(pool, actor, repo.id, false, &[actor])
            .await
            .is_err());
    }
    database
        .execute(
            "UPDATE repositories SET is_archived=FALSE WHERE id=$1",
            &[repo.id],
        )
        .await;
    // Numeric org/user identity collision is not personal ownership.
    database
        .execute(
            "INSERT INTO users(id,username) VALUES(700000,'write_collision_user')",
            &[],
        )
        .await;
    database
        .execute(
            "INSERT INTO organizations(id,name) VALUES(700000,'write_collision_org')",
            &[],
        )
        .await;
    database
        .execute(
            "INSERT INTO org_memberships(org_id,user_id,role) VALUES(700000,$1,'owner')",
            &[owner],
        )
        .await;
    let collision = database.repo(owner, Some(700000), "write_collision").await;
    expect_writes(&database, 700000, collision.id, false).await;
    assert!(enqueue(pool, 700000, collision.id, false, &[700000])
        .await
        .is_err());
    assert!(fork(pool, 700000, collision.id).await.is_err());
    let collision_personal = database
        .repo(700000, None, "write_collision_personal")
        .await;
    expect_writes(&database, 700000, collision_personal.id, true).await;
    assert!(
        enqueue(pool, 700000, collision_personal.id, false, &[700000])
            .await
            .is_ok()
    );
    database.close().await;
}

async fn team_cancellation_stays_with_its_repositories(pg: bool) {
    let mut database = Database::new(pg).await;
    let pool = database.pool();
    let owner = database.actor("team_cancel_owner").await;
    let member = database.actor("team_cancel_member").await;
    let org = applied(
        orgs::create_organization(
            pool,
            owner,
            &orgs::NewOrganization::new(
                "team_cancel_org",
                orgs::OrganizationProfile::new(None, None).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    applied(
        orgs::add_organization_member(pool, owner, org, member, OrgRole::Member)
            .await
            .unwrap(),
    );
    let mut teams = Vec::new();
    for slug in ["team_cancel_a", "team_cancel_b"] {
        let team = applied(
            orgs::create_organization_team(
                pool,
                owner,
                org,
                &orgs::NewTeam::new(
                    slug,
                    orgs::TeamProfile::new(slug, None, orgs::TeamPrivacy::Visible).unwrap(),
                )
                .unwrap(),
            )
            .await
            .unwrap(),
        )
        .id;
        applied(
            orgs::add_organization_team_member(
                pool,
                owner,
                org,
                team,
                member,
                orgs::TeamRole::Member,
            )
            .await
            .unwrap(),
        );
        let repo = database.repo(owner, Some(org), slug).await;
        applied(
            orgs::grant_organization_team_repository(
                pool,
                owner,
                org,
                team,
                repo.id,
                orgs::TeamRepositoryPermission::new(3).unwrap(),
            )
            .await
            .unwrap(),
        );
        teams.push((team, repo.id));
    }
    let (team_a, repo_a) = teams[0];
    let (_, repo_b) = teams[1];
    let a_running = enqueue(pool, member, repo_a, false, &[member])
        .await
        .unwrap();
    let claimed = db::ci::claim_ci_run(pool, "team-worker", "local", &[member])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, a_running.id);
    let b_queued = enqueue(pool, member, repo_b, false, &[member])
        .await
        .unwrap();
    applied(
        orgs::remove_organization_team_member(pool, owner, org, team_a, member)
            .await
            .unwrap(),
    );
    let running = db::ci::get_ci_run_by_run_id(pool, a_running.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(running.status, "running");
    assert!(
        running.cancel_requested,
        "running team A job must receive a stop request"
    );
    assert_eq!(
        db::ci::get_ci_run_by_run_id(pool, b_queued.run_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "queued"
    );

    applied(
        orgs::add_organization_team_member(
            pool,
            owner,
            org,
            team_a,
            member,
            orgs::TeamRole::Member,
        )
        .await
        .unwrap(),
    );
    let a_queued = enqueue(pool, member, repo_a, false, &[member])
        .await
        .unwrap();
    let b_other = enqueue(pool, member, repo_b, false, &[member])
        .await
        .unwrap();
    applied(
        orgs::delete_organization_team(pool, owner, org, team_a)
            .await
            .unwrap(),
    );
    assert_eq!(
        db::ci::get_ci_run_by_run_id(pool, a_queued.run_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "cancelled"
    );
    for run in [&b_queued, &b_other] {
        let current = db::ci::get_ci_run_by_run_id(pool, run.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            current.status, "queued",
            "unrelated team repository must keep its job"
        );
        assert!(!current.cancel_requested);
    }
    // Removing the org membership, unlike removing a team, ends all org jobs.
    applied(
        orgs::remove_organization_member(pool, owner, org, member)
            .await
            .unwrap(),
    );
    for run in [&b_queued, &b_other] {
        assert_eq!(
            db::ci::get_ci_run_by_run_id(pool, run.run_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "cancelled"
        );
    }
    database.close().await;
}

#[tokio::test]
async fn postgres_team_cancellation_stays_with_its_repositories() {
    team_cancellation_stays_with_its_repositories(true).await;
}

#[tokio::test]
async fn sqlite_team_cancellation_stays_with_its_repositories() {
    team_cancellation_stays_with_its_repositories(false).await;
}

#[tokio::test]
async fn postgres_organization_write_authority() {
    authority_contract(true).await;
}
#[tokio::test]
async fn sqlite_organization_write_authority() {
    authority_contract(false).await;
}

// Observe the actual server lock wait, not a sleep-based claim of concurrency.
