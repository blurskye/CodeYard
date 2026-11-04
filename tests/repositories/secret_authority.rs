//! Secret authority and lock contention on disposable PostgreSQL and SQLite
//! databases. Database-level contracts only; secrets are not delivered to CI here.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
mod contention;
use crate::support::database;
use crate::support::organizations::applied;
mod fixture;

use codeyard_support::secrets::SecretEncryptor;

use codeyard_db_core::models::AccessLevel;
use codeyard_db_core::models::OrgRole;
use codeyard_db_core::queries::secrets::SecretsError;
use database::Database;

const NAME: &str = "SENSITIVE_DEPLOY_TOKEN";
const VALUE: &str = "owned-fixture-plaintext-never-in-metadata";
fn encryptor() -> SecretEncryptor {
    SecretEncryptor::new(&[0x63; 32])
}
fn denied<T>(result: Result<T, SecretsError>) {
    assert!(
        matches!(result, Err(SecretsError::Denied)),
        "expected typed secret denial"
    );
}
async fn put(
    pool: &codeyard_db_core::DbPool,
    repo: i64,
    actor: Option<i64>,
) -> Result<(), SecretsError> {
    codeyard_db_actions::secrets::create_or_update_secret(
        pool,
        &encryptor(),
        repo,
        NAME,
        VALUE,
        actor,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&format!(
            "{:064x}",
            actor.unwrap_or(-1)
        )),
    )
    .await
    .map(|_| ())
}
async fn refuse(d: &Database, repo: i64, actor: i64) {
    refuse_writes(d, repo, actor).await;
    denied(codeyard_db_actions::secrets::list_secrets(d.pool(), actor, repo).await);
}
async fn refuse_writes(d: &Database, repo: i64, actor: i64) {
    let before = d.snapshot(repo).await;
    denied(put(d.pool(), repo, Some(actor)).await);
    denied(
        codeyard_db_actions::secrets::create_or_update_secret(
            d.pool(),
            &encryptor(),
            repo,
            "DENIED_NEW_NAME",
            "denied replacement",
            Some(actor),
            &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&format!(
                "{actor:064x}"
            )),
        )
        .await,
    );
    denied(
        codeyard_db_actions::secrets::delete_secret(
            d.pool(),
            actor,
            repo,
            NAME,
            &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&format!(
                "{actor:064x}"
            )),
        )
        .await,
    );
    denied(
        codeyard_db_actions::secrets::delete_secret(
            d.pool(),
            actor,
            repo,
            "UNKNOWN_NAME",
            &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&format!(
                "{actor:064x}"
            )),
        )
        .await,
    );
    assert_eq!(
        d.snapshot(repo).await,
        before,
        "denial changed ciphertext or metadata"
    );
}
async fn allow(d: &Database, repo: i64, actor: i64) {
    put(d.pool(), repo, Some(actor)).await.unwrap();
    let rows = codeyard_db_actions::secrets::list_secrets(d.pool(), actor, repo)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, NAME);
    let meta = serde_json::to_value(&rows[0]).unwrap();
    let keys: Vec<_> = meta
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["created_at", "name", "updated_at"]);
    assert!(!meta.to_string().contains(VALUE));
    assert!(codeyard_db_actions::secrets::delete_secret(
        d.pool(),
        actor,
        repo,
        NAME,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&format!(
            "{actor:064x}"
        ))
    )
    .await
    .unwrap());
    assert!(!codeyard_db_actions::secrets::delete_secret(
        d.pool(),
        actor,
        repo,
        NAME,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&format!(
            "{actor:064x}"
        ))
    )
    .await
    .unwrap());
    assert!(
        codeyard_db_actions::secrets::list_secrets(d.pool(), actor, repo)
            .await
            .unwrap()
            .is_empty()
    );
    put(d.pool(), repo, Some(actor)).await.unwrap();
}

