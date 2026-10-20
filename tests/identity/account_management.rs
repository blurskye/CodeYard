//! Account management contracts on SQLite and PostgreSQL: administrator roles, email changes
//! behind the current password, organization invitations and signed-in sessions.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]

use crate::support::database::Database;
use chrono::{Duration, Utc};
use codeyard::backend::{
    db::{
        self,
        models::OrgRole,
        organizations::{
            NewOrganization, OrganizationOutcome, OrganizationPage, OrganizationProfile,
        },
        users::transitions::{BanChange, RoleChange},
    },
    identity::{self, accounts::EmailChangeError},
};

async fn make_administrator(f: &Database, id: i64) {
    f.execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[id])
        .await;
}

async fn administrator_roles(pg: bool) {
    let mut f = Database::new(pg).await;
    let pool = f.pool();
    let admin = f.actor("admin").await;
    make_administrator(&f, admin).await;
    let second = f.actor("second").await;
    make_administrator(&f, second).await;
    let member = f.actor("member").await;
    let plain = f.actor("plain").await;

    // A plain member changes no role, and nobody changes their own.
    assert!(matches!(
        db::users::transitions::change_admin_role(pool, member, plain, true)
            .await
            .unwrap(),
        RoleChange::Denied
    ));
    assert!(matches!(
        db::users::transitions::change_admin_role(pool, admin, admin, false)
            .await
            .unwrap(),
        RoleChange::SelfChange
    ));
    assert!(matches!(
        db::users::transitions::change_admin_role(pool, admin, 9_999_999, true)
            .await
            .unwrap(),
        RoleChange::Missing
    ));

    // An administrator grants the role, then removes another administrator's role.
    assert!(matches!(
        db::users::transitions::change_admin_role(pool, admin, plain, true)
            .await
            .unwrap(),
        RoleChange::Updated(user) if user.is_admin
    ));
    assert!(matches!(
        db::users::transitions::change_admin_role(pool, admin, second, false)
            .await
            .unwrap(),
        RoleChange::Updated(user) if !user.is_admin
    ));

    // Self-ban is refused; banning another administrator leaves the acting one in charge.
    assert!(matches!(
        db::users::transitions::change_ban(pool, admin, admin, true)
            .await
            .unwrap(),
        BanChange::SelfBan
    ));
    assert!(matches!(
        db::users::transitions::change_ban(pool, admin, plain, true)
            .await
            .unwrap(),
        BanChange::Updated(_)
    ));
    assert!(matches!(
        db::users::transitions::change_ban(pool, plain, admin, true)
            .await
            .unwrap(),
        BanChange::Denied
    ));

    // Browser operations keep the initiating session, not just the administrator ID.
    f.execute("INSERT INTO node_configuration(id,node_mode,setup_completed) VALUES(1,'private',TRUE) ON CONFLICT(id) DO UPDATE SET node_mode='private',setup_completed=TRUE", &[]).await;
    db::users::sessions::create_session(
        pool,
        "admin-original",
        admin,
        None,
        None,
        Utc::now() + Duration::hours(1),
    )
    .await
    .unwrap();
    let created = db::users::transitions::register_password(
        pool,
        "managed-admin",
        None,
        "temporary-hash",
        Some((admin, "admin-original", true)),
    )
    .await
    .unwrap();
    assert!(
        matches!(created, db::users::transitions::AccountAdmission::User(user) if user.is_admin && user.must_change_password)
    );
    db::users::sessions::delete_session(pool, "admin-original")
        .await
        .unwrap();
    assert!(matches!(
        db::users::transitions::change_admin_role(
            pool,
            (admin, "admin-original".to_owned()),
            second,
            true
        )
        .await
        .unwrap(),
        RoleChange::Denied
    ));
    assert!(matches!(
        db::users::transitions::change_ban(
            pool,
            (admin, "admin-original".to_owned()),
            member,
            true
        )
        .await
        .unwrap(),
        BanChange::Denied
    ));
    assert!(matches!(
        db::users::transitions::register_password(
            pool,
            "not-created",
            None,
            "hash",
            Some((admin, "admin-original", true))
        )
        .await
        .unwrap(),
        db::users::transitions::AccountAdmission::Denied
    ));
    assert!(
        db::users::accounts::get_user_by_username(pool, "not-created")
            .await
            .unwrap()
            .is_none()
    );
    let pending = db::users::signup_requests::create_signup_request(pool, "pending", None, "hash")
        .await
        .unwrap();
    assert!(db::users::signup_requests::approve_signup_request(
        pool,
        pending.id,
        (admin, "admin-original".to_owned())
    )
    .await
    .is_err());
    assert!(db::users::signup_requests::reject_signup_request(
        pool,
        pending.id,
        (admin, "admin-original".to_owned())
    )
    .await
    .is_err());
    assert_eq!(
        db::users::signup_requests::list_signup_requests(pool, "pending", 100, 0, 0)
            .await
            .unwrap()
            .len(),
        1
    );

    f.finish().await;
}

