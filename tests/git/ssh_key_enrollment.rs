//! Shared SSH key validation, enrollment and live identity reads on both databases.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::native_database as native;
use crate::support::pull_requests as clients;

use axum::http::StatusCode;
use clients::Client;
use codeyard::backend::{api::create_router, app::AppState, db, storage::ObjectStore};
use database::Database;
use serde_json::json;
use std::sync::Arc;

const PUBLIC: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";

async fn contract(pg: bool) {
    use db::users::ssh_keys::SshKeyWriteRefusal as Refusal;
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        Default::default(),
        None,
    ));
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let bob = Client::login(&router, f.pool(), bob_id).await;

    for name in [String::new(), "x".repeat(101)] {
        assert_eq!(
            codeyard::backend::identity::keys::enroll(
                f.pool(),
                alice_id,
                Some(&alice.session),
                &name,
                PUBLIC
            )
            .await
            .unwrap_err(),
            codeyard::backend::identity::keys::KeyError::InvalidName,
        );
        let error = alice
            .expect(
                "POST",
                "/api/user/keys",
                json!({"name": name, "public_key": PUBLIC}),
                StatusCode::BAD_REQUEST,
            )
            .await;
        assert_eq!(error["error"], "validation_error");
        let (status, _) = alice
            .form("/settings/keys", &[("name", &name), ("public_key", PUBLIC)])
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    for invalid in [
        "ssh-ed25519 AAAA".to_owned(),
        "ssh-ed25519 !!!".into(),
        PUBLIC.replacen("ssh-ed25519", "ssh-rsa", 1),
        format!("command=bad {PUBLIC}"),
        format!("{PUBLIC}\n{PUBLIC}"),
        "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\n-----END OPENSSH PRIVATE KEY-----".into(),
    ] {
        let error = db::users::ssh_keys::add_ssh_key(f.pool(), alice_id, "Invalid", &invalid)
            .await
            .unwrap_err();
        assert_eq!(error.downcast_ref::<Refusal>(), Some(&Refusal::InvalidKey));
        let error = alice
            .expect(
                "POST",
                "/api/user/keys",
                json!({"name": "Invalid", "public_key": invalid}),
                StatusCode::BAD_REQUEST,
            )
            .await;
        assert_eq!(error["error"], "validation_error");
        assert!(!error.to_string().contains("AAAA"));
    }
    assert!(db::users::ssh_keys::list_user_keys(f.pool(), alice_id)
        .await
        .unwrap()
        .is_empty());
    let (status, page) = alice
        .form(
            "/settings/keys",
            &[
                ("name", "Failed key title"),
                ("public_key", "ssh-ed25519 AAAA"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
    assert!(page.contains("Invalid SSH public key format."));
    assert!(
        page.contains("Failed key title") && page.contains("ssh-ed25519 AAAA"),
        "draft is retained"
    );
    let private = "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\n-----END OPENSSH PRIVATE KEY-----";
    let (status, page) = alice
        .form(
            "/settings/keys",
            &[("name", "Private paste"), ("public_key", private)],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(page.contains("never a private key"));
    assert!(!page.contains("BEGIN OPENSSH PRIVATE KEY"));

    // Security-key algorithm names must not be rejected by an API prefix shortcut.
    let security_key = "sk-ssh-ed25519@openssh.com AAAAGnNrLXNzaC1lZDI1NTE5QG9wZW5zc2guY29tAAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJlAAAAC3NzaDpmaXh0dXJl";
    let enrolled = alice
        .expect(
            "POST",
            "/api/user/keys",
            json!({"name": "Security key", "public_key": security_key}),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(enrolled["key_type"], "sk-ssh-ed25519@openssh.com");
    let id = enrolled["id"].as_i64().unwrap();
    assert_eq!(
        db::users::ssh_keys::get_user_by_ssh_key_id(f.pool(), id)
            .await
            .unwrap()
            .unwrap()
            .user
            .id,
        alice_id
    );
    alice
        .expect(
            "DELETE",
            &format!("/api/user/keys/{id}"),
            json!(null),
            StatusCode::NO_CONTENT,
        )
        .await;

    let form_public = format!("  {PUBLIC} alice public comment  ");
    let (status, _) = alice
        .form(
            "/settings/keys",
            &[("name", "Laptop"), ("public_key", &form_public)],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let keys = db::users::ssh_keys::list_user_keys(f.pool(), alice_id)
        .await
        .unwrap();
    assert_eq!(keys.len(), 1);
    let key = &keys[0];
    assert_eq!(
        key.fingerprint,
        "SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU"
    );
    assert_eq!(key.public_key, format!("{PUBLIC} alice public comment"));
    assert_eq!(
        db::users::ssh_keys::get_user_by_key_fingerprint(f.pool(), &key.fingerprint)
            .await
            .unwrap()
            .unwrap()
            .user
            .id,
        alice_id
    );
    assert_eq!(
        db::users::ssh_keys::get_user_by_ssh_key_id(f.pool(), key.id)
            .await
            .unwrap()
            .unwrap()
            .key
            .id,
        key.id
    );
    for (actor, client) in [(alice_id, &alice), (bob_id, &bob)] {
        let public = format!("{PUBLIC} another comment");
        let error = db::users::ssh_keys::add_ssh_key(f.pool(), actor, "Copy", &public)
            .await
            .unwrap_err();
        assert_eq!(error.downcast_ref::<Refusal>(), Some(&Refusal::AlreadyUsed));
        let error = client
            .expect(
                "POST",
                "/api/user/keys",
                json!({"name": "Copy", "public_key": public}),
                StatusCode::CONFLICT,
            )
            .await;
        assert_eq!(error["error"], "key_already_registered");
        assert!(!error.to_string().contains("alice"));
        let (status, page) = client
            .form(
                "/settings/keys",
                &[("name", "Duplicate draft"), ("public_key", &public)],
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(page.contains("This SSH public key is already registered."));
        assert!(page.contains("Duplicate draft") && page.contains(&public));
    }

    // Serialize enrollment even when several users submit a new key simultaneously.
    let private =
        ssh_key::PrivateKey::random(&mut rand::rngs::OsRng, ssh_key::Algorithm::Ed25519).unwrap();
    let public = private.public_key().to_openssh().unwrap();
    let fingerprint = db::users::ssh_keys::calculate_fingerprint(&public).unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..8 {
        let pool = f.pool().clone();
        let public = format!("{public} submitter-{index}");
        let barrier = barrier.clone();
        tasks.spawn(async move {
            barrier.wait().await;
            db::users::ssh_keys::add_ssh_key(
                &pool,
                if index % 2 == 0 { alice_id } else { bob_id },
                "Race",
                &public,
            )
            .await
        });
    }
    let mut successes = 0;
    while let Some(result) = tasks.join_next().await {
        match result.unwrap() {
            Ok(key) => {
                successes += 1;
                assert_eq!(key.fingerprint, fingerprint);
            }
            Err(error) => assert_eq!(error.downcast_ref::<Refusal>(), Some(&Refusal::AlreadyUsed)),
        }
    }
    assert_eq!(successes, 1);
    let mut all = db::users::ssh_keys::list_user_keys(f.pool(), alice_id)
        .await
        .unwrap();
    all.extend(
        db::users::ssh_keys::list_user_keys(f.pool(), bob_id)
            .await
            .unwrap(),
    );
    assert_eq!(
        all.iter()
            .filter(|key| key.fingerprint == fingerprint)
            .count(),
        1
    );

    // Other users cannot delete this key.
    bob.expect(
        "DELETE",
        &format!("/api/user/keys/{}", key.id),
        json!(null),
        StatusCode::NOT_FOUND,
    )
    .await;
    let (status, page) = bob
        .form(&format!("/settings/keys/{}/delete", key.id), &[])
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{page}");
    assert!(db::users::ssh_keys::list_user_keys(f.pool(), alice_id)
        .await
        .unwrap()
        .iter()
        .any(|row| row.id == key.id));
    alice
        .expect(
            "DELETE",
            &format!("/api/user/keys/{}", key.id),
            json!(null),
            StatusCode::NO_CONTENT,
        )
        .await;
    alice
        .expect(
            "DELETE",
            &format!("/api/user/keys/{}", key.id),
            json!(null),
            StatusCode::NOT_FOUND,
        )
        .await;
    let (status, page) = alice
        .form(&format!("/settings/keys/{}/delete", key.id), &[])
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{page}");
    assert!(
        db::users::ssh_keys::get_user_by_key_fingerprint(f.pool(), &key.fingerprint)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        db::users::ssh_keys::get_user_by_ssh_key_id(f.pool(), key.id)
            .await
            .unwrap()
            .is_none()
    );

    // A stored key that fails validation cannot authenticate on either transport.
    f.execute(&format!("INSERT INTO ssh_keys(user_id,name,fingerprint,public_key,key_type) VALUES($1,'Malformed','{}','ssh-ed25519 AAAA','ssh-ed25519')", key.fingerprint), &[alice_id]).await;
    let malformed = db::users::ssh_keys::list_user_keys(f.pool(), alice_id)
        .await
        .unwrap()
        .into_iter()
        .find(|other| other.name == "Malformed")
        .unwrap();
    assert!(
        db::users::ssh_keys::get_user_by_key_fingerprint(f.pool(), &key.fingerprint)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        db::users::ssh_keys::get_user_by_ssh_key_id(f.pool(), malformed.id)
            .await
            .unwrap()
            .is_none()
    );
    let (status, page) = alice
        .form(&format!("/settings/keys/{}/delete", malformed.id), &[])
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    assert!(!db::users::ssh_keys::list_user_keys(f.pool(), alice_id)
        .await
        .unwrap()
        .iter()
        .any(|row| row.id == malformed.id));

    for (name, restriction) in [
        ("banned-key-owner", "is_banned"),
        ("password-change-key-owner", "must_change_password"),
    ] {
        let user = f.actor(name).await;
        let client = Client::login(&router, f.pool(), user).await;
        let existing_private =
            ssh_key::PrivateKey::random(&mut rand::rngs::OsRng, ssh_key::Algorithm::Ed25519)
                .unwrap();
        let existing_public = existing_private.public_key().to_openssh().unwrap();
        let existing = codeyard::backend::identity::keys::enroll(
            f.pool(),
            user,
            Some(&client.session),
            "Existing",
            &existing_public,
        )
        .await
        .unwrap();
        f.execute(
            &format!("UPDATE users SET {restriction}=TRUE WHERE id=$1"),
            &[user],
        )
        .await;

        let refused_private =
            ssh_key::PrivateKey::random(&mut rand::rngs::OsRng, ssh_key::Algorithm::Ed25519)
                .unwrap();
        let refused_public = refused_private.public_key().to_openssh().unwrap();
        let error =
            db::users::ssh_keys::add_ssh_key(f.pool(), user, "Must not persist", &refused_public)
                .await
                .unwrap_err();
        assert_eq!(
            error.downcast_ref::<Refusal>(),
            Some(&Refusal::AccountRestricted)
        );
        assert_eq!(
            codeyard::backend::identity::keys::delete(f.pool(), user, existing.id)
                .await
                .unwrap_err(),
            codeyard::backend::identity::keys::KeyError::AccountRestricted
        );

        let (status, page) = client
            .form(
                "/settings/keys",
                &[
                    ("name", "Must not persist"),
                    ("public_key", &refused_public),
                ],
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
        let (status, page) = client
            .form(&format!("/settings/keys/{}/delete", existing.id), &[])
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
        // The restriction covers every settings form except the password change.
        let (status, page) = client
            .form("/settings/profile", &[("display_name", "Changed")])
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
        let keys = db::users::ssh_keys::list_user_keys(f.pool(), user)
            .await
            .unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].id, existing.id);
    }
    f.finish().await;
}

async fn session_enrollment_contract(pg: bool) {
    use codeyard::backend::identity::keys::{self, KeyError};
    use db::users::{sessions, ssh_keys, transitions};

    let mut f = Database::new(pg).await;
    let user = db::users::accounts::create_user_with_password_and_flags(
        f.pool(),
        "session-key-owner",
        None,
        "old-hash",
        false,
        false,
    )
    .await
    .unwrap();
    let other = f.actor("other-key-owner").await;
    let session = native::token();
    sessions::create_session(
        f.pool(),
        &session,
        user.id,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    for (actor, id) in [
        (user.id, None),
        (user.id, Some("missing")),
        (other, Some(session.as_str())),
    ] {
        assert_eq!(
            keys::enroll(f.pool(), actor, id, "Refused", PUBLIC)
                .await
                .unwrap_err(),
            KeyError::InvalidSession
        );
    }

    // Enrollment first: a committed key remains independent of its browser session.
    let key = keys::enroll(f.pool(), user.id, Some(&session), "Before change", PUBLIC)
        .await
        .unwrap();
    assert!(transitions::compare_exchange_password(
        f.pool(),
        user.id,
        "old-hash",
        "new-hash",
        false,
        true
    )
    .await
    .unwrap());
    assert!(ssh_keys::get_user_by_ssh_key_id(f.pool(), key.id)
        .await
        .unwrap()
        .is_some());
    keys::delete(f.pool(), user.id, key.id).await.unwrap();
    assert_eq!(
        keys::enroll(f.pool(), user.id, Some(&session), "After change", PUBLIC)
            .await
            .unwrap_err(),
        KeyError::InvalidSession
    );

    let session = native::token();
    sessions::create_session(
        f.pool(),
        &session,
        user.id,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let pool = f.pool().clone();
    let user_id = user.id;
    let enrollment = ssh_keys::add_ssh_key_for_session(&pool, user_id, &session, "Queued", PUBLIC);
    tokio::pin!(enrollment);
    match f.pool() {
        db::DbPool::Postgres(raw) => {
            let mut change = raw.begin().await.unwrap();
            sqlx::query("UPDATE users SET password_hash='third-hash' WHERE id=$1")
                .bind(user.id)
                .execute(&mut *change)
                .await
                .unwrap();
            let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *change)
                .await
                .unwrap();
            let blocked = async {
                loop {
                    let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)))")
                        .bind(holder).fetch_one(raw).await.unwrap();
                    if waiting {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            };
            tokio::select! {
                result = &mut enrollment => panic!("enrollment did not wait: {result:?}"),
                result = tokio::time::timeout(std::time::Duration::from_secs(5), blocked) => result.expect("enrollment waits on account lock"),
            }
            sqlx::query("DELETE FROM sessions WHERE user_id=$1")
                .bind(user.id)
                .execute(&mut *change)
                .await
                .unwrap();
            change.commit().await.unwrap();
        }
        db::DbPool::Sqlite(raw) => {
            let mut change = raw.begin_with("BEGIN IMMEDIATE").await.unwrap();
            sqlx::query("UPDATE users SET password_hash='third-hash' WHERE id=?1")
                .bind(user.id)
                .execute(&mut *change)
                .await
                .unwrap();
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(100), &mut enrollment)
                    .await
                    .is_err(),
                "enrollment must wait for the writer"
            );
            sqlx::query("DELETE FROM sessions WHERE user_id=?1")
                .bind(user.id)
                .execute(&mut *change)
                .await
                .unwrap();
            change.commit().await.unwrap();
        }
    }
    assert_eq!(
        enrollment
            .await
            .unwrap_err()
            .downcast_ref::<ssh_keys::SshKeyWriteRefusal>(),
        Some(&ssh_keys::SshKeyWriteRefusal::InvalidSession)
    );
    assert!(ssh_keys::list_user_keys(f.pool(), user.id)
        .await
        .unwrap()
        .is_empty());

    let expired = native::token();
    sessions::create_session(
        f.pool(),
        &expired,
        user.id,
        None,
        None,
        chrono::Utc::now() - chrono::Duration::seconds(1),
    )
    .await
    .unwrap();
    assert_eq!(
        keys::enroll(f.pool(), user.id, Some(&expired), "Expired", PUBLIC)
            .await
            .unwrap_err(),
        KeyError::InvalidSession
    );
    // Operator authority is explicit and does not inherit the revoked session.
    ssh_keys::add_ssh_key(f.pool(), user.id, "Operator", PUBLIC)
        .await
        .unwrap();
    f.finish().await;
}

#[tokio::test]
async fn sqlite_ssh_enrollment_rechecks_its_session_after_password_changes() {
    session_enrollment_contract(false).await;
}

#[tokio::test]
async fn postgres_ssh_enrollment_rechecks_its_session_after_password_changes() {
    session_enrollment_contract(true).await;
}

#[tokio::test]
async fn sqlite_ssh_key_enrollment_and_identity_are_unambiguous() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_ssh_key_enrollment_and_identity_are_unambiguous() {
    contract(true).await;
}
