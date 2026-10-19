//! Two-factor sign-in: enabling with a generated code, the password step withholding the session,
//! single-use codes, recovery codes, password-gated changes, administrator recovery and the
//! settings forms over HTTP. On SQLite and PostgreSQL.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::fake_webui_bundle as bundle;
use crate::support::native_database::token;
use crate::support::pull_requests::Client;
use crate::support::{database::Database, PASSWORD_WORK};
use axum::http::StatusCode;
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
    // The setup code is spent, so this step's code is refused; the next step's code works.
    assert_eq!(
        second_factor(
            &database,
            &key,
            &pending,
            &code_at(&secret, step() + 1),
            Some("Firefox")
        )
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
        second_factor(
            &database,
            &key,
            &again,
            &code_at(&secret, step() + 1),
            Some("Firefox")
        )
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
    assert!(matches!(
        two_factor::regenerate_recovery_codes(database.pool(), &alice, "wrong password").await,
        Err(TwoFactorError::InvalidPassword)
    ));
    assert!(matches!(
        two_factor::disable(database.pool(), &alice, "wrong password").await,
        Err(TwoFactorError::InvalidPassword)
    ));
    assert!(two_factor::is_enabled(database.pool(), alice.id)
        .await
        .unwrap());

    let regenerated = two_factor::regenerate_recovery_codes(database.pool(), &alice, PASSWORD)
        .await
        .unwrap();
    assert_eq!(regenerated.len(), 10);
    assert!(!two_factor::verify_sign_in_code(
        database.pool(),
        &key,
        alice.id,
        &codes[0],
        two_factor::now_unix()
    )
    .await
    .unwrap());
    assert!(two_factor::verify_sign_in_code(
        database.pool(),
        &key,
        alice.id,
        &regenerated[0],
        two_factor::now_unix()
    )
    .await
    .unwrap());

    two_factor::disable(database.pool(), &alice, PASSWORD)
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

    let enabled = client
        .exchange(
            "POST",
            "/settings/two-factor/enable",
            "application/x-www-form-urlencoded",
            form(&format!("setup={token}&code={}", code_at(&secret, step()))),
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