async fn email_changes(pg: bool) {
    let mut f = Database::new(pg).await;
    let pool = f.pool();
    let hash = identity::passwords::hash_password("correct-password")
        .await
        .unwrap();
    let user = db::users::accounts::create_user_with_password(
        pool,
        "mailer",
        Some("old@example.test"),
        &hash,
    )
    .await
    .unwrap();
    db::users::accounts::create_user_with_password(
        pool,
        "taken",
        Some("taken@example.test"),
        &hash,
    )
    .await
    .unwrap();
    db::users::sessions::create_session(
        pool,
        "email-session",
        user.id,
        None,
        None,
        Utc::now() + Duration::days(1),
    )
    .await
    .unwrap();

    // A wrong password changes nothing.
    assert!(matches!(
        identity::accounts::change_email(
            pool,
            Some("email-session"),
            "wrong-password",
            "new@example.test"
        )
        .await,
        Err(EmailChangeError::InvalidCredentials)
    ));
    let stored = db::users::accounts::get_user_by_id(pool, user.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.email.as_deref(), Some("old@example.test"));

    // Malformed and taken addresses are refused, with the right password.
    assert!(matches!(
        identity::accounts::change_email(
            pool,
            Some("email-session"),
            "correct-password",
            "bad\0@example.test"
        )
        .await,
        Err(EmailChangeError::InvalidEmail)
    ));
    assert!(matches!(
        identity::accounts::change_email(
            pool,
            Some("email-session"),
            "correct-password",
            "not-an-address"
        )
        .await,
        Err(EmailChangeError::InvalidEmail)
    ));
    assert!(matches!(
        identity::accounts::change_email(
            pool,
            Some("email-session"),
            "correct-password",
            "taken@example.test"
        )
        .await,
        Err(EmailChangeError::Taken)
    ));

    f.execute(
        "UPDATE users SET email_verified=TRUE WHERE id=$1",
        &[user.id],
    )
    .await;
    let saved = identity::accounts::change_email(
        pool,
        Some("email-session"),
        "correct-password",
        "  new@example.test ",
    )
    .await
    .unwrap();
    assert_eq!(saved.email.as_deref(), Some("new@example.test"));
    assert!(
        !saved.email_verified,
        "verification must not survive an address change"
    );
    // A request that verified a snapshot must retain its original credentials and session.
    db::users::sessions::delete_session(pool, "email-session")
        .await
        .unwrap();
    assert!(matches!(
        db::users::transitions::change_email(
            pool,
            user.id,
            "stale@example.test",
            &hash,
            "email-session"
        )
        .await
        .unwrap(),
        db::users::transitions::EmailChange::Missing
    ));
    assert!(db::users::transitions::compare_exchange_password(
        pool,
        user.id,
        &hash,
        "replacement",
        true,
        true,
        Some("email-session")
    )
    .await
    .unwrap()
    .is_none());
    db::users::sessions::create_session(
        pool,
        "replacement-session",
        user.id,
        None,
        None,
        Utc::now() + Duration::days(1),
    )
    .await
    .unwrap();
    let changed = db::users::transitions::compare_exchange_password(
        pool,
        user.id,
        &hash,
        "replacement",
        true,
        true,
        Some("replacement-session"),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(changed.password_hash.as_deref(), Some("replacement"));
    assert!(!changed.must_change_password);
    assert!(matches!(
        db::users::transitions::change_email(
            pool,
            user.id,
            "stale@example.test",
            &hash,
            "replacement-session"
        )
        .await
        .unwrap(),
        db::users::transitions::EmailChange::Missing
    ));

    f.finish().await;
}

async fn sessions_belong_to_their_account(pg: bool) {
    let mut f = Database::new(pg).await;
    let pool = f.pool();
    let a = f.actor("sessions-a").await;
    let b = f.actor("sessions-b").await;
    let expires = Utc::now() + Duration::days(1);
    for id in ["a-one", "a-two"] {
        db::users::sessions::create_session(pool, id, a, None, None, expires)
            .await
            .unwrap();
    }
    db::users::sessions::create_session(pool, "b-one", b, None, None, expires)
        .await
        .unwrap();
    assert_eq!(
        db::users::sessions::list_user_sessions(pool, a)
            .await
            .unwrap()
            .len(),
        2
    );

    // Revoking another account's session does nothing.
    assert!(!db::users::sessions::revoke_user_session(pool, a, "b-one")
        .await
        .unwrap());
    assert!(db::users::sessions::get_session(pool, "b-one")
        .await
        .unwrap()
        .is_some());

    assert!(db::users::sessions::revoke_user_session(pool, a, "a-one")
        .await
        .unwrap());
    // Sign out everywhere else keeps the named session and leaves the other account alone.
    assert_eq!(
        db::users::sessions::revoke_other_user_sessions(pool, a, "a-two")
            .await
            .unwrap(),
        0
    );
    db::users::sessions::create_session(pool, "a-three", a, None, None, expires)
        .await
        .unwrap();
    assert_eq!(
        db::users::sessions::revoke_other_user_sessions(pool, a, "a-two")
            .await
            .unwrap(),
        1
    );
    let left = db::users::sessions::list_user_sessions(pool, a)
        .await
        .unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, "a-two");
    assert_eq!(
        db::users::sessions::list_user_sessions(pool, b)
            .await
            .unwrap()
            .len(),
        1
    );

    f.finish().await;
}