async fn validation_contract(d: &Database, repo: i64, owner: i64) {
    let before = d.snapshot(repo).await;
    for name in [
        "",
        "1TOKEN",
        "BAD-NAME",
        "BAD.NAME",
        "BAD\nINJECTED_NAME",
        "TÖKEN",
    ] {
        let error = codeyard_db_actions::secrets::create_or_update_secret(
            d.pool(),
            &encryptor(),
            repo,
            name,
            "private hostile value",
            Some(owner),
            &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&format!(
                "{owner:064x}"
            )),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, SecretsError::InvalidName), "{name:?}");
    }
    for name in ["GITHUB_TOKEN", "github_token", "GitHub_OIDC"] {
        let error = codeyard_db_actions::secrets::create_or_update_secret(
            d.pool(),
            &encryptor(),
            repo,
            name,
            "private reserved value",
            Some(owner),
            &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&format!(
                "{owner:064x}"
            )),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, SecretsError::ReservedName), "{name}");
    }
    let error = codeyard_db_actions::secrets::create_or_update_secret(
        d.pool(),
        &encryptor(),
        repo,
        "VALID_NAME",
        "private\0value",
        Some(owner),
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&format!(
            "{owner:064x}"
        )),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, SecretsError::ValueContainsNul));
    let error = codeyard_db_actions::secrets::create_or_update_secret(
        d.pool(),
        &encryptor(),
        repo,
        "VALID_NAME",
        &"x".repeat(codeyard_actions::values::MAX_VALUE_BYTES + 1),
        Some(owner),
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&format!(
            "{owner:064x}"
        )),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, SecretsError::ValueTooLarge));
    assert_eq!(d.snapshot(repo).await, before);

    let longest_name = format!(
        "A{}",
        "Z".repeat(codeyard_actions::values::MAX_NAME_BYTES - 1)
    );
    let largest_value = "v".repeat(codeyard_actions::values::MAX_VALUE_BYTES);
    codeyard_db_actions::secrets::create_or_update_secret(
        d.pool(),
        &encryptor(),
        repo,
        &longest_name,
        &largest_value,
        Some(owner),
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&format!(
            "{owner:064x}"
        )),
    )
    .await
    .unwrap();
    codeyard_db_actions::secrets::create_or_update_secret(
        d.pool(),
        &encryptor(),
        repo,
        "_",
        "",
        Some(owner),
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&format!(
            "{owner:064x}"
        )),
    )
    .await
    .unwrap();
    let decrypted =
        codeyard_db_actions::secrets::decrypt_repository_secrets(d.pool(), &encryptor(), repo)
            .await
            .unwrap();
    assert!(decrypted
        .iter()
        .any(|(name, value)| name == &longest_name && value == &largest_value));
    assert!(decrypted
        .iter()
        .any(|(name, value)| name == "_" && value.is_empty()));
    assert!(codeyard_db_actions::secrets::delete_secret(
        d.pool(),
        owner,
        repo,
        &longest_name,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&format!(
            "{owner:064x}"
        ))
    )
    .await
    .unwrap());
    assert!(codeyard_db_actions::secrets::delete_secret(
        d.pool(),
        owner,
        repo,
        "_",
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&format!(
            "{owner:064x}"
        ))
    )
    .await
    .unwrap());

    // A repository holds at most what one CI claim carries; replacing a value still works.
    let held = codeyard_db_actions::secrets::list_secrets(d.pool(), owner, repo)
        .await
        .unwrap()
        .len();
    let fill = |index: usize| format!("FILL_{index}");
    for index in held..codeyard_actions::values::MAX_SCOPE_VALUES {
        codeyard_db_actions::secrets::create_or_update_secret(
            d.pool(),
            &encryptor(),
            repo,
            &fill(index),
            "v",
            Some(owner),
            &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&format!(
                "{owner:064x}"
            )),
        )
        .await
        .unwrap();
    }
    let error = codeyard_db_actions::secrets::create_or_update_secret(
        d.pool(),
        &encryptor(),
        repo,
        "ONE_TOO_MANY",
        "v",
        Some(owner),
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&format!(
            "{owner:064x}"
        )),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, SecretsError::TooManySecrets));
    codeyard_db_actions::secrets::create_or_update_secret(
        d.pool(),
        &encryptor(),
        repo,
        &fill(held),
        "replaced",
        Some(owner),
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&format!(
            "{owner:064x}"
        )),
    )
    .await
    .unwrap();
    for index in held..codeyard_actions::values::MAX_SCOPE_VALUES {
        assert!(codeyard_db_actions::secrets::delete_secret(
            d.pool(),
            owner,
            repo,
            &fill(index),
            &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&format!(
                "{owner:064x}"
            ))
        )
        .await
        .unwrap());
    }

    // Reserved names are refused on create and replace. A secret already stored under one must
    // still be listed and deleted, so it can always be removed. The name is set directly in the
    // database because the create path refuses it.
    codeyard_db_actions::secrets::create_or_update_secret(
        d.pool(),
        &encryptor(),
        repo,
        "DEPLOY_KEY",
        "deploy value",
        Some(owner),
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&format!(
            "{owner:064x}"
        )),
    )
    .await
    .unwrap();
    d.execute(
        "UPDATE repo_secrets SET name='GITHUB_DEPLOY_KEY' WHERE repo_id=$1 AND name='DEPLOY_KEY'",
        &[repo],
    )
    .await;
    let rows = codeyard_db_actions::secrets::list_secrets(d.pool(), owner, repo)
        .await
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|secret| secret.name.as_str())
            .collect::<Vec<_>>(),
        ["GITHUB_DEPLOY_KEY"]
    );
    let stored = d.snapshot(repo).await;
    let error = codeyard_db_actions::secrets::create_or_update_secret(
        d.pool(),
        &encryptor(),
        repo,
        "GITHUB_DEPLOY_KEY",
        "replacement must be refused",
        Some(owner),
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&format!(
            "{owner:064x}"
        )),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, SecretsError::ReservedName));
    assert_eq!(d.snapshot(repo).await, stored);
    assert!(codeyard_db_actions::secrets::delete_secret(
        d.pool(),
        owner,
        repo,
        "GITHUB_DEPLOY_KEY",
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&format!(
            "{owner:064x}"
        ))
    )
    .await
    .unwrap());
    assert!(d.snapshot(repo).await.is_empty());
}

