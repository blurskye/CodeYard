use crate::support::database::Database;

use codeyard_db_repos::repositories::creation::NewRepository;
use codeyard_db_repos::repositories::creation::RepositoryCreationOwner;
use codeyard_db_repos::repositories::dependencies as records;
use codeyard_db_repos::repositories::dependencies::AlertState;
use codeyard_db_repos::repositories::dependencies::Feature;
async fn contract(postgres: bool) {
    let f = Database::new(postgres).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let repo = codeyard_db_repos::repositories::creation::insert_repository(
        f.pool(),
        NewRepository {
            owner: RepositoryCreationOwner::personal(alice),
            name: "dependencies",
            description: None,
            is_private: false,
            disk_path: "/unused/dependency-fixture.git",
            storage_node_id: None,
            source_id: None,
        },
    )
    .await
    .unwrap()
    .unwrap();
    let alice_session = uuid::Uuid::new_v4().to_string();
    let bob_session = uuid::Uuid::new_v4().to_string();
    for (actor, session) in [(alice, &alice_session), (bob, &bob_session)] {
        codeyard_db_identity::users::sessions::create_session(
            f.pool(),
            session,
            actor,
            None,
            None,
            chrono::Utc::now() + chrono::Duration::hours(1),
        )
        .await
        .unwrap();
    }
    let alice_credential =
        codeyard_database::queries::users::accounts::OwnedAccountCredential::from(&alice_session);
    let bob_credential =
        codeyard_database::queries::users::accounts::OwnedAccountCredential::from(&bob_session);
    let configure = |actor, feature, enabled, consent| {
        let credential = if actor == alice {
            &alice_credential
        } else {
            &bob_credential
        };
        records::configure(
            f.pool(),
            (actor, credential),
            repo.id,
            feature,
            enabled,
            consent,
        )
    };
    let state = || records::state(f.pool(), repo.id);
    assert!(state().await.unwrap().is_none());
    assert!(configure(bob, Feature::Alerts, true, true).await.is_err());
    assert!(configure(alice, Feature::Updates, true, true)
        .await
        .is_err());
    configure(alice, Feature::Alerts, true, false)
        .await
        .unwrap();
    let job = records::claim(f.pool(), &uuid::Uuid::new_v4().to_string())
        .await
        .unwrap()
        .unwrap();
    assert!(records::claim(f.pool(), &uuid::Uuid::new_v4().to_string())
        .await
        .unwrap()
        .is_none());
    records::check_job(f.pool(), &job, Feature::Alerts)
        .await
        .unwrap();
    let advisory = serde_json::json!({"id":"GHSA-2345-6789-cfgh","modified":"2026-10-01T00:00:00Z","summary":"An advisory","affected":[{"package":{"ecosystem":"npm","name":"foo"},"ranges":[{"type":"SEMVER","events":[{"introduced":"0"},{"fixed":"1.2.4"}]}]}]});
    codeyard_app::vulnerabilities::ingest(f.pool(), &serde_json::to_vec(&advisory).unwrap())
        .await
        .unwrap();
    let sha = "a".repeat(40);
    let inventory = codeyard_repositories::repositories::dependency_graph::snapshot(
        sha.clone(),
        "refs/heads/main".into(),
        vec![codeyard_types::repositories::Package {
            manifest: "package-lock.json".into(),
            ecosystem: "npm".into(),
            name: "foo".into(),
            version: "1.2.3".into(),
            package_url: "pkg:npm/foo@1.2.3".into(),
            scope: "runtime".into(),
        }],
    );
    codeyard_db_security::dependency_graph::submit(f.pool(), alice, repo.id, &inventory)
        .await
        .unwrap();
    records::complete_scan(f.pool(), &job, &sha).await.unwrap();
    let snapshot = records::snapshot(f.pool(), alice, repo.id).await.unwrap();
    let key = snapshot.alerts.keys().next().unwrap().clone();
    let alert = codeyard_db_security::dependabot_alerts::alerts(f.pool(), alice, repo.id)
        .await
        .unwrap()
        .remove(0);
    assert!(records::snapshot(f.pool(), bob, repo.id).await.is_err());
    assert_eq!(
        records::snapshot(f.pool(), alice, repo.id)
            .await
            .unwrap()
            .alerts
            .len(),
        1
    );
    codeyard_db_security::dependabot_alerts::update(
        f.pool(),
        alice,
        repo.id,
        alert.number,
        &codeyard_db_security::dependabot_alerts::Update {
            state: "dismissed".into(),
            dismissed_reason: Some("tolerable_risk".into()),
            dismissed_comment: Some("Test-only use reviewed by an administrator".into()),
        },
    )
    .await
    .unwrap();
    records::complete_scan(f.pool(), &job, &sha).await.unwrap();
    assert_eq!(
        records::snapshot(f.pool(), alice, repo.id)
            .await
            .unwrap()
            .alerts[&key]
            .state,
        AlertState::Dismissed
    );
    assert!(codeyard_db_security::dependabot_alerts::update(
        f.pool(),
        bob,
        repo.id,
        alert.number,
        &codeyard_db_security::dependabot_alerts::Update {
            state: "open".into(),
            dismissed_reason: None,
            dismissed_comment: None,
        }
    )
    .await
    .is_err());
    assert!(records::complete_scan(f.pool(), &job, &"b".repeat(40))
        .await
        .is_err());
    // A newer push is coalesced while this lease remains held, and cannot be overwritten.
    records::enqueue(f.pool(), repo.id).await.unwrap();
    assert!(records::complete_scan(f.pool(), &job, &sha).await.is_err());
    records::release(f.pool(), &job, None, None).await.unwrap();
    let next = records::claim(f.pool(), &uuid::Uuid::new_v4().to_string())
        .await
        .unwrap()
        .unwrap();
    assert!(next.generation > job.generation);
    configure(alice, Feature::Alerts, false, false)
        .await
        .unwrap();
    assert!(records::check_job(f.pool(), &next, Feature::Alerts)
        .await
        .is_err());
    records::release(
        f.pool(),
        &next,
        Some("error"),
        Some("An obsolete worker result"),
    )
    .await
    .unwrap();
    assert_eq!(state().await.unwrap().unwrap().last_status, "idle");
    assert_eq!(
        records::snapshot(f.pool(), alice, repo.id)
            .await
            .unwrap()
            .alerts
            .len(),
        1
    );
    configure(alice, Feature::Alerts, true, false)
        .await
        .unwrap();
    configure(alice, Feature::Updates, true, false)
        .await
        .unwrap();
    let public = records::claim(f.pool(), &uuid::Uuid::new_v4().to_string())
        .await
        .unwrap()
        .unwrap();
    records::check_job(f.pool(), &public, Feature::Updates)
        .await
        .unwrap();
    // A public consent decision cannot authorize a concurrent transition to private.
    f.execute(
        "UPDATE repositories SET is_private=TRUE WHERE id=$1",
        &[repo.id],
    )
    .await;
    let paused = state().await.unwrap().unwrap();
    assert!(
        paused.alerts_enabled
            && !paused.updates_enabled
            && !paused.private_consent
            && paused.alerts_actor_id.is_none()
            && paused.lease_nonce.is_none()
    );
    assert!(records::check_job(f.pool(), &public, Feature::Alerts)
        .await
        .is_err());
    assert!(configure(alice, Feature::Updates, true, true)
        .await
        .is_err());
    assert!(configure(alice, Feature::Alerts, true, false)
        .await
        .is_err());
    assert!(state().await.unwrap().unwrap().alerts_actor_id.is_none());
    configure(alice, Feature::Alerts, true, true).await.unwrap();
    assert!(state().await.unwrap().unwrap().private_consent);
    f.execute(
        "UPDATE repositories SET is_private=FALSE WHERE id=$1",
        &[repo.id],
    )
    .await;
    f.execute(
        "UPDATE repositories SET is_private=TRUE WHERE id=$1",
        &[repo.id],
    )
    .await;
    assert!(state().await.unwrap().unwrap().alerts_actor_id.is_none());
    configure(alice, Feature::Alerts, true, true).await.unwrap();
    // Ownership ABA never restores old consent, including to the same old owner.
    f.execute(
        "UPDATE repositories SET owner_user_id=$1 WHERE id=$2",
        &[bob, repo.id],
    )
    .await;
    assert!(records::snapshot(f.pool(), alice, repo.id).await.is_err());
    configure(bob, Feature::Alerts, true, true).await.unwrap();
    assert_eq!(
        records::snapshot(f.pool(), bob, repo.id)
            .await
            .unwrap()
            .alerts
            .len(),
        1
    );
    f.execute(
        "UPDATE repositories SET owner_user_id=$1 WHERE id=$2",
        &[alice, repo.id],
    )
    .await;
    assert!(state().await.unwrap().unwrap().alerts_actor_id.is_none());
    configure(alice, Feature::Alerts, true, true).await.unwrap();
    let expired = records::claim(f.pool(), &uuid::Uuid::new_v4().to_string())
        .await
        .unwrap()
        .unwrap();
    f.execute("UPDATE repository_dependency_security SET lease_until='2000-01-01T00:00:00.000000Z' WHERE repo_id=$1",&[repo.id]).await;
    let replacement = records::claim(f.pool(), &uuid::Uuid::new_v4().to_string())
        .await
        .unwrap()
        .unwrap();
    assert_ne!(expired.nonce, replacement.nonce);
    records::release(f.pool(), &expired, None, None)
        .await
        .unwrap();
    assert_eq!(
        state().await.unwrap().unwrap().lease_nonce.as_deref(),
        Some(replacement.nonce.as_str())
    );
    records::check_job(f.pool(), &replacement, Feature::Alerts)
        .await
        .unwrap();
    records::release(f.pool(), &replacement, None, None)
        .await
        .unwrap();
    assert!(records::claim(f.pool(), &uuid::Uuid::new_v4().to_string())
        .await
        .unwrap()
        .is_none());
    f.execute(
        "UPDATE repositories SET default_branch='stable' WHERE id=$1",
        &[repo.id],
    )
    .await;
    let switched = records::claim(f.pool(), &uuid::Uuid::new_v4().to_string())
        .await
        .unwrap()
        .unwrap();
    assert!(switched.generation > replacement.generation);
    records::release(f.pool(), &switched, None, None)
        .await
        .unwrap();
}
#[tokio::test]
async fn sqlite_dependency_security() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_dependency_security() {
    contract(true).await;
}