/// A session that recorded its address lists with that address, on both backends.
async fn session_list_shows_recorded_addresses(pg: bool) {
    let mut f = Database::new(pg).await;
    let pool = f.pool();
    let a = f.actor("sessions-address").await;
    let expires = Utc::now() + Duration::days(1);
    db::users::sessions::create_session(
        pool,
        "with-address",
        a,
        Some("Firefox"),
        Some("203.0.113.7"),
        expires,
    )
    .await
    .unwrap();
    let listed = db::users::sessions::list_user_sessions(pool, a)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    // SQLite keeps the prefix of the stored address, and PostgreSQL leaves out a host prefix.
    let address = listed[0].ip_address.as_deref().unwrap_or_default();
    assert_eq!(address.trim_end_matches("/32"), "203.0.113.7");
    f.finish().await;
}

fn organization(
    outcome: OrganizationOutcome<db::organizations::OrganizationMetadata>,
) -> db::organizations::OrganizationMetadata {
    match outcome {
        OrganizationOutcome::Applied(metadata) => metadata,
        _ => panic!("the owner creates the organization"),
    }
}

async fn invitations(pg: bool) {
    let mut f = Database::new(pg).await;
    let pool = f.pool();
    let owner = f.actor("invite-owner").await;
    let invitee = f.actor("invitee").await;
    let org_admin = f.actor("invite-admin").await;
    let org = organization(
        db::organizations::create_organization(
            pool,
            owner,
            &NewOrganization::new("invite-org", OrganizationProfile::new(None, None).unwrap())
                .unwrap(),
        )
        .await
        .unwrap(),
    );
    let page = OrganizationPage::new(0, 100).unwrap();

    // The invitation is pending: the invitee is not a member until they accept.
    assert!(matches!(
        db::organizations::invite_organization_member(
            pool,
            owner,
            org.id,
            invitee,
            OrgRole::Member
        )
        .await
        .unwrap(),
        OrganizationOutcome::Applied(_)
    ));
    assert!(matches!(
        db::organizations::get_organization_role(pool, invitee, org.id)
            .await
            .unwrap(),
        OrganizationOutcome::Denied
    ));
    // Someone already invited, and an existing member, are refused.
    assert!(matches!(
        db::organizations::invite_organization_member(
            pool,
            owner,
            org.id,
            invitee,
            OrgRole::Member
        )
        .await
        .unwrap(),
        OrganizationOutcome::Conflict
    ));
    assert!(matches!(
        db::organizations::invite_organization_member(pool, owner, org.id, owner, OrgRole::Member)
            .await
            .unwrap(),
        OrganizationOutcome::Conflict
    ));
    // The owner sees the pending invitation; the invitee sees it on their own list.
    match db::organizations::list_organization_invitations(pool, owner, org.id, page)
        .await
        .unwrap()
    {
        OrganizationOutcome::Applied(rows) => assert_eq!(rows.len(), 1),
        _ => panic!("the owner lists invitations"),
    }
    let received = db::organizations::list_received_organization_invitations(
        pool,
        invitee,
        OrganizationPage::new(0, 100).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].org_name, "invite-org");

    // Declining removes it, and a second answer finds nothing.
    assert!(matches!(
        db::organizations::decline_organization_invitation(pool, invitee, org.id)
            .await
            .unwrap(),
        OrganizationOutcome::Applied(())
    ));
    assert!(matches!(
        db::organizations::decline_organization_invitation(pool, invitee, org.id)
            .await
            .unwrap(),
        OrganizationOutcome::Missing
    ));

    // Cancelling withdraws the offer; only an owner or admin can do it.
    db::organizations::invite_organization_member(pool, owner, org.id, invitee, OrgRole::Member)
        .await
        .unwrap();
    assert!(matches!(
        db::organizations::cancel_organization_invitation(pool, invitee, org.id, invitee)
            .await
            .unwrap(),
        OrganizationOutcome::Denied
    ));
    assert!(matches!(
        db::organizations::cancel_organization_invitation(pool, owner, org.id, invitee)
            .await
            .unwrap(),
        OrganizationOutcome::Applied(())
    ));
    assert!(matches!(
        db::organizations::cancel_organization_invitation(pool, owner, org.id, invitee)
            .await
            .unwrap(),
        OrganizationOutcome::Missing
    ));

    // Accepting makes the member, once.
    db::organizations::invite_organization_member(pool, owner, org.id, invitee, OrgRole::Member)
        .await
        .unwrap();
    assert!(matches!(
        db::organizations::accept_organization_invitation(pool, invitee, org.id)
            .await
            .unwrap(),
        OrganizationOutcome::Applied(member) if member.role == "member"
    ));
    assert!(matches!(
        db::organizations::get_organization_role(pool, invitee, org.id)
            .await
            .unwrap(),
        OrganizationOutcome::Applied(OrgRole::Member)
    ));
    assert!(matches!(
        db::organizations::accept_organization_invitation(pool, invitee, org.id)
            .await
            .unwrap(),
        OrganizationOutcome::Missing
    ));

    // An administrator may invite only plain members, not another administrator.
    db::organizations::invite_organization_member(pool, owner, org.id, org_admin, OrgRole::Admin)
        .await
        .unwrap();
    db::organizations::accept_organization_invitation(pool, org_admin, org.id)
        .await
        .unwrap();
    assert!(matches!(
        db::organizations::invite_organization_member(
            pool,
            org_admin,
            org.id,
            invitee,
            OrgRole::Admin
        )
        .await
        .unwrap(),
        OrganizationOutcome::Denied
    ));

    f.finish().await;
}

