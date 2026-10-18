//! Organization authority contracts on disposable PostgreSQL and SQLite databases.
//! The PostgreSQL fixture needs GITSERV_TEST_DATABASE_URL and fails without it.
//! No ambient .env, private installation, service, executor or live accounts.
#![cfg(not(target_arch = "wasm32"))]

use crate::support::production_ci;
use codeyard::backend::db::{
    self,
    models::{OrgRole, Repository},
    organizations::*,
};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use std::{str::FromStr, time::Duration};
use OrganizationOutcome::{Applied, Conflict, Denied, Missing};

// Raw driver SQL is fixture-only corruption/observation, never product authority.
enum Raw {
    Postgres(sqlx::PgPool),
    Sqlite(sqlx::SqlitePool),
}
struct Database {
    pool: db::DbPool,
    raw: Raw,
    _pg: Option<production_ci::Fixture>,
    _sqlite: Option<tempfile::TempDir>,
}
impl Database {
    async fn postgres() -> Self {
        let pg = production_ci::Fixture::new().await;
        Self {
            pool: pg.pool.clone(),
            raw: Raw::Postgres(pg.raw.clone()),
            _pg: Some(pg),
            _sqlite: None,
        }
    }
    async fn sqlite() -> Self {
        let directory = tempfile::tempdir().expect("owned organization fixture");
        let url = format!(
            "sqlite://{}",
            directory.path().join("organizations.sqlite").display()
        );
        let raw = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(
                SqliteConnectOptions::from_str(&url)
                    .unwrap()
                    .create_if_missing(true)
                    .foreign_keys(true)
                    .journal_mode(SqliteJournalMode::Wal)
                    .busy_timeout(Duration::from_secs(3)),
            )
            .await
            .expect("owned SQLite connections");
        let pool: db::DbPool = raw.clone().into();
        db::run_migrations(&pool)
            .await
            .expect("required native SQLite migrations");
        Self {
            pool,
            raw: Raw::Sqlite(raw),
            _pg: None,
            _sqlite: Some(directory),
        }
    }
    async fn execute(&self, sql: &str, ids: &[i64]) {
        // SQLite supports numbered $ parameters too. Fixture SQL uses common types.
        match &self.raw {
            Raw::Postgres(pool) => {
                let mut query = sqlx::query(sql);
                for id in ids {
                    query = query.bind(*id);
                }
                query
                    .execute(pool)
                    .await
                    .expect("owned PG fixture mutation");
            }
            Raw::Sqlite(pool) => {
                let mut query = sqlx::query(sql);
                for id in ids {
                    query = query.bind(*id);
                }
                query
                    .execute(pool)
                    .await
                    .expect("owned SQLite fixture mutation");
            }
        }
    }
    async fn count(&self, sql: &str, ids: &[i64]) -> i64 {
        match &self.raw {
            Raw::Postgres(pool) => {
                let mut query = sqlx::query_scalar::<_, i64>(sql);
                for id in ids {
                    query = query.bind(*id);
                }
                query
                    .fetch_one(pool)
                    .await
                    .expect("owned PG fixture observation")
            }
            Raw::Sqlite(pool) => {
                let mut query = sqlx::query_scalar::<_, i64>(sql);
                for id in ids {
                    query = query.bind(*id);
                }
                query
                    .fetch_one(pool)
                    .await
                    .expect("owned SQLite fixture observation")
            }
        }
    }
    async fn run(&self, repo: i64, actor: i64, status: &str) {
        let sql = "INSERT INTO ci_runs(run_id,repo_id,triggered_by_id,mode,pipeline,event,branch,workflow_path,status) VALUES($1,$2,$3,'local','ci','workflow_dispatch','main','.github/workflows/ci.yml',$4)";
        match &self.raw {
            Raw::Postgres(pool) => {
                sqlx::query(sql)
                    .bind(uuid::Uuid::new_v4())
                    .bind(repo)
                    .bind(actor)
                    .bind(status)
                    .execute(pool)
                    .await
                    .unwrap();
            }
            Raw::Sqlite(pool) => {
                sqlx::query(sql)
                    .bind(uuid::Uuid::new_v4())
                    .bind(repo)
                    .bind(actor)
                    .bind(status)
                    .execute(pool)
                    .await
                    .unwrap();
            }
        }
    }
    /// What revocations did to `actor`'s runs in `repo` so far: queued runs are
    /// cancelled on the spot, running ones are flagged for their worker. Nothing
    /// may stay queued with only a flag.
    async fn stopped_runs(&self, repo: i64, actor: i64) -> (i64, i64) {
        assert_eq!(
            self.count(
                "SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND triggered_by_id=$2 AND status='queued'",
                &[repo, actor]
            )
            .await,
            0,
            "a revocation leaves no queued run behind"
        );
        let cancelled = self.count("SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND triggered_by_id=$2 AND status='cancelled' AND error_message='The person who started this run no longer has access'", &[repo, actor]).await;
        let flagged = self.count("SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND triggered_by_id=$2 AND status='running' AND cancel_requested", &[repo, actor]).await;
        (cancelled, flagged)
    }
    async fn actor(&self, name: &str) -> i64 {
        db::users::accounts::create_user(&self.pool, name, None)
            .await
            .unwrap()
            .id
    }
    async fn org(&self, actor: i64, slug: &str) -> OrganizationMetadata {
        applied(
            create_organization(
                &self.pool,
                actor,
                &NewOrganization::new(
                    slug,
                    OrganizationProfile::new(Some("Fixture"), Some("Safe description")).unwrap(),
                )
                .unwrap(),
            )
            .await
            .unwrap(),
        )
    }
    async fn team(
        &self,
        actor: i64,
        org: i64,
        slug: &str,
        privacy: TeamPrivacy,
    ) -> db::models::Team {
        applied(
            create_organization_team(
                &self.pool,
                actor,
                org,
                &NewTeam::new(
                    slug,
                    TeamProfile::new("Fixture team", None, privacy).unwrap(),
                )
                .unwrap(),
            )
            .await
            .unwrap(),
        )
    }
    async fn repo(&self, owner: i64, org: Option<i64>, name: &str) -> Repository {
        let repository = db::repositories::records::create_repository(
            &self.pool,
            owner,
            name,
            None,
            true,
            "/fixture/not-executed.git",
        )
        .await
        .unwrap();
        if let Some(org) = org {
            self.execute("UPDATE repositories SET owner_kind='org',owner_user_id=NULL,owner_org_id=$1 WHERE id=$2", &[org, repository.id]).await;
        }
        db::repositories::records::get_repository_by_id(&self.pool, repository.id)
            .await
            .unwrap()
            .unwrap()
    }
    async fn close(&mut self) {
        self.pool.close().await;
        match &self.raw {
            Raw::Postgres(pool) => pool.close().await,
            Raw::Sqlite(pool) => pool.close().await,
        }
        if let Some(pg) = &mut self._pg {
            pg.finish().await;
        }
    }
}
fn applied<T: std::fmt::Debug>(result: OrganizationOutcome<T>) -> T {
    match result {
        Applied(value) => value,
        other => panic!("expected applied, got {other:?}"),
    }
}
fn page() -> OrganizationPage {
    OrganizationPage::new(0, 100).unwrap()
}
fn profile() -> OrganizationProfile {
    OrganizationProfile::new(Some("Changed"), Some("description")).unwrap()
}
fn permission(level: i16) -> TeamRepositoryPermission {
    TeamRepositoryPermission::new(level).unwrap()
}

async fn atomic_creation_safe_metadata_namespace_and_validation(database: &Database) {
    let owner = database.actor("creation_owner").await;
    let organization = database.org(owner, "creation_org").await;
    assert_eq!(organization.name, "creation_org");
    assert!(!organization.is_verified);
    let roster = applied(
        list_organization_members(&database.pool, owner, organization.id, page())
            .await
            .unwrap(),
    );
    assert_eq!(roster.len(), 1);
    assert_eq!(roster[0].user_id, owner);
    assert_eq!(roster[0].role, "owner");
    let duplicate = NewOrganization::new("creation_org", profile()).unwrap();
    assert!(matches!(
        create_organization(&database.pool, owner, &duplicate)
            .await
            .unwrap(),
        Conflict
    ));
    let user_collision = NewOrganization::new("creation_owner", profile()).unwrap();
    assert!(matches!(
        create_organization(&database.pool, owner, &user_collision)
            .await
            .unwrap(),
        Conflict
    ));
    // Exact current username case policy is preserved, not silently lowercased.
    database.org(owner, "Creation_owner").await;
    database.execute("UPDATE organizations SET billing_email='private@example.test',is_verified=TRUE WHERE id=$1", &[organization.id]).await;
    let safe = applied(
        update_organization_profile(&database.pool, owner, organization.id, &profile())
            .await
            .unwrap(),
    );
    assert!(safe.is_verified);
    assert_eq!(safe.name, "creation_org");
    let json = serde_json::to_string(&safe).unwrap();
    assert!(!json.contains("billing") && !json.contains("private@example.test"));
    assert_eq!(database.count("SELECT COUNT(*) FROM organizations WHERE id=$1 AND billing_email='private@example.test' AND is_verified", &[organization.id]).await, 1);
    assert!(NewOrganization::new("bad/name", profile()).is_err());
    assert!(NewOrganization::new("ab", profile()).is_err());
    assert!(NewOrganization::new(&"a".repeat(256), profile()).is_err());
    assert!(NewOrganization::new("éé", profile()).is_ok());
    assert!(OrganizationProfile::new(Some(" "), None).is_err());
    assert!(OrganizationProfile::new(Some(&"a".repeat(256)), None).is_err());
    assert!(OrganizationProfile::new(None, Some(&"a".repeat(4097))).is_err());
    assert!(parse_organization_role("unknown").is_err());
    assert!(TeamRole::parse("owner").is_err());
    assert!(TeamPrivacy::parse("unknown").is_err());
    for level in [-1, 0, 6, 32767] {
        assert!(TeamRepositoryPermission::new(level).is_err());
    }
    for level in 1..=5 {
        assert!(TeamRepositoryPermission::new(level).is_ok());
    }
    assert!(OrganizationPage::new(-1, 10).is_err());
    assert!(OrganizationPage::new(0, 101).is_err());
    assert!(matches!(
        get_organization_metadata(&database.pool, owner, 900_000)
            .await
            .unwrap(),
        Missing
    ));
}

