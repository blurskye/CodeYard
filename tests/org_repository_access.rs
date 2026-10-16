//! Repository read access through organization roles, teams and direct grants,
//! on a disposable SQLite file and an owned gitserv_test PostgreSQL schema.
//! Setup failure fails the test; nothing is skipped.
#![cfg(not(target_arch = "wasm32"))]

#[path = "support/native_database.rs"]
mod native_database;
#[path = "support/production_ci.rs"]
mod production_ci;

use codeyard::backend::db::{
    self,
    models::{AccessLevel, OrgRole},
    organizations::{self, OrganizationOutcome},
};

// Reuse the existing guarded resources/cleanup rather than another fixture owner.
enum Database {
    Postgres(Box<production_ci::Fixture>),
    Sqlite(native_database::NativeDatabase),
}
impl Database {
    fn pool(&self) -> &db::DbPool {
        match self {
            Self::Postgres(fixture) => &fixture.pool,
            Self::Sqlite(fixture) => &fixture.pool,
        }
    }
    async fn execute(&self, sql: &str, ids: &[i64]) {
        // Numbered $ parameters work on both native drivers. Corruption is
        // fixture-only; all effective access assertions use the public facade.
        match self {
            Self::Postgres(fixture) => {
                let mut query = sqlx::query(sql);
                for id in ids {
                    query = query.bind(*id);
                }
                query
                    .execute(&fixture.raw)
                    .await
                    .expect("owned PG mutation");
            }
            Self::Sqlite(fixture) => {
                let mut query = sqlx::query(sql);
                for id in ids {
                    query = query.bind(*id);
                }
                query
                    .execute(&fixture.raw)
                    .await
                    .expect("owned SQLite mutation");
            }
        }
    }
    async fn actor(&self, name: &str) -> i64 {
        db::users::accounts::create_user(self.pool(), name, None)
            .await
            .unwrap()
            .id
    }
    async fn org(&self, owner: i64, name: &str) -> i64 {
        applied(
            organizations::create_organization(
                self.pool(),
                owner,
                &organizations::NewOrganization::new(
                    name,
                    organizations::OrganizationProfile::new(None, None).unwrap(),
                )
                .unwrap(),
            )
            .await
            .unwrap(),
        )
        .id
    }
    async fn member(&self, owner: i64, org: i64, actor: i64, role: OrgRole) {
        applied(
            organizations::add_organization_member(self.pool(), owner, org, actor, role)
                .await
                .unwrap(),
        );
    }
    async fn team(
        &self,
        owner: i64,
        org: i64,
        slug: &str,
        privacy: organizations::TeamPrivacy,
    ) -> i64 {
        applied(
            organizations::create_organization_team(
                self.pool(),
                owner,
                org,
                &organizations::NewTeam::new(
                    slug,
                    organizations::TeamProfile::new("Access fixture", None, privacy).unwrap(),
                )
                .unwrap(),
            )
            .await
            .unwrap(),
        )
        .id
    }
    async fn team_member(&self, owner: i64, org: i64, team: i64, actor: i64) {
        applied(
            organizations::add_organization_team_member(
                self.pool(),
                owner,
                org,
                team,
                actor,
                organizations::TeamRole::Member,
            )
            .await
            .unwrap(),
        );
    }
    async fn grant_team(&self, owner: i64, org: i64, team: i64, repo: i64, level: i16) {
        applied(
            organizations::grant_organization_team_repository(
                self.pool(),
                owner,
                org,
                team,
                repo,
                organizations::TeamRepositoryPermission::new(level).unwrap(),
            )
            .await
            .unwrap(),
        );
    }
    async fn repo(&self, owner: i64, org: Option<i64>, name: &str, private: bool) -> i64 {
        let repo = db::repositories::records::create_repository(
            self.pool(),
            owner,
            name,
            None,
            private,
            "/fixture/not-executed.git",
        )
        .await
        .unwrap();
        if let Some(org) = org {
            self.execute(
                "UPDATE repositories SET owner_kind='org',owner_user_id=NULL,owner_org_id=$1 WHERE id=$2",
                &[org, repo.id],
            )
            .await;
        }
        repo.id
    }
    async fn expect(&self, actor: i64, repo: i64, level: AccessLevel) {
        assert_eq!(
            db::repositories::access::get_user_repo_access(self.pool(), actor, repo)
                .await
                .unwrap(),
            level,
            "canonical actor={actor} repo={repo}"
        );
    }
    async fn close(&mut self) {
        match self {
            Self::Postgres(fixture) => fixture.finish().await,
            Self::Sqlite(fixture) => fixture.close().await,
        }
    }
}
fn applied<T: std::fmt::Debug>(result: OrganizationOutcome<T>) -> T {
    match result {
        OrganizationOutcome::Applied(value) => value,
        other => panic!("expected applied, got {other:?}"),
    }
}

