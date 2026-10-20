//! Live actor and repository authority at local Git publication boundaries.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database;

use codeyard::backend::{
    db::{self, models::AccessLevel, organizations as orgs},
    repositories::{
        lifecycle::provision,
        store::{repo_store, FileWrite},
    },
};
use database::Database;

fn applied<T: std::fmt::Debug>(outcome: orgs::OrganizationOutcome<T>) -> T {
    match outcome {
        orgs::OrganizationOutcome::Applied(value) => value,
        other => panic!("organization fixture change failed: {other:?}"),
    }
}

fn write(branch: &str, path: &str, expected: &str) -> FileWrite {
    FileWrite {
        branch: branch.into(),
        path: path.into(),
        message: format!("write {path}"),
        expected_revision: expected.into(),
        creating: true,
        deleting: false,
    }
}

async fn contract(pg: bool) {
    let mut fixture = Database::new(pg).await;
    let owner = fixture.actor("publication_owner").await;
    let writer = fixture.actor("publication_writer").await;
    let root = tempfile::tempdir().unwrap();
    let repository = provision(
        (fixture.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(owner),
        "publication",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let store = repo_store(&repository, None);
    let initial = store
        .save_file_as(
            fixture.pool(),
            repository.id,
            owner,
            &write("main", "README.md", "unborn"),
            b"initial\n",
            "Owner",
            "owner@example.invalid",
        )
        .await
        .unwrap()
        .sha;
    let other = provision(
        (fixture.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(owner),
        "publication-other",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let other_store = repo_store(&other, None);
    let mismatched = other_store
        .prepare_file(
            &write("main", "wrong-repository.txt", "unborn"),
            b"must remain unreachable\n",
            "Owner",
            "owner@example.invalid",
        )
        .await
        .unwrap();
    assert!(other_store
        .publish_file_as(fixture.pool(), repository.id, owner, &mismatched)
        .await
        .unwrap_err()
        .is_write_denied());
    assert!(!other_store
        .refs()
        .await
        .unwrap()
        .contains_key("refs/heads/main"));

    db::repositories::access::grant_repo_access(
        fixture.pool(),
        writer,
        repository.id,
        AccessLevel::Write,
    )
    .await
    .unwrap();

    let direct = store
        .prepare_file(
            &write("main", "direct.txt", &initial),
            b"prepared before revoke\n",
            "Writer",
            "writer@example.invalid",
        )
        .await
        .unwrap();
    assert!(
        db::repositories::access::revoke_repo_access(fixture.pool(), writer, repository.id)
            .await
            .unwrap()
    );
    assert!(store
        .publish_file_as(fixture.pool(), repository.id, writer, &direct)
        .await
        .unwrap_err()
        .is_write_denied());
    assert_eq!(store.commit("main").await.unwrap().sha, initial);

    db::repositories::access::grant_repo_access(
        fixture.pool(),
        writer,
        repository.id,
        AccessLevel::Write,
    )
    .await
    .unwrap();
    let banned = store
        .prepare_file(
            &write("main", "banned.txt", &initial),
            b"prepared before ban\n",
            "Writer",
            "writer@example.invalid",
        )
        .await
        .unwrap();
    fixture
        .execute("UPDATE users SET is_banned=TRUE WHERE id=$1", &[writer])
        .await;
    assert!(store
        .publish_file_as(fixture.pool(), repository.id, writer, &banned)
        .await
        .unwrap_err()
        .is_write_denied());
    fixture
        .execute(
            "UPDATE users SET is_banned=FALSE,must_change_password=TRUE WHERE id=$1",
            &[writer],
        )
        .await;
    assert!(store
        .create_branch_as(
            fixture.pool(),
            repository.id,
            writer,
            "reset-denied",
            &initial,
        )
        .await
        .unwrap_err()
        .is_write_denied());
    fixture
        .execute(
            "UPDATE users SET must_change_password=FALSE WHERE id=$1",
            &[writer],
        )
        .await;

    let protected = store
        .prepare_file(
            &write("main", "protected.txt", &initial),
            b"prepared before protection\n",
            "Owner",
            "owner@example.invalid",
        )
        .await
        .unwrap();
    db::repositories::protection::upsert_branch_protection_rule(
        fixture.pool(),
        db::repositories::protection::BranchProtectionRuleUpsert {
            actor_id: owner,
            repo_id: repository.id,
            pattern: "main",
            require_pull_request: true,
            required_approving_reviews: 0,
            dismiss_stale_reviews: false,
            require_status_checks: false,
            required_status_checks: vec![],
            require_up_to_date_branch: false,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(store
        .publish_file_as(fixture.pool(), repository.id, owner, &protected)
        .await
        .unwrap_err()
        .is_protected_branch());
    assert_eq!(store.commit("main").await.unwrap().sha, initial);

    store
        .create_branch_as(fixture.pool(), repository.id, writer, "feature", &initial)
        .await
        .unwrap();
    assert!(
        db::repositories::access::revoke_repo_access(fixture.pool(), writer, repository.id)
            .await
            .unwrap()
    );
    assert!(store
        .delete_branch_as(fixture.pool(), repository.id, writer, "feature")
        .await
        .unwrap_err()
        .is_write_denied());
    assert!(store
        .refs()
        .await
        .unwrap()
        .contains_key("refs/heads/feature"));

    fixture
        .execute(
            "UPDATE repositories SET is_archived=TRUE WHERE id=$1",
            &[repository.id],
        )
        .await;
    assert!(store
        .create_tag_as(
            fixture.pool(),
            repository.id,
            owner,
            "archive-denied",
            &initial,
            Some("prepared tag"),
            "Owner",
            "owner@example.invalid",
        )
        .await
        .unwrap_err()
        .is_write_denied());
    fixture
        .execute(
            "UPDATE repositories SET is_archived=FALSE WHERE id=$1",
            &[repository.id],
        )
        .await;
    let tag = store
        .create_tag_as(
            fixture.pool(),
            repository.id,
            owner,
            "版本-β",
            &initial,
            None,
            "Owner",
            "owner@example.invalid",
        )
        .await
        .unwrap();
    assert_eq!(tag.name, "版本-β");
    store
        .delete_tag_as(fixture.pool(), repository.id, owner, "版本-β")
        .await
        .unwrap();

    let org = applied(
        orgs::create_organization(
            fixture.pool(),
            owner,
            &orgs::NewOrganization::new(
                "publication_org",
                orgs::OrganizationProfile::new(None, None).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    applied(
        orgs::add_organization_member(
            fixture.pool(),
            owner,
            org.id,
            writer,
            db::models::OrgRole::Member,
        )
        .await
        .unwrap(),
    );
    let team = applied(
        orgs::create_organization_team(
            fixture.pool(),
            owner,
            org.id,
            &orgs::NewTeam::new(
                "writers",
                orgs::TeamProfile::new("Writers", None, orgs::TeamPrivacy::Secret).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    applied(
        orgs::add_organization_team_member(
            fixture.pool(),
            owner,
            org.id,
            team.id,
            writer,
            orgs::TeamRole::Member,
        )
        .await
        .unwrap(),
    );
    let org_repo = provision(
        (fixture.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner {
            actor_id: owner,
            target: db::repositories::creation::RepositoryTarget::Organization(org.id),
        },
        "team-publication",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    applied(
        orgs::grant_organization_team_repository(
            fixture.pool(),
            owner,
            org.id,
            team.id,
            org_repo.id,
            orgs::TeamRepositoryPermission::new(3).unwrap(),
        )
        .await
        .unwrap(),
    );
    let org_store = repo_store(&org_repo, None);
    let org_initial = org_store
        .save_file_as(
            fixture.pool(),
            org_repo.id,
            owner,
            &write("main", "README.md", "unborn"),
            b"org\n",
            "Owner",
            "owner@example.invalid",
        )
        .await
        .unwrap()
        .sha;
    let team_prepared = org_store
        .prepare_file(
            &write("main", "team.txt", &org_initial),
            b"prepared before team loss\n",
            "Writer",
            "writer@example.invalid",
        )
        .await
        .unwrap();
    applied(
        orgs::remove_organization_team_member(fixture.pool(), owner, org.id, team.id, writer)
            .await
            .unwrap(),
    );
    assert!(org_store
        .publish_file_as(fixture.pool(), org_repo.id, writer, &team_prepared)
        .await
        .unwrap_err()
        .is_write_denied());
    assert_eq!(org_store.commit("main").await.unwrap().sha, org_initial);
    assert!(db::pull_requests::records::list_repo_pull_requests(
        fixture.pool(),
        repository.id,
        &Default::default(),
        10,
    )
    .await
    .unwrap()
    .is_empty());
    assert!(
        db::ci::latest_ci_runs_for_commit(fixture.pool(), repository.id, &initial, 0, "main")
            .await
            .unwrap()
            .is_empty()
    );
    fixture.finish().await;
}

#[tokio::test]
async fn sqlite_publications_recheck_live_authority() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_publications_recheck_live_authority() {
    contract(true).await;
}