async fn owner_admin_member_rules_and_inactive_actors(database: &Database) {
    let owner = database.actor("roles_owner").await;
    let admin = database.actor("roles_admin").await;
    let member = database.actor("roles_member").await;
    let outsider = database.actor("roles_site_admin").await;
    database
        .execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[outsider])
        .await;
    let org = database.org(owner, "roles_org").await;
    applied(
        add_organization_member(&database.pool, owner, org.id, admin, OrgRole::Admin)
            .await
            .unwrap(),
    );
    applied(
        add_organization_member(&database.pool, admin, org.id, member, OrgRole::Member)
            .await
            .unwrap(),
    );
    assert!(matches!(
        update_organization_profile(&database.pool, outsider, org.id, &profile())
            .await
            .unwrap(),
        Denied
    ));
    assert!(matches!(
        update_organization_profile(&database.pool, member, org.id, &profile())
            .await
            .unwrap(),
        Denied
    ));
    applied(
        update_organization_profile(&database.pool, admin, org.id, &profile())
            .await
            .unwrap(),
    );
    assert!(matches!(
        add_organization_member(&database.pool, member, org.id, outsider, OrgRole::Member)
            .await
            .unwrap(),
        Denied
    ));
    assert!(matches!(
        update_organization_member_role(&database.pool, admin, org.id, member, OrgRole::Admin)
            .await
            .unwrap(),
        Denied
    ));
    assert!(matches!(
        update_organization_member_role(&database.pool, admin, org.id, owner, OrgRole::Member)
            .await
            .unwrap(),
        Denied
    ));
    assert!(matches!(
        remove_organization_member(&database.pool, admin, org.id, admin)
            .await
            .unwrap(),
        Denied
    ));
    assert!(matches!(
        remove_organization_member(&database.pool, admin, org.id, owner)
            .await
            .unwrap(),
        Denied
    ));
    applied(
        update_organization_member_role(&database.pool, owner, org.id, member, OrgRole::Admin)
            .await
            .unwrap(),
    );
    assert!(matches!(
        remove_organization_member(&database.pool, admin, org.id, member)
            .await
            .unwrap(),
        Denied
    ));
    applied(
        update_organization_member_role(&database.pool, owner, org.id, member, OrgRole::Member)
            .await
            .unwrap(),
    );
    for flag in ["is_banned", "must_change_password"] {
        database
            .execute(
                &format!("UPDATE users SET {flag}=TRUE WHERE id=$1"),
                &[admin],
            )
            .await;
        assert!(matches!(
            update_organization_profile(&database.pool, admin, org.id, &profile())
                .await
                .unwrap(),
            Denied
        ));
        assert!(matches!(
            add_organization_member(&database.pool, admin, org.id, outsider, OrgRole::Member)
                .await
                .unwrap(),
            Denied
        ));
        let new = NewOrganization::new("inactive_org", profile()).unwrap();
        assert!(matches!(
            create_organization(&database.pool, admin, &new)
                .await
                .unwrap(),
            Denied
        ));
        database
            .execute(
                &format!("UPDATE users SET {flag}=FALSE WHERE id=$1"),
                &[admin],
            )
            .await;
    }
    assert!(matches!(
        update_organization_profile(&database.pool, 900_000, org.id, &profile())
            .await
            .unwrap(),
        Denied
    ));
    assert!(matches!(
        add_organization_member(&database.pool, owner, org.id, 900_000, OrgRole::Member)
            .await
            .unwrap(),
        Denied
    ));
    let orgs = applied(
        list_actor_organizations(&database.pool, member, page())
            .await
            .unwrap(),
    );
    assert!(orgs.iter().any(|row| row.id == org.id));
    assert!(applied(
        list_actor_organizations(&database.pool, outsider, page())
            .await
            .unwrap()
    )
    .is_empty());
}

async fn concurrent_last_owner_mutations(database: &Database) {
    let first = database.actor("race_first").await;
    let second = database.actor("race_second").await;
    let org = database.org(first, "race_org").await;
    assert!(matches!(
        remove_organization_member(&database.pool, first, org.id, first)
            .await
            .unwrap(),
        Conflict
    ));
    assert!(matches!(
        update_organization_member_role(&database.pool, first, org.id, first, OrgRole::Member)
            .await
            .unwrap(),
        Conflict
    ));
    applied(
        add_organization_member(&database.pool, first, org.id, second, OrgRole::Owner)
            .await
            .unwrap(),
    );
    // Each self-demotion is individually permitted with two owners; serialization
    // must make exactly one commit and leave a durable owner.
    let (a, b) = tokio::join!(
        update_organization_member_role(&database.pool, first, org.id, first, OrgRole::Member),
        update_organization_member_role(&database.pool, second, org.id, second, OrgRole::Member),
    );
    let a = a.unwrap();
    let b = b.unwrap();
    assert_eq!(
        usize::from(matches!(a, Applied(_))) + usize::from(matches!(b, Applied(_))),
        1
    );
    assert_eq!(
        usize::from(matches!(a, Conflict)) + usize::from(matches!(b, Conflict)),
        1
    );
    assert_eq!(
        database
            .count(
                "SELECT COUNT(*) FROM org_memberships WHERE org_id=$1 AND role='owner'",
                &[org.id]
            )
            .await,
        1
    );
    let owner = if matches!(a, Applied(_)) {
        second
    } else {
        first
    };
    let other = if owner == first { second } else { first };
    applied(
        update_organization_member_role(&database.pool, owner, org.id, other, OrgRole::Owner)
            .await
            .unwrap(),
    );
    // Concurrent self-removals also leave exactly one owner and one membership.
    let (a, b) = tokio::join!(
        remove_organization_member(&database.pool, first, org.id, first),
        remove_organization_member(&database.pool, second, org.id, second),
    );
    let a = a.unwrap();
    let b = b.unwrap();
    assert_eq!(
        usize::from(matches!(a, Applied(_))) + usize::from(matches!(b, Applied(_))),
        1
    );
    assert_eq!(
        usize::from(matches!(a, Conflict)) + usize::from(matches!(b, Conflict)),
        1
    );
    assert_eq!(
        database
            .count(
                "SELECT COUNT(*) FROM org_memberships WHERE org_id=$1 AND role='owner'",
                &[org.id]
            )
            .await,
        1
    );
}