async fn contract(pg: bool) {
    let mut d = Database::new(pg).await;
    let owner = d.secret_actor("secret_owner").await;
    let admin = d.secret_actor("secret_admin").await;
    let member = d.secret_actor("secret_member").await;
    let writer = d.secret_actor("secret_writer").await;
    let collaborator = d.secret_actor("secret_collaborator").await;
    let site = d.secret_actor("secret_site").await;
    let outsider = d.secret_actor("secret_outsider").await;
    d.execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[site])
        .await;
    let org = applied(
        codeyard_db_orgs::organizations::create_organization(
            d.pool(),
            owner,
            &codeyard_db_orgs::organizations::NewOrganization::new(
                "secret_org",
                codeyard_db_orgs::organizations::OrganizationProfile::new(None, None).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    for (actor, role) in [(admin, OrgRole::Admin), (member, OrgRole::Member)] {
        applied(
            codeyard_db_orgs::organizations::add_organization_member(
                d.pool(),
                owner,
                org,
                actor,
                role,
            )
            .await
            .unwrap(),
        );
    }
    let personal = d.repo(owner, None, "secret_personal", true).await;
    let private = d.repo(owner, Some(org), "secret_private", true).await;
    let public = d.repo(owner, Some(org), "secret_public", false).await;
    validation_contract(&d, personal, owner).await;
    for repo in [personal, private, public] {
        codeyard_db_repos::repositories::access::grant_repo_access(
            d.pool(),
            writer,
            repo,
            AccessLevel::Write,
        )
        .await
        .unwrap();
        codeyard_db_repos::repositories::access::grant_repo_access(
            d.pool(),
            collaborator,
            repo,
            AccessLevel::Admin,
        )
        .await
        .unwrap();
    }
    for repo in [personal, private, public] {
        allow(&d, repo, owner).await;
        allow(&d, repo, collaborator).await;
        for actor in [writer, member, outsider, -1] {
            refuse(&d, repo, actor).await;
        }
        let before = d.snapshot(repo).await;
        denied(put(d.pool(), repo, None).await);
        denied(
            codeyard_db_actions::secrets::create_or_update_secret(
                d.pool(),
                &encryptor(),
                repo,
                "MISSING_ACTOR",
                VALUE,
                None,
                &codeyard_db_core::queries::users::accounts::AccountCredential::Session(""),
            )
            .await,
        );
        assert_eq!(d.snapshot(repo).await, before);
    }
    allow(&d, personal, site).await; // Site-admin access covers personal repositories only.
    for repo in [private, public] {
        allow(&d, repo, admin).await;
        refuse(&d, repo, site).await; // No organization siteadmin bypass.
    }
    // Updating ciphertext does not erase or adopt the persisted creator.
    codeyard_db_actions::secrets::delete_secret(
        d.pool(),
        owner,
        private,
        NAME,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&format!(
            "{owner:064x}"
        )),
    )
    .await
    .unwrap();
    put(d.pool(), private, Some(owner)).await.unwrap();
    let before = d.snapshot(private).await;
    assert_eq!(before[0].created_by_id, Some(owner));
    codeyard_db_actions::secrets::create_or_update_secret(
        d.pool(),
        &encryptor(),
        private,
        NAME,
        "changed fixture value",
        Some(admin),
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&format!(
            "{admin:064x}"
        )),
    )
    .await
    .unwrap();
    let after = d.snapshot(private).await;
    assert_eq!(after[0].created_by_id, before[0].created_by_id);
    assert_eq!(after[0].created_at, before[0].created_at);
    assert_ne!(after[0].encrypted_value, before[0].encrypted_value);
    assert_ne!(after[0].nonce, before[0].nonce);

    // Cached grants do not survive revocation, demotion or tenant removal.
    codeyard_db_repos::repositories::access::revoke_repo_access(d.pool(), collaborator, private)
        .await
        .unwrap();
    refuse(&d, private, collaborator).await;
    codeyard_db_repos::repositories::access::grant_repo_access(
        d.pool(),
        collaborator,
        private,
        AccessLevel::Admin,
    )
    .await
    .unwrap();
    applied(
        codeyard_db_orgs::organizations::update_organization_member_role(
            d.pool(),
            owner,
            org,
            admin,
            OrgRole::Member,
        )
        .await
        .unwrap(),
    );
    refuse(&d, private, admin).await;
    applied(
        codeyard_db_orgs::organizations::update_organization_member_role(
            d.pool(),
            owner,
            org,
            admin,
            OrgRole::Admin,
        )
        .await
        .unwrap(),
    );
    applied(
        codeyard_db_orgs::organizations::remove_organization_member(d.pool(), owner, org, admin)
            .await
            .unwrap(),
    );
    refuse(&d, private, admin).await;
    applied(
        codeyard_db_orgs::organizations::add_organization_member(
            d.pool(),
            owner,
            org,
            admin,
            OrgRole::Admin,
        )
        .await
        .unwrap(),
    );

    for flag in ["is_banned", "must_change_password"] {
        for actor in [owner, admin, collaborator, site] {
            d.execute(
                &format!("UPDATE users SET {flag}=TRUE WHERE id=$1"),
                &[actor],
            )
            .await;
            for repo in [personal, private, public] {
                refuse(&d, repo, actor).await;
            }
            d.execute(
                &format!("UPDATE users SET {flag}=FALSE WHERE id=$1"),
                &[actor],
            )
            .await;
        }
        // Restricted original governance suspends organization authority even
        // for active Admin/direct Admin actors, but not personal collaboration.
        d.execute(
            &format!("UPDATE users SET {flag}=TRUE WHERE id=$1"),
            &[owner],
        )
        .await;
        for repo in [private, public] {
            for actor in [admin, collaborator, site] {
                refuse(&d, repo, actor).await;
            }
        }
        allow(&d, personal, collaborator).await;
        d.execute(
            &format!("UPDATE users SET {flag}=FALSE WHERE id=$1"),
            &[owner],
        )
        .await;
    }
    // An archived repository refuses every secret write. Its administrators may still list the
    // names, as they may read its other settings.
    for repo in [personal, private, public] {
        d.execute(
            "UPDATE repositories SET is_archived=TRUE WHERE id=$1",
            &[repo],
        )
        .await;
        for actor in [owner, admin, collaborator, site] {
            let administers = actor == owner
                || actor == collaborator
                || actor == if repo == personal { site } else { admin };
            if administers {
                refuse_writes(&d, repo, actor).await;
                assert_eq!(
                    codeyard_db_actions::secrets::list_secrets(d.pool(), actor, repo)
                        .await
                        .unwrap()
                        .len(),
                    1
                );
            } else {
                refuse(&d, repo, actor).await;
            }
        }
        d.execute(
            "UPDATE repositories SET is_archived=FALSE WHERE id=$1",
            &[repo],
        )
        .await;
    }
    // A deleted repository takes its secrets with it and refuses every operation.
    let doomed = d.repo(owner, None, "secret_deleted", true).await;
    allow(&d, doomed, owner).await;
    d.execute("DELETE FROM repositories WHERE id=$1", &[doomed])
        .await;
    assert!(d.snapshot(doomed).await.is_empty());
    for actor in [owner, site] {
        refuse(&d, doomed, actor).await;
    }
    // Exact malformed fixture facts; the production schema is never relaxed.
    d.execute(
        "UPDATE org_memberships SET role='invalid' WHERE org_id=$1 AND user_id=$2",
        &[org, admin],
    )
    .await;
    refuse(&d, private, admin).await;
    for level in [0, 6, 7, -1] {
        d.execute(
            &format!("UPDATE repo_access SET access_level={level} WHERE repo_id=$1 AND user_id=$2"),
            &[personal, collaborator],
        )
        .await;
        refuse(&d, personal, collaborator).await;
    }
    d.execute(
        "UPDATE org_memberships SET role='invalid' WHERE org_id=$1 AND user_id=$2",
        &[org, owner],
    )
    .await;
    refuse(&d, public, collaborator).await;
    refuse(&d, private, owner).await;
    // No account adoption: a vanished original actor cannot be substituted.
    d.execute("DELETE FROM users WHERE id=$1", &[collaborator])
        .await;
    for repo in [personal, private, public] {
        refuse(&d, repo, collaborator).await;
    }
    d.finish().await;
}

#[tokio::test]
async fn postgres_repository_secret_authority() {
    contract(true).await;
}
#[tokio::test]
async fn sqlite_repository_secret_authority() {
    contract(false).await;
}
