//! Two-factor sign-in: enabling with a generated code, the password step withholding the session,
//! single-use codes, recovery codes, password-gated changes, administrator recovery and the
//! settings forms over HTTP. On SQLite and PostgreSQL.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::fake_webui_bundle as bundle;
use crate::support::native_database::token;
use crate::support::pull_requests::Client;
use crate::support::{database::Database, PASSWORD_WORK};
use axum::{
    body::{to_bytes, Body},
    http::{header, HeaderMap, Request, StatusCode},
    Router,
};
use codeyard::backend::{
    api::create_router,
    app::AppState,
    ci::CiConfig,
    crypto::SecretEncryptor,
    db,
    db::models::User,
    identity::{
        self,
        login::{FirstFactor, LoginError, PasswordCredentials},
        passwords,
        sessions::{InitiatingSession, SessionContext},
        two_factor::{self, TwoFactorError},
    },
    storage::ObjectStore,
};
use std::sync::Arc;
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery";

async fn fixture(pg: bool) -> Database {
    bundle::install();
    let database = Database::new(pg).await;
    database
        .execute(
            "INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE",
            &[],
        )
        .await;
    database
}

async fn new_user(database: &Database, name: &str, admin: bool) -> User {
    let hash = passwords::hash_password(PASSWORD).await.unwrap();
    db::users::accounts::create_user_with_password_and_flags(
        database.pool(),
        name,
        None,
        &hash,
        admin,
        false,
    )
    .await
    .unwrap()
}

fn encryptor() -> SecretEncryptor {
    SecretEncryptor::new(&[7u8; 32])
}

fn context<'a>(fresh: &'a str, user_agent: Option<&'a str>) -> SessionContext<'a> {
    SessionContext {
        initiating: InitiatingSession::Missing,
        fresh_id: fresh,
        user_agent,
        expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
    }
}

fn step() -> u64 {
    two_factor::now_unix() / 30
}

fn code_at(secret: &[u8], step: u64) -> String {
    format!("{:06}", two_factor::totp_code(secret, step))
}

fn secret_bytes(base32: &str) -> Vec<u8> {
    data_encoding::BASE32_NOPAD
        .decode(base32.as_bytes())
        .unwrap()
}