async fn same_org_teams_secret_visibility_and_permissions(database: &Database) {
    let owner = database.actor("teams_owner").await;
    let member = database.actor("teams_member").await;
    let other = database.actor("teams_other").await;
    let foreign = database.actor("teams_foreign").await;
    let org = database.org(owner, "teams_org").await;
    let foreign_org = database.org(foreign, "teams_foreign_org").await;
    applied(
        add_organization_member(&database.pool, owner, org.id, member, OrgRole::Member)
            .await
            .unwrap(),
    );
    applied(
        add_organization_member(&database.pool, owner, org.id, other, OrgRole::Member)
            .await
            .unwrap(),
    );
    let visible = database
        .team(owner, org.id, "visible_team", TeamPrivacy::Visible)
        .await;
    let secret = database
        .team(owner, org.id, "secret_team", TeamPrivacy::Secret)
        .await;
    let foreign_team = database
        .team(
            foreign,
            foreign_org.id,
            "foreign_team",
            TeamPrivacy::Visible,
        )
        .await;
    let repo = database.repo(owner, Some(org.id), "team_repo").await;
    let foreign_repo = database
        .repo(foreign, Some(foreign_org.id), "foreign_repo")
        .await;
    assert!(matches!(
        add_organization_team_member(
            &database.pool,
            owner,
            org.id,
            secret.id,
            foreign,
            TeamRole::Member
        )
        .await
        .unwrap(),
        Denied
    ));
    assert!(matches!(
        add_organization_team_member(
            &database.pool,
            owner,
            org.id,
            foreign_team.id,
            member,
            TeamRole::Member
        )
        .await
        .unwrap(),
        Missing
    ));
    assert!(matches!(
        create_organization_team(
            &database.pool,
            foreign,
            org.id,
            &NewTeam::new(
                "unauthorized",
                TeamProfile::new("name", None, TeamPrivacy::Visible).unwrap()
            )
            .unwrap()
        )
        .await
        .unwrap(),
        Denied
    ));
    assert!(matches!(
        grant_organization_team_repository(
            &database.pool,
            owner,
            org.id,
            secret.id,
            foreign_repo.id,
            permission(3)
        )
        .await
        .unwrap(),
        Denied
    ));
    assert!(matches!(
        grant_organization_team_repository(
            &database.pool,
            owner,
            org.id,
            foreign_team.id,
            repo.id,
            permission(3)
        )
        .await
        .unwrap(),
        Missing
    ));
    applied(
        add_organization_team_member(
            &database.pool,
            owner,
            org.id,
            secret.id,
            member,
            TeamRole::Maintainer,
        )
        .await
        .unwrap(),
    );
    applied(
        grant_organization_team_repository(
            &database.pool,
            owner,
            org.id,
            secret.id,
            repo.id,
            permission(3),
        )
        .await
        .unwrap(),
    );
    assert_eq!(
        db::organizations::get_organization_repo_access(&database.pool, member, &repo)
            .await
            .unwrap(),
        db::models::AccessLevel::Write
    );
    for level in 1..=5 {
        applied(
            grant_organization_team_repository(
                &database.pool,
                owner,
                org.id,
                secret.id,
                repo.id,
                permission(level),
            )
            .await
            .unwrap(),
        );
        assert_eq!(
            get_organization_repo_access(&database.pool, member, &repo)
                .await
                .unwrap(),
            db::models::AccessLevel::from(level)
        );
    }
    applied(
        grant_organization_team_repository(
            &database.pool,
            owner,
            org.id,
            secret.id,
            repo.id,
            permission(3),
        )
        .await
        .unwrap(),
    );
    applied(
        add_organization_team_member(
            &database.pool,
            owner,
            org.id,
            visible.id,
            member,
            TeamRole::Member,
        )
        .await
        .unwrap(),
    );
    applied(
        grant_organization_team_repository(
            &database.pool,
            owner,
            org.id,
            visible.id,
            repo.id,
            permission(5),
        )
        .await
        .unwrap(),
    );
    assert_eq!(
        get_organization_repo_access(&database.pool, member, &repo)
            .await
            .unwrap(),
        db::models::AccessLevel::Admin,
        "maximum valid same-org team grant wins, never Owner"
    );
    applied(
        revoke_organization_team_repository(&database.pool, owner, org.id, visible.id, repo.id)
            .await
            .unwrap(),
    );
    assert_eq!(
        get_organization_repo_access(&database.pool, member, &repo)
            .await
            .unwrap(),
        db::models::AccessLevel::Write
    );
    applied(
        remove_organization_team_member(&database.pool, owner, org.id, visible.id, member)
            .await
            .unwrap(),
    );
    assert_eq!(
        db::organizations::get_organization_repo_access(&database.pool, other, &repo)
            .await
            .unwrap(),
        db::models::AccessLevel::None
    );
    assert_eq!(
        db::organizations::get_organization_repo_access(&database.pool, owner, &repo)
            .await
            .unwrap(),
        db::models::AccessLevel::Owner
    );
    assert!(matches!(
        list_organization_team_members(&database.pool, other, org.id, secret.id, page())
            .await
            .unwrap(),
        Denied
    ));
    assert!(matches!(
        list_organization_team_repositories(&database.pool, other, org.id, secret.id, page())
            .await
            .unwrap(),
        Denied
    ));
    assert_eq!(
        applied(
            list_organization_team_members(&database.pool, member, org.id, secret.id, page())
                .await
                .unwrap()
        )
        .len(),
        1
    );
    assert_eq!(
        applied(
            list_organization_team_repositories(&database.pool, member, org.id, secret.id, page())
                .await
                .unwrap()
        )
        .len(),
        1
    );
    let teams = applied(
        list_organization_teams(&database.pool, other, org.id, page())
            .await
            .unwrap(),
    );
    assert_eq!(teams.len(), 1);
    assert_eq!(teams[0].team.id, visible.id);
    assert_eq!(
        applied(
            list_organization_teams(&database.pool, member, org.id, page())
                .await
                .unwrap()
        )
        .len(),
        2
    );
    let limited = applied(
        list_organization_teams(
            &database.pool,
            member,
            org.id,
            OrganizationPage::new(0, 1).unwrap(),
        )
        .await
        .unwrap(),
    );
    assert_eq!(limited.len(), 1);
    let next = applied(
        list_organization_teams(
            &database.pool,
            member,
            org.id,
            OrganizationPage::new(limited[0].team.id, 1).unwrap(),
        )
        .await
        .unwrap(),
    );
    assert_eq!(next.len(), 1);
    assert_ne!(limited[0].team.id, next[0].team.id);
    assert!(applied(
        list_organization_team_members(&database.pool, other, org.id, visible.id, page())
            .await
            .unwrap()
    )
    .is_empty());
    // Team maintainer is a membership role, not org management/grant authority.
    assert!(matches!(
        grant_organization_team_repository(
            &database.pool,
            member,
            org.id,
            secret.id,
            repo.id,
            permission(5)
        )
        .await
        .unwrap(),
        Denied
    ));
    let changed = applied(
        update_organization_team(
            &database.pool,
            owner,
            org.id,
            visible.id,
            &TeamProfile::new("Changed", Some("team description"), TeamPrivacy::Secret).unwrap(),
        )
        .await
        .unwrap(),
    );
    assert_eq!(changed.slug, visible.slug);
    assert_eq!(changed.privacy, "secret");
    assert_eq!(
        get_organization_repo_access(&database.pool, other, &foreign_repo)
            .await
            .unwrap(),
        db::models::AccessLevel::None
    );
    // Active same-org member is required for ADDING, not exact scoped cleanup.
    database
        .execute("UPDATE users SET is_banned=TRUE WHERE id=$1", &[other])
        .await;
    assert!(matches!(
        add_organization_team_member(
            &database.pool,
            owner,
            org.id,
            secret.id,
            other,
            TeamRole::Member
        )
        .await
        .unwrap(),
        Denied
    ));
}

async fn invalid_persisted_roles_levels_and_joins_fail_closed(database: &Database) {
    let owner = database.actor("invalid_owner").await;
    let member = database.actor("invalid_member").await;
    let admin = database.actor("invalid_admin").await;
    let foreign = database.actor("invalid_foreign").await;
    let org = database.org(owner, "invalid_org").await;
    applied(
        add_organization_member(&database.pool, owner, org.id, admin, OrgRole::Admin)
            .await
            .unwrap(),
    );
    let foreign_org = database.org(foreign, "invalid_foreign_org").await;
    let repo = database.repo(owner, Some(org.id), "invalid_repo").await;
    let team = database
        .team(owner, org.id, "invalid_team", TeamPrivacy::Visible)
        .await;
    let foreign_team = database
        .team(
            foreign,
            foreign_org.id,
            "invalid_foreign_team",
            TeamPrivacy::Visible,
        )
        .await;
    applied(
        add_organization_member(&database.pool, owner, org.id, member, OrgRole::Member)
            .await
            .unwrap(),
    );
    applied(
        add_organization_team_member(
            &database.pool,
            owner,
            org.id,
            team.id,
            member,
            TeamRole::Member,
        )
        .await
        .unwrap(),
    );
    applied(
        grant_organization_team_repository(
            &database.pool,
            owner,
            org.id,
            team.id,
            repo.id,
            permission(5),
        )
        .await
        .unwrap(),
    );
    for role in ["unknown", "OWNER", "", "member "] {
        database
            .execute(
                &format!("UPDATE org_memberships SET role='{role}' WHERE org_id=$1 AND user_id=$2"),
                &[org.id, member],
            )
            .await;
        assert_eq!(
            get_organization_repo_access(&database.pool, member, &repo)
                .await
                .unwrap(),
            db::models::AccessLevel::None
        );
        assert!(matches!(
            get_organization_metadata(&database.pool, member, org.id)
                .await
                .unwrap(),
            Denied
        ));
        assert!(matches!(
            update_organization_profile(&database.pool, member, org.id, &profile())
                .await
                .unwrap(),
            Denied
        ));
        assert!(matches!(
            update_organization_member_role(&database.pool, admin, org.id, member, OrgRole::Member)
                .await
                .unwrap(),
            Denied
        ));
        assert!(matches!(
            remove_organization_member(&database.pool, admin, org.id, member)
                .await
                .unwrap(),
            Denied
        ));
    }
    database
        .execute(
            "UPDATE org_memberships SET role='member' WHERE org_id=$1 AND user_id=$2",
            &[org.id, member],
        )
        .await;
    database
        .execute(
            "UPDATE team_memberships SET role='unknown' WHERE team_id=$1 AND user_id=$2",
            &[team.id, member],
        )
        .await;
    assert_eq!(
        get_organization_repo_access(&database.pool, member, &repo)
            .await
            .unwrap(),
        db::models::AccessLevel::None
    );
    database
        .execute(
            "UPDATE team_memberships SET role='member' WHERE team_id=$1 AND user_id=$2",
            &[team.id, member],
        )
        .await;
    for level in [-1, 0, 6, 32767] {
        database
            .execute(
                "UPDATE team_repo_access SET permission=$1 WHERE team_id=$2 AND repo_id=$3",
                &[level, team.id, repo.id],
            )
            .await;
        assert_eq!(
            get_organization_repo_access(&database.pool, member, &repo)
                .await
                .unwrap(),
            db::models::AccessLevel::None
        );
    }
    // Corrupt cross-org memberships and grants never become authority.
    database
        .execute(
            "INSERT INTO team_memberships(team_id,user_id,role) VALUES($1,$2,'member')",
            &[foreign_team.id, member],
        )
        .await;
    database
        .execute(
            "INSERT INTO team_repo_access(team_id,repo_id,permission) VALUES($1,$2,5)",
            &[foreign_team.id, repo.id],
        )
        .await;
    assert_eq!(
        get_organization_repo_access(&database.pool, member, &repo)
            .await
            .unwrap(),
        db::models::AccessLevel::None
    );
    database
        .execute(
            "UPDATE team_repo_access SET permission=3 WHERE team_id=$1 AND repo_id=$2",
            &[team.id, repo.id],
        )
        .await;
    assert_eq!(
        get_organization_repo_access(&database.pool, member, &repo)
            .await
            .unwrap(),
        db::models::AccessLevel::Write
    );
    database
        .execute("UPDATE teams SET privacy='invalid' WHERE id=$1", &[team.id])
        .await;
    assert_eq!(
        get_organization_repo_access(&database.pool, member, &repo)
            .await
            .unwrap(),
        db::models::AccessLevel::None
    );
    database
        .execute("UPDATE teams SET privacy='secret' WHERE id=$1", &[team.id])
        .await;
    database
        .execute(
            "DELETE FROM org_memberships WHERE org_id=$1 AND user_id=$2",
            &[org.id, member],
        )
        .await;
    assert_eq!(
        get_organization_repo_access(&database.pool, member, &repo)
            .await
            .unwrap(),
        db::models::AccessLevel::None
    );
}

