use super::*;
use db::users::sessions::{rotate_authenticated_session, SessionCredential};

async fn checked_totp(
    database: &Database,
    key: &SecretEncryptor,
    user: &User,
    secret: &[u8],
) -> (Vec<u8>, i64) {
    two_factor::verify_totp_code(
        database.pool(),
        key,
        user.id,
        &code_at(secret, step() + 1),
        two_factor::now_unix(),
    )
    .await
    .unwrap()
    .unwrap()
}

#[tokio::test]
async fn totp_consumption_rolls_back_when_the_session_insert_fails() {
    let _work = PASSWORD_WORK.lock().await;
    for pg in [false, true] {
        let mut database = fixture(pg).await;
        let key = encryptor();
        let user = new_user(&database, "atomic_totp", false).await;
        let (secret, _) = enable(&database, &key, &user).await;
        let (nonce, step) = checked_totp(&database, &key, &user, &secret).await;
        let credential = SessionCredential::SecondFactor {
            password: user.password_hash.as_deref().unwrap(),
            recovery_hash: None,
            totp: Some((&nonce, step)),
        };
        let collision = token();
        let expiry = chrono::Utc::now() + chrono::Duration::hours(1);
        db::users::sessions::create_browser_session(
            database.pool(),
            &collision,
            None,
            None,
            expiry,
        )
        .await
        .unwrap();
        assert!(rotate_authenticated_session(
            database.pool(),
            None,
            &collision,
            user.id,
            credential,
            None,
            expiry
        )
        .await
        .is_err());
        let stored = db::users::two_factor::get_two_factor(database.pool(), user.id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            stored.last_used_step.unwrap() < step,
            "a failed session insert must not spend its TOTP"
        );
        assert!(rotate_authenticated_session(
            database.pool(),
            None,
            &token(),
            user.id,
            credential,
            None,
            expiry
        )
        .await
        .unwrap()
        .is_some());
        assert!(rotate_authenticated_session(
            database.pool(),
            None,
            &token(),
            user.id,
            credential,
            None,
            expiry
        )
        .await
        .unwrap()
        .is_none());
        database.finish().await;
    }
}

#[tokio::test]
async fn a_verified_retired_authenticator_cannot_issue_a_session() {
    let _work = PASSWORD_WORK.lock().await;
    for pg in [false, true] {
        let mut database = fixture(pg).await;
        let key = encryptor();
        let user = new_user(&database, "retired_totp", false).await;
        let (secret, _) = enable(&database, &key, &user).await;
        let (nonce, step) = checked_totp(&database, &key, &user, &secret).await;
        let management = account_session(&database, user.id).await;
        two_factor::disable(database.pool(), &user, &management, PASSWORD)
            .await
            .unwrap();
        enable(&database, &key, &user).await;
        let credential = SessionCredential::SecondFactor {
            password: user.password_hash.as_deref().unwrap(),
            recovery_hash: None,
            totp: Some((&nonce, step)),
        };
        assert!(rotate_authenticated_session(
            database.pool(),
            None,
            &token(),
            user.id,
            credential,
            None,
            chrono::Utc::now() + chrono::Duration::hours(1)
        )
        .await
        .unwrap()
        .is_none());
        database.finish().await;
    }
}