async fn account_session(database: &Database, user_id: i64) -> String {
    let session = token();
    db::users::sessions::create_session(
        database.pool(),
        &session,
        user_id,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    session
}

/// Turns two-factor on for the user and returns the raw secret and the recovery codes.
async fn enable(database: &Database, key: &SecretEncryptor, user: &User) -> (Vec<u8>, Vec<String>) {
    let setup = two_factor::begin_setup(database.pool(), key, user)
        .await
        .unwrap();
    let secret = secret_bytes(&setup.secret);
    let codes = two_factor::confirm_setup(
        database.pool(),
        key,
        user,
        &account_session(database, user.id).await,
        PASSWORD,
        &setup.token,
        &code_at(&secret, step()),
    )
    .await
    .unwrap();
    (secret, codes)
}

/// The sealed token of a password step that is waiting for its second factor.
async fn pending_after_password(
    database: &Database,
    key: &SecretEncryptor,
    name: &str,
    user_agent: Option<&str>,
) -> String {
    let fresh = token();
    match identity::login::password_login(
        database.pool(),
        Some(key),
        PasswordCredentials {
            identity: name,
            password: PASSWORD,
        },
        context(&fresh, user_agent),
    )
    .await
    .unwrap()
    {
        FirstFactor::SecondFactor { pending } => {
            assert!(
                db::users::sessions::get_session_account(database.pool(), &fresh)
                    .await
                    .unwrap()
                    .is_none(),
                "the password step must not create a session"
            );
            pending
        }
        FirstFactor::Authenticated(_) => panic!("a two-factor account signed in on its password"),
    }
}

async fn second_factor(
    database: &Database,
    key: &SecretEncryptor,
    pending: &str,
    code: &str,
    user_agent: Option<&str>,
) -> Result<identity::AuthenticatedAccount, LoginError> {
    let fresh = token();
    identity::login::second_factor_login(
        database.pool(),
        Some(key),
        pending,
        code,
        context(&fresh, user_agent),
    )
    .await
}

async fn sign_in_flow(pg: bool) {
    let _work = PASSWORD_WORK.lock().await;
    let mut database = fixture(pg).await;
    let key = encryptor();
    let alice = new_user(&database, "alice", false).await;
    let (secret, codes) = enable(&database, &key, &alice).await;
    assert_eq!(codes.len(), 10);
    assert!(two_factor::is_enabled(database.pool(), alice.id)
        .await
        .unwrap());

    let pending = pending_after_password(&database, &key, "alice", Some("Firefox")).await;
    // Enrollment spent its step. Keep the exact accepted code for the replay check.
    let accepted_code = code_at(&secret, step() + 1);
    assert_eq!(
        second_factor(&database, &key, &pending, &accepted_code, Some("Firefox"))
            .await
            .err(),
        None
    );
    assert_eq!(
        second_factor(
            &database,
            &key,
            &pending,
            &code_at(&secret, step() + 10),
            Some("Firefox")
        )
        .await
        .err(),
        Some(LoginError::InvalidTwoFactorCode)
    );
    // A tampered token, or the same token from another browser, starts nothing.
    assert_eq!(
        second_factor(
            &database,
            &key,
            &format!("{pending}x"),
            &code_at(&secret, step() + 2),
            Some("Firefox")
        )
        .await
        .err(),
        Some(LoginError::PendingLoginExpired)
    );
    assert_eq!(
        second_factor(
            &database,
            &key,
            &pending,
            &code_at(&secret, step() + 2),
            Some("Chrome")
        )
        .await
        .err(),
        Some(LoginError::PendingLoginExpired)
    );

    // Replaying the code that just signed in is refused, from a new pending sign-in as well.
    let again = pending_after_password(&database, &key, "alice", Some("Firefox")).await;
    assert_eq!(
        second_factor(&database, &key, &again, &accepted_code, Some("Firefox"))
            .await
            .err(),
        Some(LoginError::InvalidTwoFactorCode)
    );

    // A recovery code works once, in any case or spacing.
    let recovery = pending_after_password(&database, &key, "alice", Some("Firefox")).await;
    assert!(second_factor(
        &database,
        &key,
        &recovery,
        &codes[0].to_lowercase().replace('-', " "),
        Some("Firefox")
    )
    .await
    .is_ok());
    assert_eq!(
        second_factor(&database, &key, &recovery, &codes[0], Some("Firefox"))
            .await
            .err(),
        Some(LoginError::InvalidTwoFactorCode)
    );
    database.finish().await;
}

async fn password_changes_need_the_password(pg: bool) {
    let _work = PASSWORD_WORK.lock().await;
    let mut database = fixture(pg).await;
    let key = encryptor();
    let alice = new_user(&database, "alice", false).await;
    let (_, codes) = enable(&database, &key, &alice).await;
    let session = account_session(&database, alice.id).await;
    assert!(matches!(
        two_factor::regenerate_recovery_codes(database.pool(), &alice, &session, "wrong password")
            .await,
        Err(TwoFactorError::InvalidPassword)
    ));
    assert!(matches!(
        two_factor::disable(database.pool(), &alice, &session, "wrong password").await,
        Err(TwoFactorError::InvalidPassword)
    ));
    assert!(two_factor::is_enabled(database.pool(), alice.id)
        .await
        .unwrap());

    let regenerated =
        two_factor::regenerate_recovery_codes(database.pool(), &alice, &session, PASSWORD)
            .await
            .unwrap();
    assert_eq!(regenerated.len(), 10);
    let pending = pending_after_password(&database, &key, "alice", None).await;
    assert_eq!(
        second_factor(&database, &key, &pending, &codes[0], None)
            .await
            .err(),
        Some(LoginError::InvalidTwoFactorCode)
    );
    assert!(
        second_factor(&database, &key, &pending, &regenerated[0], None)
            .await
            .is_ok()
    );

    two_factor::disable(database.pool(), &alice, &session, PASSWORD)
        .await
        .unwrap();
    assert!(!two_factor::is_enabled(database.pool(), alice.id)
        .await
        .unwrap());
    // With two-factor off, the password alone signs in again.
    let fresh = token();
    assert!(matches!(
        identity::login::password_login(
            database.pool(),
            Some(&key),
            PasswordCredentials {
                identity: "alice",
                password: PASSWORD
            },
            context(&fresh, None),
        )
        .await,
        Ok(FirstFactor::Authenticated(_))
    ));
    database.finish().await;
}

async fn administrator_recovery(pg: bool) {
    let _work = PASSWORD_WORK.lock().await;
    let mut database = fixture(pg).await;
    let key = encryptor();
    let alice = new_user(&database, "alice", false).await;
    let root = new_user(&database, "root", true).await;
    enable(&database, &key, &alice).await;
    assert!(matches!(
        two_factor::admin_turn_off(database.pool(), None, alice.id).await,
        Err(TwoFactorError::Authority(_))
    ));
    // A signed-in account that is not an administrator is refused too.
    let member_session = token();
    db::users::sessions::create_session(
        database.pool(),
        &member_session,
        alice.id,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    assert!(matches!(
        two_factor::admin_turn_off(database.pool(), Some(&member_session), alice.id).await,
        Err(TwoFactorError::Authority(_))
    ));
    let admin_session = token();
    db::users::sessions::create_session(
        database.pool(),
        &admin_session,
        root.id,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    two_factor::admin_turn_off(database.pool(), Some(&admin_session), alice.id)
        .await
        .unwrap();
    assert!(!two_factor::is_enabled(database.pool(), alice.id)
        .await
        .unwrap());
    database.finish().await;
}

/// The settings forms over HTTP: the setup page shows the secret, a code turns two-factor on and
/// shows the recovery codes once, and disabling checks the current password.
async fn settings_forms(pg: bool) {
    let _work = PASSWORD_WORK.lock().await;
    let mut database = fixture(pg).await;
    let alice = new_user(&database, "alice", false).await;
    let root = tempfile::tempdir().unwrap();
    let router = create_router(AppState::new(
        database.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        Some(Arc::new(encryptor())),
        CiConfig::default(),
        None,
    ));
    let client = Client::login(&router, database.pool(), alice.id).await;
    let form = |fields: &str| format!("csrf_token={}&{fields}", client.csrf).into_bytes();

    let setup = client
        .exchange(
            "POST",
            "/settings/two-factor/setup",
            "application/x-www-form-urlencoded",
            form(""),
        )
        .await;
    assert_eq!(setup.status, StatusCode::OK);
    let page = String::from_utf8_lossy(&setup.body).to_string();
    let secret = between(&page, "secret=", |c| {
        c.is_ascii_uppercase() || c.is_ascii_digit()
    });
    let token = field_value(&page, "setup");
    let secret = secret_bytes(&secret);

    let refused = client
        .exchange(
            "POST",
            "/settings/two-factor/enable",
            "application/x-www-form-urlencoded",
            form(&format!("setup={token}&code={}", code_at(&secret, step()))),
        )
        .await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
    assert!(!two_factor::is_enabled(database.pool(), alice.id)
        .await
        .unwrap());

    let enabled = client
        .exchange(
            "POST",
            "/settings/two-factor/enable",
            "application/x-www-form-urlencoded",
            form(&format!(
                "current_password={PASSWORD}&setup={token}&code={}",
                code_at(&secret, step())
            )),
        )
        .await;
    assert_eq!(enabled.status, StatusCode::OK);
    let shown = recovery_codes_in(&String::from_utf8_lossy(&enabled.body));
    assert_eq!(shown.len(), 10, "codes are shown once in the response");
    assert!(two_factor::is_enabled(database.pool(), alice.id)
        .await
        .unwrap());

    let wrong = client
        .exchange(
            "POST",
            "/settings/two-factor/disable",
            "application/x-www-form-urlencoded",
            form("current_password=nope"),
        )
        .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert!(String::from_utf8_lossy(&wrong.body).contains("Current password is incorrect"));
    assert!(two_factor::is_enabled(database.pool(), alice.id)
        .await
        .unwrap());

    let off = client
        .exchange(
            "POST",
            "/settings/two-factor/disable",
            "application/x-www-form-urlencoded",
            form(&format!("current_password={PASSWORD}")),
        )
        .await;
    assert_eq!(off.status, StatusCode::SEE_OTHER);
    assert!(off
        .location
        .unwrap_or_default()
        .contains("saved=two-factor-off"));
    assert!(!two_factor::is_enabled(database.pool(), alice.id)
        .await
        .unwrap());
    database.finish().await;
}

/// The text after `marker` made of the characters `accept` allows.
fn between(page: &str, marker: &str, accept: impl Fn(char) -> bool) -> String {
    let start = page.find(marker).expect("marker") + marker.len();
    page[start..].chars().take_while(|c| accept(*c)).collect()
}

/// The value of the first hidden input called `name`.
fn field_value(page: &str, name: &str) -> String {
    let at = page.find(&format!("name=\"{name}\"")).expect("field");
    let after = &page[at..];
    let start = after.find("value=\"").expect("value") + "value=\"".len();
    after[start..].split('"').next().unwrap().to_owned()
}

/// Recovery codes are text nodes of the form XXXX-XXXX-XXXX-XXXX.
fn recovery_codes_in(page: &str) -> Vec<String> {
    page.split(['<', '>'])
        .map(str::trim)
        .filter(|text| {
            text.len() == 19
                && text.split('-').count() == 4
                && text.split('-').all(|group| {
                    group.len() == 4
                        && group
                            .bytes()
                            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
                })
        })
        .map(str::to_owned)
        .collect()
}

/// The anonymous session cookie and CSRF token of a visitor, from the sign-in page.
async fn visitor(router: &Router) -> (String, String) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/login")
                .header(header::HOST, "forge.example.test")
                .header(header::ACCEPT, "text/html")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|value| value.strip_prefix("session_id="))
        .and_then(|value| value.split(';').next())
        .expect("the sign-in page sets a visitor session")
        .to_owned();
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    let page = String::from_utf8_lossy(&bytes).into_owned();
    (cookie, field_value(&page, "csrf_token"))
}

/// Submits a native form the way a browser does, with the session cookie and same-origin headers.
async fn submit(
    router: &Router,
    path: &str,
    session: &str,
    fields: &[(&str, &str)],
) -> (StatusCode, HeaderMap, String) {
    let mut body = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in fields {
        body.append_pair(name, value);
    }
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header(header::HOST, "forge.example.test")
                .header(header::ORIGIN, "http://forge.example.test")
                .header(header::ACCEPT, "text/html")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, format!("session_id={session}"))
                .body(Body::from(body.finish()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

/// On a private node the password step and the second-factor page are both sign-in pages, so a
/// two-factor user can finish signing in. Only anonymous visitors reach this path.
async fn private_node_two_factor_sign_in(pg: bool) {
    let _work = PASSWORD_WORK.lock().await;
    let mut database = fixture(pg).await;
    database
        .execute(
            "UPDATE node_configuration SET node_mode='private' WHERE id=1",
            &[],
        )
        .await;
    let key = encryptor();
    let alice = new_user(&database, "alice", false).await;
    let (secret, _) = enable(&database, &key, &alice).await;
    let root = tempfile::tempdir().unwrap();
    let router = create_router(AppState::new(
        database.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        Some(Arc::new(encryptor())),
        CiConfig::default(),
        None,
    ));
    let (visitor_session, csrf) = visitor(&router).await;
    let (status, _, page) = submit(
        &router,
        "/login",
        &visitor_session,
        &[
            ("csrf_token", csrf.as_str()),
            ("username", "alice"),
            ("password", PASSWORD),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let pending = field_value(&page, "pending");
    let code = code_at(&secret, step() + 1);
    let (status, headers, page) = submit(
        &router,
        "/login/two-factor",
        &visitor_session,
        &[
            ("csrf_token", csrf.as_str()),
            ("pending", pending.as_str()),
            ("code", code.as_str()),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    // The session cookie is issued only when the second factor completes the sign-in.
    assert!(headers[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .starts_with("session_id="));
    database.finish().await;
}

/// A password change ends the sign-ins that were started with the old password, including their
/// unused recovery codes.
async fn password_change_ends_pending_sign_ins(pg: bool) {
    let _work = PASSWORD_WORK.lock().await;
    let mut database = fixture(pg).await;
    let key = encryptor();
    let alice = new_user(&database, "alice", false).await;
    let (_, codes) = enable(&database, &key, &alice).await;
    let pending = pending_after_password(&database, &key, "alice", Some("Firefox")).await;
    let session = token();
    db::users::sessions::create_session(
        database.pool(),
        &session,
        alice.id,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    identity::accounts::change_password(
        database.pool(),
        Some(&session),
        identity::accounts::PasswordChange {
            current_password: PASSWORD,
            new_password: "replacement password",
        },
    )
    .await
    .unwrap();
    assert_eq!(
        second_factor(&database, &key, &pending, &codes[0], Some("Firefox"))
            .await
            .err(),
        Some(LoginError::PendingLoginExpired)
    );
    database.finish().await;
}

async fn enrollment_and_revocation(pg: bool) {
    use db::users::two_factor::{self as stored, FactorAuthority};
    let _work = PASSWORD_WORK.lock().await;
    let mut database = fixture(pg).await;
    let pool = database.pool();
    let key = encryptor();
    let alice = new_user(&database, "alice", false).await;
    let session = account_session(&database, alice.id).await;
    let authority = FactorAuthority::Password {
        session: &session,
        expected: alice.password_hash.as_deref().unwrap(),
    };
    let (_, codes) = enable(&database, &key, &alice).await;
    let original = stored::get_two_factor(pool, alice.id)
        .await
        .unwrap()
        .unwrap();
    let confirmed = original
        .last_used_step
        .expect("enrollment commits the spent step");
    assert!(
        !stored::use_totp_step(pool, alice.id, &original.nonce, confirmed)
            .await
            .unwrap()
    );
    assert!(stored::disable_two_factor(pool, alice.id, authority)
        .await
        .unwrap());
    enable(&database, &key, &alice).await;
    assert!(
        !stored::use_totp_step(pool, alice.id, &original.nonce, confirmed + 100)
            .await
            .unwrap()
    );
    let fresh = token();
    assert!(
        db::users::sessions::rotate_authenticated_session(
            pool,
            None,
            &fresh,
            alice.id,
            alice.password_hash.as_deref(),
            None,
            chrono::Utc::now() + chrono::Duration::hours(1)
        )
        .await
        .unwrap()
        .is_none(),
        "password-only issuance must see the new enrollment"
    );
    db::users::sessions::delete_session(pool, &session)
        .await
        .unwrap();
    assert!(
        stored::replace_recovery_codes(pool, alice.id, authority, &[])
            .await
            .is_err()
    );
    assert!(stored::disable_two_factor(pool, alice.id, authority)
        .await
        .is_err());
    let root = new_user(&database, "root", true).await;
    let root_session = account_session(&database, root.id).await;
    let admin = FactorAuthority::Administrator {
        user_id: root.id,
        session: &root_session,
    };
    database
        .execute("UPDATE users SET is_admin=FALSE WHERE id=$1", &[root.id])
        .await;
    assert!(stored::disable_two_factor(pool, alice.id, admin)
        .await
        .is_err());
    database
        .execute(
            "UPDATE users SET is_admin=TRUE,is_banned=TRUE WHERE id=$1",
            &[root.id],
        )
        .await;
    assert!(stored::disable_two_factor(pool, alice.id, admin)
        .await
        .is_err());
    let pending = pending_after_password(&database, &key, "alice", None).await;
    assert_eq!(
        second_factor(&database, &key, &pending, &codes[0], None)
            .await
            .err(),
        Some(LoginError::InvalidTwoFactorCode),
        "reenrollment replaced old recovery codes"
    );
    database.finish().await;
}

async fn unsuccessful_disable_changes_nothing(pg: bool) {
    use db::users::two_factor::{self as stored, FactorAuthority};
    let _work = PASSWORD_WORK.lock().await;
    let mut database = fixture(pg).await;
    let alice = new_user(&database, "alice", false).await;
    let session = account_session(&database, alice.id).await;
    database
        .execute(
            "INSERT INTO two_factor_recovery_codes(user_id,code_hash) VALUES($1,'0000000000000000000000000000000000000000000000000000000000000000')",
            &[alice.id],
        )
        .await;
    database
        .execute(
            "UPDATE users SET two_factor_enabled=TRUE WHERE id=$1",
            &[alice.id],
        )
        .await;
    let authority = FactorAuthority::Password {
        session: &session,
        expected: alice.password_hash.as_deref().unwrap(),
    };
    assert!(
        !stored::disable_two_factor(database.pool(), alice.id, authority)
            .await
            .unwrap()
    );
    let sql = "SELECT COUNT(*) FROM two_factor_recovery_codes WHERE user_id=$1";
    let remaining: i64 = match database.pool() {
        db::DbPool::Postgres(pool) => sqlx::query_scalar(sql)
            .bind(alice.id)
            .fetch_one(pool)
            .await
            .unwrap(),
        db::DbPool::Sqlite(pool) => sqlx::query_scalar(sql)
            .bind(alice.id)
            .fetch_one(pool)
            .await
            .unwrap(),
    };
    assert_eq!(
        remaining, 1,
        "an unsuccessful disable must leave recovery codes alone"
    );
    assert!(
        db::users::accounts::get_user_by_id(database.pool(), alice.id)
            .await
            .unwrap()
            .unwrap()
            .two_factor_enabled
    );
    database.finish().await;
}

#[tokio::test]
async fn sqlite_unsuccessful_disable_changes_nothing() {
    unsuccessful_disable_changes_nothing(false).await;
}
#[tokio::test]
async fn postgres_unsuccessful_disable_changes_nothing() {
    unsuccessful_disable_changes_nothing(true).await;
}

async fn recovery_code_survives_session_failure(pg: bool) {
    let _work = PASSWORD_WORK.lock().await;
    let mut database = fixture(pg).await;
    let key = encryptor();
    let alice = new_user(&database, "alice", false).await;
    let (_, codes) = enable(&database, &key, &alice).await;
    let pending = pending_after_password(&database, &key, "alice", None).await;
    let collision = account_session(&database, alice.id).await;
    assert!(identity::login::second_factor_login(
        database.pool(),
        Some(&key),
        &pending,
        &codes[0],
        context(&collision, None)
    )
    .await
    .is_err());
    assert!(
        second_factor(&database, &key, &pending, &codes[0], None)
            .await
            .is_ok(),
        "failed session insertion must roll back code consumption"
    );
    database.finish().await;
}

#[tokio::test]
async fn sqlite_enrollment_and_revocation() {
    enrollment_and_revocation(false).await;
}
#[tokio::test]
async fn postgres_enrollment_and_revocation() {
    enrollment_and_revocation(true).await;
}
#[tokio::test]
async fn sqlite_recovery_code_survives_session_failure() {
    recovery_code_survives_session_failure(false).await;
}
#[tokio::test]
async fn postgres_recovery_code_survives_session_failure() {
    recovery_code_survives_session_failure(true).await;
}

#[tokio::test]
async fn postgres_two_factor_private_node_sign_in() {
    private_node_two_factor_sign_in(true).await;
}
#[tokio::test]
async fn sqlite_two_factor_private_node_sign_in() {
    private_node_two_factor_sign_in(false).await;
}
#[tokio::test]
async fn postgres_two_factor_password_change_ends_pending_sign_ins() {
    password_change_ends_pending_sign_ins(true).await;
}
#[tokio::test]
async fn sqlite_two_factor_password_change_ends_pending_sign_ins() {
    password_change_ends_pending_sign_ins(false).await;
}

#[tokio::test]
async fn postgres_two_factor_sign_in_flow() {
    sign_in_flow(true).await;
}
#[tokio::test]
async fn sqlite_two_factor_sign_in_flow() {
    sign_in_flow(false).await;
}
#[tokio::test]
async fn postgres_two_factor_password_changes_need_the_password() {
    password_changes_need_the_password(true).await;
}
#[tokio::test]
async fn sqlite_two_factor_password_changes_need_the_password() {
    password_changes_need_the_password(false).await;
}
#[tokio::test]
async fn postgres_two_factor_administrator_recovery() {
    administrator_recovery(true).await;
}
#[tokio::test]
async fn sqlite_two_factor_administrator_recovery() {
    administrator_recovery(false).await;
}
#[tokio::test]
async fn postgres_two_factor_settings_forms() {
    settings_forms(true).await;
}
#[tokio::test]
async fn sqlite_two_factor_settings_forms() {
    settings_forms(false).await;
}