#[tokio::test]
async fn sqlite_administrator_roles_keep_one_acting_administrator() {
    administrator_roles(false).await;
}

#[tokio::test]
async fn postgres_administrator_roles_keep_one_acting_administrator() {
    administrator_roles(true).await;
}

#[tokio::test]
async fn sqlite_email_changes_need_the_current_password_and_a_free_address() {
    email_changes(false).await;
}

#[tokio::test]
async fn postgres_email_changes_need_the_current_password_and_a_free_address() {
    email_changes(true).await;
}

#[tokio::test]
async fn sqlite_session_revoke_touches_only_your_own_sessions() {
    sessions_belong_to_their_account(false).await;
}

#[tokio::test]
async fn postgres_session_revoke_touches_only_your_own_sessions() {
    sessions_belong_to_their_account(true).await;
}

#[tokio::test]
async fn sqlite_session_list_shows_recorded_addresses() {
    session_list_shows_recorded_addresses(false).await;
}

#[tokio::test]
async fn postgres_session_list_shows_recorded_addresses() {
    session_list_shows_recorded_addresses(true).await;
}

#[tokio::test]
async fn sqlite_invitations_start_membership_only_on_accept() {
    invitations(false).await;
}

#[tokio::test]
async fn postgres_invitations_start_membership_only_on_accept() {
    invitations(true).await;
}
