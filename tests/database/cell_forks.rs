//! A foreign source cannot authorize a native fork after its last admission is revoked.
use crate::support::database::Database;
use codeyard_db_core::cells::{
    authorization::{Home, Proof},
    repositories::{Admission, Owner, Source},
};
use codeyard_db_core::queries::credentials::Actor;
use codeyard_db_repos::repositories::creation::{
    self, CreationAdmission, NewRepository, RepositoryCreationOwner,
};
use codeyard_types::cells::Prefix;
use futures_util::future::BoxFuture;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

struct SourceHome {
    source: Source,
    calls: AtomicUsize,
    revoke_at: AtomicUsize,
}
impl Home for SourceHome {
    fn user<'a>(
        &'a self,
        _name: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<Option<codeyard_db_core::models::User>>> {
        Box::pin(async { Ok(None) })
    }
    fn authorize<'a>(
        &'a self,
        _proof: &'a Proof,
    ) -> BoxFuture<'a, anyhow::Result<Option<codeyard_db_core::cells::authorization::Answer>>> {
        Box::pin(async { Ok(None) })
    }
    fn repository_source<'a>(
        &'a self,
        owner: &'a str,
        name: &'a str,
        _proof: &'a Proof,
    ) -> BoxFuture<'a, anyhow::Result<Option<Source>>> {
        Box::pin(async move {
            assert_eq!(owner, self.source.owner.name());
            assert_eq!(name, self.source.repository.name);
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            Ok((call < self.revoke_at.load(Ordering::SeqCst)).then(|| self.source.clone()))
        })
    }
}
async fn contract(pg: bool) {
    let mut f = Database::empty(pg).await;
    f.complete_setup().await;
    let user = f.actor("fork-owner").await;
    let session = codeyard_identity::sessions::fresh_id(f.pool()).unwrap();
    codeyard_db_identity::users::sessions::create_session(
        f.pool(),
        &session,
        user,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let actor = Actor::from((user, session.clone()));
    let template = codeyard_db_repos::repositories::records::create_repository(
        f.pool(),
        user,
        "local-template",
        None,
        true,
        &uuid::Uuid::new_v4().to_string(),
    )
    .await
    .unwrap();
    assert!(creation::read_fork_source(f.pool(), &actor, template.id)
        .await
        .unwrap()
        .is_some());
    f.execute("INSERT INTO repository_features(repo_id,allow_forking) VALUES($1,FALSE) ON CONFLICT(repo_id) DO UPDATE SET allow_forking=FALSE", &[template.id]).await;
    assert!(
        creation::read_repository_source(f.pool(), &actor, template.id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(creation::read_fork_source(f.pool(), &actor, template.id)
        .await
        .unwrap()
        .is_none());
    f.execute(
        "UPDATE repository_features SET allow_forking=TRUE WHERE repo_id=$1",
        &[template.id],
    )
    .await;
    let mut owner = codeyard_db_identity::users::accounts::get_user_by_id(f.pool(), user)
        .await
        .unwrap()
        .unwrap();
    codeyard_db_core::cells::authorization::redact_user(&mut owner);
    owner.id = *Prefix::new(1).ids().start();
    owner.home_cell = 1;
    owner.username = "remote-owner".into();
    let mut repository = template.clone();
    repository.id = owner.id + 20;
    repository.home_cell = 1;
    repository.owner_user_id = Some(owner.id);
    repository.name = "source".into();
    repository.disk_path.clear();
    repository.storage_node_id = None;
    repository.is_fork = true;
    repository.forked_from_id = Some(template.id);
    let source = Source {
        reader_id: user,
        credential_id: None,
        default_branch_oid: None,
        repository,
        owner: Owner::User(Box::new(owner)),
        allow_forking: true,
    };
    source.retain(f.pool()).await.unwrap();
    assert_eq!(
        f.repository(source.repository.id).await.forked_from_id,
        Some(template.id)
    );
    assert_eq!(
        f.repository(template.id).await.forks_count,
        0,
        "Retaining a foreign fork is metadata only, not a second local fork"
    );
    let gate = Arc::new(SourceHome {
        source: source.clone(),
        calls: AtomicUsize::new(0),
        revoke_at: AtomicUsize::new(2),
    });
    f.pool().set_foreign_authorization(gate.clone());
    assert!(Admission::new(source.clone(), actor.clone(), "another-session".into()).is_err());
    let admission = Admission::new(source.clone(), actor.clone(), session.clone()).unwrap();
    assert!(!format!("{admission:?}").contains(&session));
    let path = uuid::Uuid::new_v4().to_string();
    let insert = |name: &'static str| NewRepository {
        owner: RepositoryCreationOwner::personal(user),
        name,
        description: None,
        is_private: true,
        disk_path: &path,
        storage_node_id: None,
        source_id: Some(source.repository.id),
    };
    let proof = CreationAdmission {
        authority: Some(&actor),
        advisory: None,
        foreign_source: Some(&admission),
    };
    assert!(creation::insert_repository_with_admission(
        f.pool(),
        insert("revoked-at-commit"),
        &[],
        proof
    )
    .await
    .is_err());
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM repositories WHERE name='revoked-at-commit'",
            &[]
        )
        .await,
        0
    );
    assert_eq!(f.repository(source.repository.id).await.forks_count, 0);
    gate.revoke_at.store(usize::MAX, Ordering::SeqCst);
    let fork =
        creation::insert_repository_with_admission(f.pool(), insert("accepted-fork"), &[], proof)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(fork.forked_from_id, Some(source.repository.id));
    assert!(fork.is_fork && fork.is_private && !fork.is_stub);
    assert_eq!(fork.home_cell, 0);
    assert_eq!(
        codeyard_db_repos::repositories::discovery::named_owner_fork(
            f.pool(),
            "fork-owner",
            source.repository.id
        )
        .await
        .unwrap()
        .unwrap()
        .id,
        fork.id
    );
    assert!(
        codeyard_db_repos::repositories::discovery::named_owner_fork(
            f.pool(),
            "remote-owner",
            source.repository.id
        )
        .await
        .unwrap()
        .is_none()
    );
    f.execute("DELETE FROM repositories WHERE id=$1", &[fork.id])
        .await;
    let mut no_fork = source.clone();
    no_fork.allow_forking = false;
    let read_gate = Arc::new(SourceHome {
        source: no_fork,
        calls: AtomicUsize::new(0),
        revoke_at: AtomicUsize::new(2),
    });
    f.pool().set_foreign_authorization(read_gate.clone());
    let pull = || codeyard_db_repos::pull_requests::records::NewPullRequest {
        repo_id: template.id,
        head_repo_id: Some(source.repository.id),
        author_id: user,
        issue: None,
        title: "Foreign head",
        body: "",
        head_branch: "topic",
        head_sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        base_branch: "main",
        base_sha: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        is_draft: false,
        maintainer_can_modify: false,
    };
    assert!(
        codeyard_db_repos::pull_requests::records::create_pull_request(f.pool(), pull())
            .await
            .is_err()
    );
    assert!(
        codeyard_db_repos::pull_requests::records::create_pull_request_with_head(
            f.pool(),
            pull(),
            Some(&admission)
        )
        .await
        .is_err()
    );
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM pull_requests", &[]).await,
        0,
        "Revocation at the final head read rolls back the entire pull creation"
    );
    read_gate.revoke_at.store(usize::MAX, Ordering::SeqCst);
    let admitted = codeyard_db_repos::pull_requests::records::create_pull_request_with_head(
        f.pool(),
        pull(),
        Some(&admission),
    )
    .await
    .unwrap();
    assert_eq!(admitted.head_repo_id, Some(source.repository.id));
    assert_eq!(admitted.repo_id, template.id);
    f.pool().set_foreign_authorization(gate.clone());
    codeyard_db_identity::users::sessions::delete_session(f.pool(), &session)
        .await
        .unwrap();
    assert!(
        codeyard_db_repos::pull_requests::records::create_pull_request_with_head(
            f.pool(),
            codeyard_db_repos::pull_requests::records::NewPullRequest {
                head_branch: "revoked-session",
                ..pull()
            },
            Some(&admission),
        )
        .await
        .is_err()
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM pull_requests", &[]).await, 1);
    assert!(creation::insert_repository_with_admission(
        f.pool(),
        insert("revoked-credential"),
        &[],
        proof
    )
    .await
    .is_err());
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM repositories WHERE name='revoked-credential'",
            &[]
        )
        .await,
        0
    );
    assert!(creation::read_fork_source(f.pool(), &actor, template.id)
        .await
        .unwrap()
        .is_none());
    f.finish().await;
}
#[tokio::test]
async fn sqlite_foreign_fork_source_rechecks_access_and_original_session_at_commit() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_foreign_fork_source_rechecks_access_and_original_session_at_commit() {
    contract(true).await;
}