async fn canonical_access_contract(database: &Database) {
    use organizations::TeamPrivacy::{Secret, Visible};
    use AccessLevel::{Admin, None as NoAccess, Owner, Read, Write};
    let pool = database.pool();
    let owner = database.actor("access_owner").await;
    let admin = database.actor("access_admin").await;
    let member = database.actor("access_member").await;
    let outsider = database.actor("access_outsider").await;
    let site_admin = database.actor("access_site_admin").await;
    let foreign = database.actor("access_foreign").await;
    database
        .execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[site_admin])
        .await;
    let org = database.org(owner, "access_org").await;
    let foreign_org = database.org(foreign, "access_foreign_org").await;
    database.member(owner, org, admin, OrgRole::Admin).await;
    database.member(owner, org, member, OrgRole::Member).await;
    let secret = database.team(owner, org, "access_secret", Secret).await;
    let visible = database.team(owner, org, "access_visible", Visible).await;
    let foreign_team = database
        .team(foreign, foreign_org, "access_foreign_team", Visible)
        .await;
    let private = database
        .repo(owner, Some(org), "access_private", true)
        .await;
    let public = database
        .repo(owner, Some(org), "access_public", false)
        .await;
    let personal = database.repo(owner, None, "access_personal", true).await;
    let personal_public = database
        .repo(owner, None, "access_personal_public", false)
        .await;
    let foreign_repo = database
        .repo(foreign, Some(foreign_org), "access_foreign_repo", true)
        .await;

    database.expect(owner, private, Owner).await;
    database.expect(admin, private, Admin).await;
    database.expect(owner, public, Owner).await;
    database.expect(admin, public, Admin).await;
    database.expect(member, private, NoAccess).await;
    for actor in [outsider, site_admin, foreign] {
        database.expect(actor, private, NoAccess).await;
        database.expect(actor, public, Read).await;
        database.expect(actor, personal_public, Read).await;
        database.expect(actor, 900_000, NoAccess).await;
    }
    database.expect(900_000, public, NoAccess).await;
    database.expect(member, foreign_repo, NoAccess).await;
    database.expect(owner, personal, Owner).await;

    // Visibility or org membership alone never supplies private repository READ.
    database.grant_team(owner, org, secret, private, 3).await;
    database.expect(member, private, NoAccess).await;
    database.team_member(owner, org, secret, member).await;
    for level in 1..=5 {
        database
            .grant_team(owner, org, secret, private, level)
            .await;
        database
            .expect(member, private, AccessLevel::from(level))
            .await;
    }
    database.grant_team(owner, org, secret, private, 3).await;
    database.team_member(owner, org, visible, member).await;
    database.grant_team(owner, org, visible, private, 5).await;
    database.expect(member, private, Admin).await;
    applied(
        organizations::revoke_organization_team_repository(pool, owner, org, visible, private)
            .await
            .unwrap(),
    );
    database.expect(member, private, Write).await;
    applied(
        organizations::remove_organization_team_member(pool, owner, org, secret, member)
            .await
            .unwrap(),
    );
    database.expect(member, private, NoAccess).await;
    database.team_member(owner, org, secret, member).await;

    // Direct and org contributions combine by maximum, not short-circuit order.
    db::repositories::access::grant_repo_access(pool, member, private, Read)
        .await
        .unwrap();
    database.expect(member, private, Write).await;
    db::repositories::access::grant_repo_access(pool, member, private, Admin)
        .await
        .unwrap();
    database.expect(member, private, Admin).await;
    assert!(
        db::repositories::access::revoke_repo_access(pool, member, private)
            .await
            .unwrap()
    );
    database.expect(member, private, Write).await;
    for level in 1..=5 {
        db::repositories::access::grant_repo_access(
            pool,
            outsider,
            private,
            AccessLevel::from(level),
        )
        .await
        .unwrap();
        database
            .expect(outsider, private, AccessLevel::from(level))
            .await;
    }
    assert!(
        db::repositories::access::revoke_repo_access(pool, outsider, private)
            .await
            .unwrap()
    );
    database.expect(outsider, private, NoAccess).await;

    // Persisted direct Owner is not a collaborator permission (schema defines
    // 0..=5); only correctly scoped personal/org ownership can supply Owner.
    db::repositories::access::grant_repo_access(pool, outsider, private, Read)
        .await
        .unwrap();
    db::repositories::access::grant_repo_access(pool, outsider, public, Read)
        .await
        .unwrap();
    for level in [-1, 0, 6, 7, 32767] {
        database
            .execute(
                "UPDATE repo_access SET access_level=$1 WHERE user_id=$2",
                &[level, outsider],
            )
            .await;
        database.expect(outsider, private, NoAccess).await;
        database.expect(outsider, public, Read).await;
        database
            .execute(
                "UPDATE team_repo_access SET permission=$1 WHERE team_id=$2",
                &[level, secret],
            )
            .await;
        database.expect(member, private, NoAccess).await;
    }
    database.grant_team(owner, org, secret, private, 3).await;
    db::repositories::access::revoke_repo_access(pool, outsider, private)
        .await
        .unwrap();
    db::repositories::access::revoke_repo_access(pool, outsider, public)
        .await
        .unwrap();
    for role in ["unknown", "OWNER", "member "] {
        database
            .execute(
                &format!("UPDATE org_memberships SET role='{role}' WHERE org_id=$1 AND user_id=$2"),
                &[org, member],
            )
            .await;
        database.expect(member, private, NoAccess).await;
        database.expect(member, public, Read).await;
    }
    database
        .execute(
            "UPDATE org_memberships SET role='member' WHERE org_id=$1 AND user_id=$2",
            &[org, member],
        )
        .await;
    for role in ["unknown", "owner", "MEMBER"] {
        database
            .execute(
                &format!(
                    "UPDATE team_memberships SET role='{role}' WHERE team_id=$1 AND user_id=$2"
                ),
                &[secret, member],
            )
            .await;
        database.expect(member, private, NoAccess).await;
    }
    database
        .execute(
            "UPDATE team_memberships SET role='maintainer' WHERE team_id=$1 AND user_id=$2",
            &[secret, member],
        )
        .await;
    database.expect(member, private, Write).await;
    database
        .execute("UPDATE teams SET privacy='invalid' WHERE id=$1", &[secret])
        .await;
    database.expect(member, private, NoAccess).await;
    database
        .execute("UPDATE teams SET privacy='secret' WHERE id=$1", &[secret])
        .await;

    // Historical cross-org joins and hierarchy are never inherited or usable.
    database
        .execute(
            "INSERT INTO team_memberships(team_id,user_id,role) VALUES($1,$2,'member')",
            &[foreign_team, member],
        )
        .await;
    database
        .execute(
            "INSERT INTO team_repo_access(team_id,repo_id,permission) VALUES($1,$2,5)",
            &[foreign_team, private],
        )
        .await;
    database.expect(member, private, Write).await;
    for parent in [visible, foreign_team, secret] {
        database
            .execute(
                "UPDATE teams SET parent_team_id=$1 WHERE id=$2",
                &[parent, secret],
            )
            .await;
        database.expect(member, private, NoAccess).await;
    }
    database
        .execute(
            "UPDATE teams SET parent_team_id=NULL WHERE id=$1",
            &[secret],
        )
        .await;
    // Orphaned legacy team membership (absent same-org membership) denies too.
    database
        .execute(
            "DELETE FROM org_memberships WHERE org_id=$1 AND user_id=$2",
            &[org, member],
        )
        .await;
    database.expect(member, private, NoAccess).await;
    database.member(owner, org, member, OrgRole::Member).await;
    database.expect(member, private, Write).await;

    // Actor restrictions beat every source, including public and personal Owner.
    db::repositories::access::grant_repo_access(pool, outsider, private, Admin)
        .await
        .unwrap();
    for actor in [owner, admin, member, outsider, site_admin] {
        for flag in ["is_banned", "must_change_password"] {
            database
                .execute(
                    &format!("UPDATE users SET {flag}=TRUE WHERE id=$1"),
                    &[actor],
                )
                .await;
            for repo in [private, public, personal, personal_public] {
                database.expect(actor, repo, NoAccess).await;
            }
            database
                .execute(
                    &format!("UPDATE users SET {flag}=FALSE WHERE id=$1"),
                    &[actor],
                )
                .await;
        }
    }

    // No usable owner caps ALL derived authority, even a direct nonmember grant.
    for flag in ["is_banned", "must_change_password"] {
        database
            .execute(
                &format!("UPDATE users SET {flag}=TRUE WHERE id=$1"),
                &[owner],
            )
            .await;
        for actor in [admin, member, outsider] {
            database.expect(actor, private, Read).await;
            database.expect(actor, public, Read).await;
        }
        database.expect(site_admin, private, NoAccess).await;
        database.expect(site_admin, public, Read).await;
        database.expect(foreign, foreign_repo, Owner).await;
        database
            .execute(
                &format!("UPDATE users SET {flag}=FALSE WHERE id=$1"),
                &[owner],
            )
            .await;
    }
    for role in ["unknown", "member"] {
        database
            .execute(
                &format!("UPDATE org_memberships SET role='{role}' WHERE org_id=$1 AND user_id=$2"),
                &[org, owner],
            )
            .await;
        for actor in [admin, member, outsider] {
            database.expect(actor, private, Read).await;
        }
        database.expect(owner, personal, Owner).await;
        database.expect(outsider, personal_public, Read).await;
    }
    database
        .execute(
            "DELETE FROM org_memberships WHERE org_id=$1 AND user_id=$2",
            &[org, owner],
        )
        .await;
    database.expect(outsider, private, Read).await;
    database
        .execute(
            "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'owner')",
            &[org, owner],
        )
        .await;
    database.expect(owner, private, Owner).await;
    database.expect(admin, private, Admin).await;
    database.expect(member, private, Write).await;
    database.expect(outsider, private, Admin).await;

    // A second usable owner keeps governance live when the original is banned.
    database.member(owner, org, foreign, OrgRole::Owner).await;
    database
        .execute("UPDATE users SET is_banned=TRUE WHERE id=$1", &[owner])
        .await;
    database.expect(foreign, private, Owner).await;
    database.expect(outsider, private, Admin).await;
    database
        .execute("UPDATE users SET is_banned=FALSE WHERE id=$1", &[owner])
        .await;

    // Real DAO member removal clears both scoped team and direct contributions.
    db::repositories::access::grant_repo_access(pool, member, private, Admin)
        .await
        .unwrap();
    applied(
        organizations::remove_organization_member(pool, owner, org, member)
            .await
            .unwrap(),
    );
    database.expect(member, private, NoAccess).await;
    database.expect(member, public, Read).await;
    db::repositories::access::grant_repo_access(pool, member, private, Read)
        .await
        .unwrap();
    database.expect(member, private, Read).await;
    assert!(
        db::repositories::access::revoke_repo_access(pool, member, private)
            .await
            .unwrap()
    );
    database.expect(member, private, NoAccess).await;

    // Numeric identity equality is not ownership across user/org namespaces.
    database
        .execute(
            "INSERT INTO users(id,username) VALUES(700000,'access_collision_user')",
            &[],
        )
        .await;
    database
        .execute(
            "INSERT INTO organizations(id,name) VALUES(700000,'access_collision_org')",
            &[],
        )
        .await;
    database
        .execute(
            "INSERT INTO org_memberships(org_id,user_id,role) VALUES(700000,$1,'owner')",
            &[owner],
        )
        .await;
    let collision = database
        .repo(owner, Some(700_000), "access_collision", true)
        .await;
    let collision_personal = database
        .repo(700_000, None, "access_collision_personal", true)
        .await;
    database.expect(700_000, collision, NoAccess).await;
    database.expect(owner, collision, Owner).await;
    database.expect(700_000, collision_personal, Owner).await;
    database.expect(owner, collision_personal, NoAccess).await;
    db::repositories::access::grant_repo_access(pool, outsider, collision_personal, Write)
        .await
        .unwrap();
    database.expect(outsider, collision_personal, Write).await;
    database
        .execute(
            "UPDATE org_memberships SET role='member' WHERE org_id=700000",
            &[],
        )
        .await;
    database.expect(owner, collision, NoAccess).await;
    database.expect(outsider, collision_personal, Write).await;
    database.expect(700_000, collision_personal, Owner).await;

    // A deleted repository grants nothing to anyone.
    database
        .execute("DELETE FROM repositories WHERE id=$1", &[private])
        .await;
    for actor in [owner, admin, outsider] {
        database.expect(actor, private, NoAccess).await;
    }
}

#[tokio::test]
async fn postgres_canonical_organization_repository_access() {
    let mut database = Database::Postgres(Box::new(production_ci::Fixture::new().await));
    canonical_access_contract(&database).await;
    database.close().await;
}

#[tokio::test]
async fn sqlite_canonical_organization_repository_access() {
    let mut database = Database::Sqlite(native_database::NativeDatabase::new().await);
    canonical_access_contract(&database).await;
    database.close().await;
}