async fn revocation_cleanup_cancellation_downgrade_and_collaborator_regrant(database: &Database) {
    let owner = database.actor("revoke_owner").await;
    let member = database.actor("revoke_member").await;
    let foreign = database.actor("revoke_foreign").await;
    let org = database.org(owner, "revoke_org").await;
    let other_org = database.org(foreign, "revoke_other_org").await;
    let team = database
        .team(owner, org.id, "revoke_team", TeamPrivacy::Secret)
        .await;
    let other_team = database
        .team(
            foreign,
            other_org.id,
            "revoke_other_team",
            TeamPrivacy::Visible,
        )
        .await;
    let repo = database.repo(owner, Some(org.id), "revoke_repo").await;
    let other_repo = database
        .repo(foreign, Some(other_org.id), "revoke_other_repo")
        .await;
    let personal = database.repo(member, None, "revoke_personal").await;
    applied(
        add_organization_member(&database.pool, owner, org.id, member, OrgRole::Admin)
            .await
            .unwrap(),
    );
    assert_eq!(
        get_organization_repo_access(&database.pool, member, &repo)
            .await
            .unwrap(),
        db::models::AccessLevel::Admin
    );
    database.run(repo.id, member, "queued").await;
    database.run(repo.id, member, "running").await;
    database.run(repo.id, member, "success").await;
    database.run(other_repo.id, member, "running").await;
    database.run(repo.id, foreign, "running").await;
    applied(
        update_organization_member_role(&database.pool, owner, org.id, member, OrgRole::Member)
            .await
            .unwrap(),
    );
    assert_eq!(
        get_organization_repo_access(&database.pool, member, &repo)
            .await
            .unwrap(),
        db::models::AccessLevel::None
    );
    assert_eq!(database.stopped_runs(repo.id, member).await, (1, 1));
    // Restore flags solely to observe the later revocation path separately, and queue
    // another run for it to stop.
    database
        .execute(
            "UPDATE ci_runs SET cancel_requested=FALSE WHERE repo_id=$1 AND triggered_by_id=$2",
            &[repo.id, member],
        )
        .await;
    database.run(repo.id, member, "queued").await;
    applied(
        add_organization_team_member(
            &database.pool,
            owner,
            org.id,
            team.id,
            member,
            TeamRole::Member,
        )
        .await
        .unwrap(),
    );
    applied(
        grant_organization_team_repository(
            &database.pool,
            owner,
            org.id,
            team.id,
            repo.id,
            permission(5),
        )
        .await
        .unwrap(),
    );
    applied(
        grant_organization_team_repository(
            &database.pool,
            owner,
            org.id,
            team.id,
            repo.id,
            permission(1),
        )
        .await
        .unwrap(),
    );
    assert_eq!(database.stopped_runs(repo.id, member).await, (2, 1));
    database
        .execute(
            "UPDATE ci_runs SET cancel_requested=FALSE WHERE repo_id=$1 AND triggered_by_id=$2",
            &[repo.id, member],
        )
        .await;
    database.run(repo.id, member, "queued").await;
    applied(
        revoke_organization_team_repository(&database.pool, owner, org.id, team.id, repo.id)
            .await
            .unwrap(),
    );
    assert_eq!(
        get_organization_repo_access(&database.pool, member, &repo)
            .await
            .unwrap(),
        db::models::AccessLevel::None
    );
    assert_eq!(database.stopped_runs(repo.id, member).await, (3, 1));
    applied(
        grant_organization_team_repository(
            &database.pool,
            owner,
            org.id,
            team.id,
            repo.id,
            permission(3),
        )
        .await
        .unwrap(),
    );
    database
        .execute(
            "UPDATE ci_runs SET cancel_requested=FALSE WHERE repo_id=$1 AND triggered_by_id=$2",
            &[repo.id, member],
        )
        .await;
    database.run(repo.id, member, "queued").await;
    applied(
        remove_organization_team_member(&database.pool, owner, org.id, team.id, member)
            .await
            .unwrap(),
    );
    assert_eq!(
        get_organization_repo_access(&database.pool, member, &repo)
            .await
            .unwrap(),
        db::models::AccessLevel::None
    );
    assert_eq!(database.stopped_runs(repo.id, member).await, (4, 1));
    applied(
        add_organization_team_member(
            &database.pool,
            owner,
            org.id,
            team.id,
            member,
            TeamRole::Member,
        )
        .await
        .unwrap(),
    );
    applied(
        add_organization_member(
            &database.pool,
            foreign,
            other_org.id,
            member,
            OrgRole::Member,
        )
        .await
        .unwrap(),
    );
    applied(
        add_organization_team_member(
            &database.pool,
            foreign,
            other_org.id,
            other_team.id,
            member,
            TeamRole::Member,
        )
        .await
        .unwrap(),
    );
    db::repositories::access::grant_repo_access(
        &database.pool,
        member,
        repo.id,
        db::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    db::repositories::access::grant_repo_access(
        &database.pool,
        member,
        other_repo.id,
        db::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    database
        .execute(
            "UPDATE ci_runs SET cancel_requested=FALSE WHERE repo_id=$1 AND triggered_by_id=$2",
            &[repo.id, member],
        )
        .await;
    database.run(repo.id, member, "queued").await;
    applied(
        remove_organization_member(&database.pool, owner, org.id, member)
            .await
            .unwrap(),
    );
    assert_eq!(
        database
            .count(
                "SELECT COUNT(*) FROM org_memberships WHERE org_id=$1 AND user_id=$2",
                &[org.id, member]
            )
            .await,
        0
    );
    assert_eq!(
        database
            .count(
                "SELECT COUNT(*) FROM team_memberships WHERE team_id=$1 AND user_id=$2",
                &[team.id, member]
            )
            .await,
        0
    );
    assert_eq!(
        database
            .count(
                "SELECT COUNT(*) FROM repo_access WHERE repo_id=$1 AND user_id=$2",
                &[repo.id, member]
            )
            .await,
        0
    );
    assert_eq!(
        database
            .count(
                "SELECT COUNT(*) FROM team_memberships WHERE team_id=$1 AND user_id=$2",
                &[other_team.id, member]
            )
            .await,
        1
    );
    assert_eq!(
        database
            .count(
                "SELECT COUNT(*) FROM repo_access WHERE repo_id=$1 AND user_id=$2",
                &[other_repo.id, member]
            )
            .await,
        1
    );
    assert_eq!(database.stopped_runs(repo.id, member).await, (5, 1));
    assert_eq!(database.count("SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND triggered_by_id=$2 AND status='success' AND cancel_requested", &[repo.id, member]).await, 0);
    assert_eq!(
        database
            .count(
                "SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND cancel_requested",
                &[other_repo.id]
            )
            .await,
        0
    );
    assert_eq!(database.count("SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND triggered_by_id=$2 AND cancel_requested", &[repo.id, foreign]).await, 0);
    assert_eq!(
        db::repositories::access::get_user_repo_access(&database.pool, member, personal.id)
            .await
            .unwrap(),
        db::models::AccessLevel::Owner
    );
    assert_eq!(
        get_organization_repo_access(&database.pool, member, &personal)
            .await
            .unwrap(),
        db::models::AccessLevel::None
    );
    // A deliberate later independent collaborator grant remains possible.
    db::repositories::access::grant_repo_access(
        &database.pool,
        member,
        repo.id,
        db::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    assert_eq!(
        db::repositories::access::get_user_repo_access(&database.pool, member, repo.id)
            .await
            .unwrap(),
        db::models::AccessLevel::Read
    );
    assert_eq!(
        get_organization_repo_access(&database.pool, member, &repo)
            .await
            .unwrap(),
        db::models::AccessLevel::None
    );
}

async fn cancellation_failure_rolls_back_entire_membership_revocation(database: &Database) {
    let owner = database.actor("rollback_owner").await;
    let member = database.actor("rollback_member").await;
    let org = database.org(owner, "rollback_org").await;
    let team = database
        .team(owner, org.id, "rollback_team", TeamPrivacy::Visible)
        .await;
    let repo = database.repo(owner, Some(org.id), "rollback_repo").await;
    applied(
        grant_organization_team_repository(
            &database.pool,
            owner,
            org.id,
            team.id,
            repo.id,
            permission(3),
        )
        .await
        .unwrap(),
    );
    applied(
        add_organization_member(&database.pool, owner, org.id, member, OrgRole::Member)
            .await
            .unwrap(),
    );
    applied(
        add_organization_team_member(
            &database.pool,
            owner,
            org.id,
            team.id,
            member,
            TeamRole::Member,
        )
        .await
        .unwrap(),
    );
    db::repositories::access::grant_repo_access(
        &database.pool,
        member,
        repo.id,
        db::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    // A revocation cancels a queued run by finishing it and flags a running run.
    // A failure at either step has to undo the whole revocation. The triggers stay
    // in this disposable database and raise only while `org_fixture_failure` names
    // the column being updated, so they are created once and switched with data.
    database.run(repo.id, member, "queued").await;
    database.run(repo.id, member, "running").await;
    database
        .execute(
            "CREATE TABLE org_fixture_failure(column_name TEXT NOT NULL)",
            &[],
        )
        .await;
    if let Raw::Postgres(_) = &database.raw {
        database.execute("CREATE FUNCTION org_fixture_fail_cancel() RETURNS trigger AS $$ BEGIN IF EXISTS (SELECT 1 FROM org_fixture_failure WHERE column_name = TG_ARGV[0]) THEN RAISE EXCEPTION 'owned injected cancellation failure'; END IF; RETURN NEW; END; $$ LANGUAGE plpgsql", &[]).await;
    }
    for column in ["status", "cancel_requested"] {
        match &database.raw {
            Raw::Postgres(_) => {
                database.execute(&format!("CREATE TRIGGER org_fixture_fail_{column} BEFORE UPDATE OF {column} ON ci_runs FOR EACH ROW EXECUTE FUNCTION org_fixture_fail_cancel('{column}')"), &[]).await;
            }
            Raw::Sqlite(_) => {
                database.execute(&format!("CREATE TRIGGER org_fixture_fail_{column} BEFORE UPDATE OF {column} ON ci_runs WHEN EXISTS (SELECT 1 FROM org_fixture_failure WHERE column_name = '{column}') BEGIN SELECT RAISE(ABORT,'owned injected cancellation failure'); END"), &[]).await;
            }
        }
    }
    for column in ["status", "cancel_requested"] {
        database
            .execute(
                &format!("INSERT INTO org_fixture_failure(column_name) VALUES ('{column}')"),
                &[],
            )
            .await;
        assert!(
            delete_organization_team(&database.pool, owner, org.id, team.id)
                .await
                .is_err(),
            "{column}"
        );
        assert_eq!(
            database
                .count("SELECT COUNT(*) FROM teams WHERE id=$1", &[team.id])
                .await,
            1
        );
        assert_eq!(
            database
                .count(
                    "SELECT COUNT(*) FROM team_repo_access WHERE team_id=$1",
                    &[team.id]
                )
                .await,
            1
        );
        assert!(
            remove_organization_team_member(&database.pool, owner, org.id, team.id, member)
                .await
                .is_err(),
            "{column}"
        );
        assert!(
            remove_organization_member(&database.pool, owner, org.id, member)
                .await
                .is_err(),
            "internal cancellation failure must not become Applied/Denied ({column})"
        );
        // The transaction drops on error; observations must see ALL facts retained.
        assert_eq!(
            database
                .count(
                    "SELECT COUNT(*) FROM org_memberships WHERE org_id=$1 AND user_id=$2",
                    &[org.id, member]
                )
                .await,
            1
        );
        assert_eq!(
            database
                .count(
                    "SELECT COUNT(*) FROM team_memberships WHERE team_id=$1 AND user_id=$2",
                    &[team.id, member]
                )
                .await,
            1
        );
        assert_eq!(
            database
                .count(
                    "SELECT COUNT(*) FROM repo_access WHERE repo_id=$1 AND user_id=$2",
                    &[repo.id, member]
                )
                .await,
            1
        );
        // Neither run was touched: the queued one is still waiting, the running one unflagged.
        assert_eq!(
            database
                .count(
                    "SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND status='queued'",
                    &[repo.id]
                )
                .await,
            1
        );
        assert_eq!(
            database
                .count(
                    "SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND cancel_requested",
                    &[repo.id]
                )
                .await,
            0
        );
        database
            .execute("DELETE FROM org_fixture_failure", &[])
            .await;
    }
    applied(
        remove_organization_member(&database.pool, owner, org.id, member)
            .await
            .unwrap(),
    );
}

async fn numeric_user_org_collision_and_personal_owner_unchanged(database: &Database) {
    let owner = database.actor("collision_owner").await;
    database
        .execute(
            "INSERT INTO users(id,username) VALUES(700000,'collision_user')",
            &[],
        )
        .await;
    database
        .execute(
            "INSERT INTO organizations(id,name) VALUES(700000,'collision_org')",
            &[],
        )
        .await;
    database
        .execute(
            "INSERT INTO org_memberships(org_id,user_id,role) VALUES(700000,$1,'owner')",
            &[owner],
        )
        .await;
    let repository = database.repo(owner, Some(700000), "collision_repo").await;
    let personal = database.repo(700000, None, "collision_personal").await;
    assert_eq!(
        get_organization_repo_access(&database.pool, 700000, &repository)
            .await
            .unwrap(),
        db::models::AccessLevel::None
    );
    assert!(matches!(
        update_organization_profile(&database.pool, 700000, 700000, &profile())
            .await
            .unwrap(),
        Denied
    ));
    assert_eq!(
        get_organization_repo_access(&database.pool, owner, &repository)
            .await
            .unwrap(),
        db::models::AccessLevel::Owner
    );
    assert_eq!(
        get_organization_repo_access(&database.pool, 700000, &personal)
            .await
            .unwrap(),
        db::models::AccessLevel::None
    );
    assert_eq!(
        db::repositories::access::get_user_repo_access(&database.pool, 700000, personal.id)
            .await
            .unwrap(),
        db::models::AccessLevel::Owner
    );
    let mut malformed = repository.clone();
    malformed.owner_kind = "unknown".into();
    assert_eq!(
        get_organization_repo_access(&database.pool, owner, &malformed)
            .await
            .unwrap(),
        db::models::AccessLevel::None
    );
    malformed = repository.clone();
    malformed.owner_user_id = Some(owner);
    assert_eq!(
        get_organization_repo_access(&database.pool, owner, &malformed)
            .await
            .unwrap(),
        db::models::AccessLevel::None
    );
    malformed = repository.clone();
    malformed.owner_org_id = Some(900000);
    assert_eq!(
        get_organization_repo_access(&database.pool, owner, &malformed)
            .await
            .unwrap(),
        db::models::AccessLevel::None
    );
}

async fn visible_team_repositories_require_effective_read(database: &Database) {
    let owner = database.actor("read_owner").await;
    let reader = database.actor("read_outside_team").await;
    let admin = database.actor("read_admin").await;
    let foreign = database.actor("read_foreign").await;
    let org = database.org(owner, "read_org").await;
    let other_org = database.org(foreign, "read_foreign_org").await;
    applied(
        add_organization_member(&database.pool, owner, org.id, reader, OrgRole::Member)
            .await
            .unwrap(),
    );
    applied(
        add_organization_member(&database.pool, owner, org.id, admin, OrgRole::Admin)
            .await
            .unwrap(),
    );
    let visible = database
        .team(owner, org.id, "read_visible", TeamPrivacy::Visible)
        .await;
    let independent = database
        .team(owner, org.id, "read_independent", TeamPrivacy::Secret)
        .await;
    let foreign_team = database
        .team(
            foreign,
            other_org.id,
            "read_foreign_team",
            TeamPrivacy::Visible,
        )
        .await;
    let hidden = database.repo(owner, Some(org.id), "read_hidden").await;
    let public = database.repo(owner, Some(org.id), "read_public").await;
    let direct = database.repo(owner, Some(org.id), "read_direct").await;
    let other_team = database.repo(owner, Some(org.id), "read_other_team").await;
    let cross = database.repo(owner, Some(org.id), "read_cross").await;
    for repo in [&hidden, &public, &direct, &other_team, &cross] {
        applied(
            grant_organization_team_repository(
                &database.pool,
                owner,
                org.id,
                visible.id,
                repo.id,
                permission(3),
            )
            .await
            .unwrap(),
        );
    }
    // Visible roster/existence is not private repository READ, even at LIMIT 1.
    assert!(applied(
        list_organization_team_repositories(&database.pool, reader, org.id, visible.id, page())
            .await
            .unwrap()
    )
    .is_empty());
    assert!(applied(
        list_organization_team_members(&database.pool, reader, org.id, visible.id, page())
            .await
            .unwrap()
    )
    .is_empty());
    assert!(matches!(
        list_organization_team_repositories(&database.pool, foreign, org.id, visible.id, page())
            .await
            .unwrap(),
        Denied
    ));
    database
        .execute(
            "UPDATE repositories SET is_private=FALSE WHERE id=$1",
            &[public.id],
        )
        .await;
    db::repositories::access::grant_repo_access(
        &database.pool,
        reader,
        direct.id,
        db::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    applied(
        add_organization_team_member(
            &database.pool,
            owner,
            org.id,
            independent.id,
            reader,
            TeamRole::Member,
        )
        .await
        .unwrap(),
    );
    applied(
        grant_organization_team_repository(
            &database.pool,
            owner,
            org.id,
            independent.id,
            other_team.id,
            permission(1),
        )
        .await
        .unwrap(),
    );
    // Even a current foreign org membership cannot turn its team into authority.
    applied(
        add_organization_member(
            &database.pool,
            foreign,
            other_org.id,
            reader,
            OrgRole::Member,
        )
        .await
        .unwrap(),
    );
    applied(
        add_organization_team_member(
            &database.pool,
            foreign,
            other_org.id,
            foreign_team.id,
            reader,
            TeamRole::Member,
        )
        .await
        .unwrap(),
    );
    database
        .execute(
            "INSERT INTO team_repo_access(team_id,repo_id,permission) VALUES($1,$2,5)",
            &[foreign_team.id, cross.id],
        )
        .await;
    let rows = applied(
        list_organization_team_repositories(&database.pool, reader, org.id, visible.id, page())
            .await
            .unwrap(),
    );
    assert_eq!(
        rows.iter().map(|r| r.repo_id).collect::<Vec<_>>(),
        vec![public.id, direct.id, other_team.id]
    );
    let first = applied(
        list_organization_team_repositories(
            &database.pool,
            reader,
            org.id,
            visible.id,
            OrganizationPage::new(0, 1).unwrap(),
        )
        .await
        .unwrap(),
    );
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].repo_id, public.id);
    let next = applied(
        list_organization_team_repositories(
            &database.pool,
            reader,
            org.id,
            visible.id,
            OrganizationPage::new(first[0].id, 1).unwrap(),
        )
        .await
        .unwrap(),
    );
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].repo_id, direct.id);
    for actor in [owner, admin] {
        assert_eq!(
            applied(
                list_organization_team_repositories(
                    &database.pool,
                    actor,
                    org.id,
                    visible.id,
                    page()
                )
                .await
                .unwrap()
            )
            .len(),
            5
        );
    }
    for level in 1..=5 {
        db::repositories::access::grant_repo_access(
            &database.pool,
            reader,
            hidden.id,
            db::models::AccessLevel::from(level),
        )
        .await
        .unwrap();
        assert!(applied(
            list_organization_team_repositories(&database.pool, reader, org.id, visible.id, page())
                .await
                .unwrap()
        )
        .iter()
        .any(|r| r.repo_id == hidden.id));
    }
    for level in [-1, 0, 6, 32767] {
        database
            .execute(
                "UPDATE repo_access SET access_level=$1 WHERE repo_id=$2 AND user_id=$3",
                &[level, hidden.id, reader],
            )
            .await;
        assert!(!applied(
            list_organization_team_repositories(&database.pool, reader, org.id, visible.id, page())
                .await
                .unwrap()
        )
        .iter()
        .any(|r| r.repo_id == hidden.id));
    }
    for corrupt in [
        "UPDATE team_memberships SET role='unknown' WHERE team_id=$1",
        "UPDATE teams SET privacy='invalid' WHERE id=$1",
    ] {
        database.execute(corrupt, &[independent.id]).await;
        assert!(!applied(
            list_organization_team_repositories(&database.pool, reader, org.id, visible.id, page())
                .await
                .unwrap()
        )
        .iter()
        .any(|r| r.repo_id == other_team.id));
        database
            .execute(
                "UPDATE team_memberships SET role='member' WHERE team_id=$1",
                &[independent.id],
            )
            .await;
        database
            .execute(
                "UPDATE teams SET privacy='secret' WHERE id=$1",
                &[independent.id],
            )
            .await;
    }
    database
        .execute("UPDATE users SET is_banned=TRUE WHERE id=$1", &[reader])
        .await;
    assert!(matches!(
        list_organization_team_repositories(&database.pool, reader, org.id, visible.id, page())
            .await
            .unwrap(),
        Denied
    ));
}

async fn usable_owner_governance_and_inactive_cleanup(database: &Database) {
    let owner = database.actor("usable_owner").await;
    let inactive = database.actor("usable_inactive").await;
    let admin = database.actor("usable_admin").await;
    for flag in ["is_banned", "must_change_password"] {
        let org = database.org(owner, &format!("usable_{flag}")).await;
        applied(
            add_organization_member(&database.pool, owner, org.id, inactive, OrgRole::Owner)
                .await
                .unwrap(),
        );
        applied(
            add_organization_member(&database.pool, owner, org.id, admin, OrgRole::Admin)
                .await
                .unwrap(),
        );
        database
            .execute(
                &format!("UPDATE users SET {flag}=TRUE WHERE id=$1"),
                &[inactive],
            )
            .await;
        assert!(matches!(
            update_organization_member_role(&database.pool, owner, org.id, owner, OrgRole::Member)
                .await
                .unwrap(),
            Conflict
        ));
        assert!(matches!(
            remove_organization_member(&database.pool, owner, org.id, owner)
                .await
                .unwrap(),
            Conflict
        ));
        assert!(matches!(
            update_organization_member_role(
                &database.pool,
                inactive,
                org.id,
                inactive,
                OrgRole::Member
            )
            .await
            .unwrap(),
            Denied
        ));
        assert!(matches!(
            remove_organization_member(&database.pool, inactive, org.id, inactive)
                .await
                .unwrap(),
            Denied
        ));
        assert!(matches!(
            update_organization_member_role(
                &database.pool,
                admin,
                org.id,
                inactive,
                OrgRole::Member
            )
            .await
            .unwrap(),
            Denied
        ));
        assert!(matches!(
            remove_organization_member(&database.pool, admin, org.id, inactive)
                .await
                .unwrap(),
            Denied
        ));
        applied(
            update_organization_member_role(
                &database.pool,
                owner,
                org.id,
                inactive,
                OrgRole::Member,
            )
            .await
            .unwrap(),
        );
        assert!(matches!(
            update_organization_member_role(
                &database.pool,
                owner,
                org.id,
                inactive,
                OrgRole::Owner
            )
            .await
            .unwrap(),
            Denied
        ));
        database
            .execute(
                "UPDATE org_memberships SET role='owner' WHERE org_id=$1 AND user_id=$2",
                &[org.id, inactive],
            )
            .await;
        applied(
            remove_organization_member(&database.pool, owner, org.id, inactive)
                .await
                .unwrap(),
        );
        assert_eq!(database.count("SELECT COUNT(*) FROM org_memberships m JOIN users u ON u.id=m.user_id WHERE m.org_id=$1 AND m.role='owner' AND NOT u.is_banned AND NOT u.must_change_password", &[org.id]).await, 1);
        database
            .execute(
                &format!("UPDATE users SET {flag}=FALSE WHERE id=$1"),
                &[inactive],
            )
            .await;
    }
    // Malformed targets cannot receive a fresh role while restricted, and they
    // are never counted as usable owners.
    let malformed_org = database.org(owner, "usable_malformed").await;
    applied(
        add_organization_member(
            &database.pool,
            owner,
            malformed_org.id,
            inactive,
            OrgRole::Member,
        )
        .await
        .unwrap(),
    );
    database
        .execute(
            "UPDATE org_memberships SET role='unknown' WHERE org_id=$1 AND user_id=$2",
            &[malformed_org.id, inactive],
        )
        .await;
    database
        .execute(
            "UPDATE users SET must_change_password=TRUE WHERE id=$1",
            &[inactive],
        )
        .await;
    assert!(matches!(
        update_organization_member_role(
            &database.pool,
            owner,
            malformed_org.id,
            inactive,
            OrgRole::Owner
        )
        .await
        .unwrap(),
        Denied
    ));
    assert!(matches!(
        update_organization_member_role(
            &database.pool,
            owner,
            malformed_org.id,
            owner,
            OrgRole::Member
        )
        .await
        .unwrap(),
        Conflict
    ));
    database
        .execute(
            "UPDATE users SET must_change_password=FALSE WHERE id=$1",
            &[inactive],
        )
        .await;
    // Two usable owners plus an inactive owner: exactly one self-demotion wins.
    let second = database.actor("usable_second").await;
    let org = database.org(owner, "usable_race").await;
    applied(
        add_organization_member(&database.pool, owner, org.id, second, OrgRole::Owner)
            .await
            .unwrap(),
    );
    applied(
        add_organization_member(&database.pool, owner, org.id, inactive, OrgRole::Owner)
            .await
            .unwrap(),
    );
    database
        .execute("UPDATE users SET is_banned=TRUE WHERE id=$1", &[inactive])
        .await;
    let (a, b) = tokio::join!(
        update_organization_member_role(&database.pool, owner, org.id, owner, OrgRole::Member),
        update_organization_member_role(&database.pool, second, org.id, second, OrgRole::Member),
    );
    let a = a.unwrap();
    let b = b.unwrap();
    assert_eq!(
        usize::from(matches!(a, Applied(_))) + usize::from(matches!(b, Applied(_))),
        1
    );
    assert_eq!(
        usize::from(matches!(a, Conflict)) + usize::from(matches!(b, Conflict)),
        1
    );
    let winner = if matches!(a, Applied(_)) {
        second
    } else {
        owner
    };
    let demoted = if winner == owner { second } else { owner };
    applied(
        update_organization_member_role(&database.pool, winner, org.id, demoted, OrgRole::Owner)
            .await
            .unwrap(),
    );
    let (a, b) = tokio::join!(
        remove_organization_member(&database.pool, owner, org.id, owner),
        remove_organization_member(&database.pool, second, org.id, second),
    );
    let a = a.unwrap();
    let b = b.unwrap();
    assert_eq!(
        usize::from(matches!(a, Applied(_))) + usize::from(matches!(b, Applied(_))),
        1
    );
    assert_eq!(
        usize::from(matches!(a, Conflict)) + usize::from(matches!(b, Conflict)),
        1
    );
    assert_eq!(database.count("SELECT COUNT(*) FROM org_memberships m JOIN users u ON u.id=m.user_id WHERE m.org_id=$1 AND m.role='owner' AND NOT u.is_banned AND NOT u.must_change_password", &[org.id]).await, 1);
}

async fn scoped_cleanup_and_team_deletion(database: &Database) {
    let owner = database.actor("repair_owner").await;
    let member = database.actor("repair_member").await;
    let admin = database.actor("repair_admin").await;
    let foreign = database.actor("repair_foreign").await;
    let org = database.org(owner, "repair_org").await;
    let other_org = database.org(foreign, "repair_foreign_org").await;
    let team = database
        .team(owner, org.id, "repair_team", TeamPrivacy::Visible)
        .await;
    let other_team = database
        .team(
            foreign,
            other_org.id,
            "repair_other_team",
            TeamPrivacy::Secret,
        )
        .await;
    let repo = database.repo(owner, Some(org.id), "repair_repo").await;
    let foreign_repo = database
        .repo(foreign, Some(other_org.id), "repair_foreign_repo")
        .await;
    // A team revocation cancels runs only in repositories granted to that team.
    applied(
        grant_organization_team_repository(
            &database.pool,
            owner,
            org.id,
            team.id,
            repo.id,
            permission(3),
        )
        .await
        .unwrap(),
    );
    applied(
        add_organization_member(&database.pool, owner, org.id, member, OrgRole::Member)
            .await
            .unwrap(),
    );
    applied(
        add_organization_member(&database.pool, owner, org.id, admin, OrgRole::Admin)
            .await
            .unwrap(),
    );
    applied(
        add_organization_member(
            &database.pool,
            foreign,
            other_org.id,
            member,
            OrgRole::Member,
        )
        .await
        .unwrap(),
    );
    applied(
        add_organization_team_member(
            &database.pool,
            foreign,
            other_org.id,
            other_team.id,
            member,
            TeamRole::Member,
        )
        .await
        .unwrap(),
    );
    database.run(repo.id, member, "running").await;
    database.run(repo.id, member, "success").await;
    database.run(foreign_repo.id, member, "running").await;
    for (round, corruption) in ["banned", "reset", "stale", "malformed"]
        .into_iter()
        .enumerate()
    {
        // Each removal also has a queued run to cancel.
        database.run(repo.id, member, "queued").await;
        applied(
            add_organization_team_member(
                &database.pool,
                owner,
                org.id,
                team.id,
                member,
                TeamRole::Member,
            )
            .await
            .unwrap(),
        );
        match corruption {
            "banned" => {
                database
                    .execute("UPDATE users SET is_banned=TRUE WHERE id=$1", &[member])
                    .await
            }
            "reset" => {
                database
                    .execute(
                        "UPDATE users SET must_change_password=TRUE WHERE id=$1",
                        &[member],
                    )
                    .await
            }
            "stale" => {
                database
                    .execute(
                        "DELETE FROM org_memberships WHERE org_id=$1 AND user_id=$2",
                        &[org.id, member],
                    )
                    .await
            }
            _ => {
                database
                    .execute(
                        "UPDATE org_memberships SET role='unknown' WHERE org_id=$1 AND user_id=$2",
                        &[org.id, member],
                    )
                    .await
            }
        }
        assert!(matches!(
            add_organization_team_member(
                &database.pool,
                owner,
                org.id,
                team.id,
                member,
                TeamRole::Member
            )
            .await
            .unwrap(),
            Denied
        ));
        assert!(matches!(
            remove_organization_team_member(&database.pool, owner, org.id, other_team.id, member)
                .await
                .unwrap(),
            Missing
        ));
        applied(
            remove_organization_team_member(&database.pool, admin, org.id, team.id, member)
                .await
                .unwrap(),
        );
        assert!(matches!(
            remove_organization_team_member(&database.pool, admin, org.id, team.id, member)
                .await
                .unwrap(),
            Missing
        ));
        assert_eq!(
            database
                .count(
                    "SELECT COUNT(*) FROM team_memberships WHERE team_id=$1 AND user_id=$2",
                    &[other_team.id, member]
                )
                .await,
            1
        );
        assert_eq!(
            database.stopped_runs(repo.id, member).await,
            (round as i64 + 1, 1)
        );
        assert_eq!(database.count("SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND status='success' AND cancel_requested", &[repo.id]).await, 0);
        assert_eq!(
            database
                .count(
                    "SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND cancel_requested",
                    &[foreign_repo.id]
                )
                .await,
            0
        );
        database
            .execute(
                "UPDATE users SET is_banned=FALSE,must_change_password=FALSE WHERE id=$1",
                &[member],
            )
            .await;
        if corruption == "stale" {
            applied(
                add_organization_member(&database.pool, owner, org.id, member, OrgRole::Member)
                    .await
                    .unwrap(),
            );
        } else if corruption == "malformed" {
            database
                .execute(
                    "UPDATE org_memberships SET role='member' WHERE org_id=$1 AND user_id=$2",
                    &[org.id, member],
                )
                .await;
        }
        database
            .execute(
                "UPDATE ci_runs SET cancel_requested=FALSE WHERE repo_id=$1",
                &[repo.id],
            )
            .await;
    }
    applied(
        add_organization_team_member(
            &database.pool,
            owner,
            org.id,
            team.id,
            member,
            TeamRole::Member,
        )
        .await
        .unwrap(),
    );
    database
        .execute(
            "INSERT INTO team_repo_access(team_id,repo_id,permission) VALUES($1,$2,5)",
            &[team.id, foreign_repo.id],
        )
        .await;
    db::repositories::access::grant_repo_access(
        &database.pool,
        member,
        foreign_repo.id,
        db::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    applied(
        revoke_organization_team_repository(
            &database.pool,
            admin,
            org.id,
            team.id,
            foreign_repo.id,
        )
        .await
        .unwrap(),
    );
    assert_eq!(
        database
            .count(
                "SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND cancel_requested",
                &[foreign_repo.id]
            )
            .await,
        0
    );
    assert_eq!(database.count("SELECT COUNT(*) FROM repo_access WHERE repo_id=$1 AND user_id=$2 AND access_level=3", &[foreign_repo.id, member]).await, 1);
    assert_eq!(
        database
            .count(
                "SELECT COUNT(*) FROM repositories WHERE id=$1 AND owner_org_id=$2",
                &[foreign_repo.id, other_org.id]
            )
            .await,
        1
    );
    applied(
        grant_organization_team_repository(
            &database.pool,
            owner,
            org.id,
            team.id,
            repo.id,
            permission(3),
        )
        .await
        .unwrap(),
    );
    applied(
        revoke_organization_team_repository(&database.pool, admin, org.id, team.id, repo.id)
            .await
            .unwrap(),
    );
    assert_eq!(database.count("SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND status='running' AND cancel_requested", &[repo.id]).await, 1);
    applied(
        grant_organization_team_repository(
            &database.pool,
            owner,
            org.id,
            team.id,
            repo.id,
            permission(3),
        )
        .await
        .unwrap(),
    );
    applied(
        grant_organization_team_repository(
            &database.pool,
            foreign,
            other_org.id,
            other_team.id,
            foreign_repo.id,
            permission(3),
        )
        .await
        .unwrap(),
    );
    database
        .execute(
            "INSERT INTO team_repo_access(team_id,repo_id,permission) VALUES($1,$2,5)",
            &[team.id, foreign_repo.id],
        )
        .await;
    database
        .execute(
            "UPDATE ci_runs SET cancel_requested=FALSE WHERE repo_id=$1",
            &[repo.id],
        )
        .await;
    database
        .execute(
            "UPDATE users SET must_change_password=TRUE WHERE id=$1",
            &[admin],
        )
        .await;
    assert!(matches!(
        delete_organization_team(&database.pool, admin, org.id, team.id)
            .await
            .unwrap(),
        Denied
    ));
    database
        .execute(
            "UPDATE users SET must_change_password=FALSE WHERE id=$1",
            &[admin],
        )
        .await;
    assert!(matches!(
        delete_organization_team(&database.pool, member, org.id, team.id)
            .await
            .unwrap(),
        Denied
    ));
    assert!(matches!(
        delete_organization_team(&database.pool, admin, org.id, other_team.id)
            .await
            .unwrap(),
        Missing
    ));
    database.run(repo.id, member, "queued").await;
    applied(
        delete_organization_team(&database.pool, admin, org.id, team.id)
            .await
            .unwrap(),
    );
    assert_eq!(database.stopped_runs(repo.id, member).await, (5, 1));
    assert_eq!(
        database
            .count("SELECT COUNT(*) FROM teams WHERE id=$1", &[team.id])
            .await,
        0
    );
    assert_eq!(
        database
            .count(
                "SELECT COUNT(*) FROM team_memberships WHERE team_id=$1",
                &[team.id]
            )
            .await,
        0
    );
    assert_eq!(
        database
            .count(
                "SELECT COUNT(*) FROM team_repo_access WHERE team_id=$1",
                &[team.id]
            )
            .await,
        0
    );
    assert_eq!(
        database
            .count("SELECT COUNT(*) FROM teams WHERE id=$1", &[other_team.id])
            .await,
        1
    );
    assert_eq!(
        database
            .count(
                "SELECT COUNT(*) FROM team_repo_access WHERE team_id=$1 AND repo_id=$2",
                &[other_team.id, foreign_repo.id]
            )
            .await,
        1
    );
    assert_eq!(database.count("SELECT COUNT(*) FROM repo_access WHERE repo_id=$1 AND user_id=$2 AND access_level=3", &[foreign_repo.id, member]).await, 1);
    assert_eq!(database.count("SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND status='running' AND cancel_requested", &[repo.id]).await, 1);
    assert_eq!(database.count("SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND status='success' AND cancel_requested", &[repo.id]).await, 0);
    assert_eq!(
        database
            .count(
                "SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND cancel_requested",
                &[foreign_repo.id]
            )
            .await,
        0
    );
    assert!(matches!(
        delete_organization_team(&database.pool, admin, org.id, team.id)
            .await
            .unwrap(),
        Missing
    ));
}

async fn suspended_governance_and_explicit_audited_recovery(database: &Database) {
    let owner = database.actor("suspend_owner").await;
    let replacement = database.actor("suspend_member").await;
    let administrator = database.actor("suspend_site_admin").await;
    let outsider = database.actor("suspend_outsider").await;
    database
        .execute(
            "UPDATE users SET is_admin=TRUE WHERE id=$1",
            &[administrator],
        )
        .await;
    let org = database.org(owner, "suspend_org").await;
    applied(
        add_organization_member(&database.pool, owner, org.id, replacement, OrgRole::Admin)
            .await
            .unwrap(),
    );
    let repository = database.repo(owner, Some(org.id), "suspend_repo").await;
    assert!(matches!(
        recover_suspended_organization_owner(&database.pool, administrator, org.id, replacement)
            .await
            .unwrap(),
        Conflict
    ));
    assert!(
        matches!(
            db::users::transitions::change_ban(&database.pool, administrator, owner, true)
                .await
                .unwrap(),
            db::users::transitions::BanChange::Updated(_)
        ),
        "last tenant owner must not be immune to a justified account ban"
    );
    let metadata = applied(
        get_organization_metadata(&database.pool, replacement, org.id)
            .await
            .unwrap(),
    );
    assert!(metadata.governance_suspended);
    assert_eq!(
        get_organization_repo_access(&database.pool, replacement, &repository)
            .await
            .unwrap(),
        db::models::AccessLevel::Read,
        "suspension retains reads, not tenant write authority"
    );
    assert!(matches!(
        update_organization_profile(&database.pool, replacement, org.id, &profile())
            .await
            .unwrap(),
        OrganizationOutcome::Suspended
    ));
    assert!(matches!(
        add_organization_member(
            &database.pool,
            replacement,
            org.id,
            outsider,
            OrgRole::Member
        )
        .await
        .unwrap(),
        OrganizationOutcome::Suspended
    ));
    assert!(
        matches!(
            get_organization_metadata(&database.pool, administrator, org.id)
                .await
                .unwrap(),
            Denied
        ),
        "site administrator is not implicit tenant member"
    );
    assert!(matches!(
        recover_suspended_organization_owner(&database.pool, replacement, org.id, replacement)
            .await
            .unwrap(),
        Denied
    ));
    assert!(
        matches!(
            recover_suspended_organization_owner(&database.pool, administrator, org.id, outsider)
                .await
                .unwrap(),
            Denied
        ),
        "recovery cannot manufacture an external membership"
    );
    assert!(
        matches!(
            recover_suspended_organization_owner(&database.pool, administrator, org.id, owner)
                .await
                .unwrap(),
            Denied
        ),
        "recovery cannot lift owner ban/reset restrictions"
    );
    // Failure of the mandatory audit record must roll back the fresh role.
    match &database.raw {
        Raw::Postgres(_) => {
            database.execute("CREATE FUNCTION org_recovery_fail_audit() RETURNS trigger AS $$ BEGIN RAISE EXCEPTION 'owned audit outage'; END; $$ LANGUAGE plpgsql", &[]).await;
            database.execute("CREATE TRIGGER org_recovery_audit_outage BEFORE INSERT ON audit_log FOR EACH ROW EXECUTE FUNCTION org_recovery_fail_audit()", &[]).await;
        }
        Raw::Sqlite(_) => database.execute("CREATE TRIGGER org_recovery_audit_outage BEFORE INSERT ON audit_log BEGIN SELECT RAISE(ABORT,'owned audit outage'); END", &[]).await,
    }
    assert!(recover_suspended_organization_owner(
        &database.pool,
        administrator,
        org.id,
        replacement
    )
    .await
    .is_err());
    assert_eq!(database.count("SELECT COUNT(*) FROM org_memberships WHERE org_id=$1 AND user_id=$2 AND role='admin'", &[org.id,replacement]).await, 1);
    assert!(
        applied(
            get_organization_metadata(&database.pool, replacement, org.id)
                .await
                .unwrap()
        )
        .governance_suspended
    );
    match &database.raw {
        Raw::Postgres(_) => {
            database
                .execute("DROP TRIGGER org_recovery_audit_outage ON audit_log", &[])
                .await;
            database
                .execute("DROP FUNCTION org_recovery_fail_audit()", &[])
                .await;
        }
        Raw::Sqlite(_) => {
            database
                .execute("DROP TRIGGER org_recovery_audit_outage", &[])
                .await
        }
    }
    let recovered = applied(
        recover_suspended_organization_owner(&database.pool, administrator, org.id, replacement)
            .await
            .unwrap(),
    );
    assert_eq!(recovered.role, "owner");
    assert!(
        !applied(
            get_organization_metadata(&database.pool, replacement, org.id)
                .await
                .unwrap()
        )
        .governance_suspended
    );
    assert_eq!(database.count("SELECT COUNT(*) FROM audit_log WHERE org_id=$1 AND actor_id=$2 AND action='org.governance.recover_owner'", &[org.id,administrator]).await, 1);
    assert_eq!(
        database
            .count(
                "SELECT COUNT(*) FROM users WHERE id=$1 AND is_banned",
                &[owner]
            )
            .await,
        1
    );
    assert_eq!(database.count("SELECT COUNT(*) FROM org_memberships WHERE org_id=$1 AND user_id=$2 AND role='owner'", &[org.id,owner]).await, 1);
    assert!(matches!(
        recover_suspended_organization_owner(&database.pool, administrator, org.id, replacement)
            .await
            .unwrap(),
        Conflict
    ));
    assert!(matches!(
        get_organization_metadata(&database.pool, administrator, org.id)
            .await
            .unwrap(),
        Denied
    ));
    // Verified password recovery of an ORIGINAL owner restores usability without
    // replacing membership or generating an exceptional ownership-recovery audit.
    let original = database.actor("reset_original_owner").await;
    let observer = database.actor("reset_observer").await;
    let reset_org = database.org(original, "reset_governance_org").await;
    applied(
        add_organization_member(
            &database.pool,
            original,
            reset_org.id,
            observer,
            OrgRole::Member,
        )
        .await
        .unwrap(),
    );
    db::users::accounts::update_user_password(&database.pool, original, "owned-old-hash", true)
        .await
        .unwrap();
    assert!(
        applied(
            get_organization_metadata(&database.pool, observer, reset_org.id)
                .await
                .unwrap()
        )
        .governance_suspended
    );
    assert!(db::users::transitions::compare_exchange_password(
        &database.pool,
        original,
        "owned-old-hash",
        "owned-new-hash",
        true,
        true
    )
    .await
    .unwrap());
    assert!(
        !applied(
            get_organization_metadata(&database.pool, original, reset_org.id)
                .await
                .unwrap()
        )
        .governance_suspended
    );
    assert_eq!(
        database
            .count(
                "SELECT COUNT(*) FROM audit_log WHERE org_id=$1",
                &[reset_org.id]
            )
            .await,
        0
    );
}

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
        tokio::task::yield_now().await;
    }
    panic!("the authority changes did not reach the held membership row");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_organization_writes_finish_before_banning_non_owner_admin() {
    let mut database = Database::postgres().await;
    let site = database.actor("write_site_owner").await;
    database
        .execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[site])
        .await;
    let Raw::Postgres(raw) = &database.raw else {
        unreachable!("PostgreSQL fixture");
    };
    let raw = raw.clone();
    for operation in ["team", "profile", "avatar"] {
        let admin = database.actor(&format!("write_{operation}_admin")).await;
        let org = database
            .org(site, &format!("write_{operation}_org"))
            .await
            .id;
        applied(
            add_organization_member(&database.pool, site, org, admin, OrgRole::Admin)
                .await
                .unwrap(),
        );
        let mut gate = raw.begin().await.unwrap();
        let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *gate)
            .await
            .unwrap();
        // Pause the write after it locks the actor, before it checks the owner.
        sqlx::query("SELECT id FROM org_memberships WHERE org_id=$1 AND user_id=$2 FOR SHARE")
            .bind(org)
            .bind(admin)
            .fetch_one(&mut *gate)
            .await
            .unwrap();
        let pool = database.pool.clone();
        let writing = tokio::spawn(async move {
            match operation {
                "team" => {
                    let input = NewTeam::new(
                        "contended_team",
                        TeamProfile::new("Contended team", None, TeamPrivacy::Visible).unwrap(),
                    )
                    .unwrap();
                    let team = applied(
                        create_organization_team(&pool, admin, org, &input)
                            .await
                            .unwrap(),
                    );
                    assert_eq!(team.slug, "contended_team");
                }
                "profile" => {
                    let metadata = applied(
                        update_organization_profile(&pool, admin, org, &profile())
                            .await
                            .unwrap(),
                    );
                    assert_eq!(metadata.display_name.as_deref(), Some("Changed"));
                }
                "avatar" => {
                    assert_eq!(
                        applied(
                            set_organization_avatar(&pool, admin, org, None)
                                .await
                                .unwrap()
                        ),
                        None
                    );
                }
                _ => unreachable!(),
            }
        });
        wait_until_blocked_by(&raw, holder, 1).await;
        let pool = database.pool.clone();
        let banning = tokio::spawn(async move {
            db::users::transitions::change_ban(&pool, site, admin, true).await
        });
        // The ban must wait without locking the owner, which the write still needs.
        wait_until_blocked_by(&raw, holder, 2).await;
        gate.commit().await.unwrap();
        tokio::time::timeout(Duration::from_secs(30), writing)
            .await
            .expect("organization write must finish without a deadlock")
            .unwrap();
        let banned = tokio::time::timeout(Duration::from_secs(30), banning)
            .await
            .expect("ban must finish without a deadlock")
            .unwrap()
            .unwrap();
        assert!(matches!(
            banned,
            db::users::transitions::BanChange::Updated(user) if user.is_banned
        ));
        assert!(matches!(
            update_organization_profile(&database.pool, admin, org, &profile())
                .await
                .unwrap(),
            Denied
        ));
        assert!(
            !applied(
                get_organization_metadata(&database.pool, site, org)
                    .await
                    .unwrap()
            )
            .governance_suspended
        );
    }
    database.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_account_transition_includes_a_concurrent_owner_promotion() {
    let mut database = Database::postgres().await;
    let site = database.actor("promotion_site_admin").await;
    database
        .execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[site])
        .await;
    let original = database.actor("promotion_original_owner").await;
    let replacement = database.actor("promotion_replacement").await;
    let runner = database.actor("promotion_runner").await;
    let organization = database.org(original, "promotion_org").await;
    applied(
        add_organization_member(
            &database.pool,
            original,
            organization.id,
            replacement,
            OrgRole::Admin,
        )
        .await
        .unwrap(),
    );
    let repository = database
        .repo(original, Some(organization.id), "promotion_repo")
        .await;
    db::repositories::access::grant_repo_access(
        &database.pool,
        runner,
        repository.id,
        db::models::AccessLevel::Admin,
    )
    .await
    .unwrap();
    assert!(matches!(
        db::users::transitions::change_ban(&database.pool, site, original, true)
            .await
            .unwrap(),
        db::users::transitions::BanChange::Updated(user) if user.is_banned
    ));
    database.run(repository.id, runner, "queued").await;

    let Raw::Postgres(raw) = &database.raw else {
        unreachable!("PostgreSQL fixture");
    };
    let raw = raw.clone();
    let mut gate = raw.begin().await.unwrap();
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *gate)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM org_memberships WHERE org_id=$1 AND user_id=$2 FOR SHARE")
        .bind(organization.id)
        .bind(replacement)
        .fetch_one(&mut *gate)
        .await
        .unwrap();

    let pool = database.pool.clone();
    let promoting = tokio::spawn(async move {
        recover_suspended_organization_owner(&pool, site, organization.id, replacement).await
    });
    wait_until_blocked_by(&raw, holder, 1).await;
    let pool = database.pool.clone();
    let banning = tokio::spawn(async move {
        db::users::transitions::change_ban(&pool, site, replacement, true).await
    });
    wait_until_blocked_by(&raw, holder, 2).await;
    gate.commit().await.unwrap();

    let promoted = tokio::time::timeout(Duration::from_secs(30), promoting)
        .await
        .expect("the owner promotion must finish")
        .unwrap()
        .unwrap();
    assert_eq!(applied(promoted).role, "owner");
    let banned = tokio::time::timeout(Duration::from_secs(30), banning)
        .await
        .expect("the account transition must finish")
        .unwrap()
        .unwrap();
    assert!(matches!(
        banned,
        db::users::transitions::BanChange::Updated(user) if user.is_banned
    ));
    assert_eq!(database.stopped_runs(repository.id, runner).await, (1, 0));
    database.close().await;
}

async fn native_suite(database: &Database) {
    atomic_creation_safe_metadata_namespace_and_validation(database).await;
    owner_admin_member_rules_and_inactive_actors(database).await;
    concurrent_last_owner_mutations(database).await;
    usable_owner_governance_and_inactive_cleanup(database).await;
    suspended_governance_and_explicit_audited_recovery(database).await;
    visible_team_repositories_require_effective_read(database).await;
    scoped_cleanup_and_team_deletion(database).await;
    same_org_teams_secret_visibility_and_permissions(database).await;
    invalid_persisted_roles_levels_and_joins_fail_closed(database).await;
    revocation_cleanup_cancellation_downgrade_and_collaborator_regrant(database).await;
    cancellation_failure_rolls_back_entire_membership_revocation(database).await;
    numeric_user_org_collision_and_personal_owner_unchanged(database).await;
}
#[tokio::test]
async fn sqlite_organization_authority() {
    let mut database = Database::sqlite().await;
    native_suite(&database).await;
    database.close().await;
}
#[tokio::test]
async fn postgres_organization_authority_required_fixture() {
    let mut database = Database::postgres().await;
    native_suite(&database).await;
    database.close().await;
}
