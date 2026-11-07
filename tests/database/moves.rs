//! Account moves between two PostgreSQL cells: the freeze, the copy, a failed copy, a
//! repeated copy and the release.
use crate::support::native_database::PostgresDatabase;
use codeyard_db_cluster::moves::{self, Account, State};
use codeyard_model::cells::ObjectKind;
use codeyard_support::cells::Prefix;
use sqlx::PgPool;
use std::collections::BTreeMap;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(30);
const HOLD: Duration = Duration::from_secs(300);

async fn cell(prefix: u16) -> PostgresDatabase {
    let database = PostgresDatabase::new().await;
    codeyard_db_core::cells::initialize(&database.pool, Prefix::new(prefix))
        .await
        .unwrap();
    codeyard_db_core::cells::load(&database.pool).await.unwrap();
    database
}

async fn user(pool: &PgPool, name: &str, home: i32) -> i64 {
    sqlx::query_scalar("INSERT INTO users(username,email,home_cell) VALUES($1,$1||'@example.test',$2) RETURNING id")
        .bind(name)
        .bind(home)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn repository(pool: &PgPool, kind: &str, owner: i64, name: &str, home: i32) -> i64 {
    let (user, organization) = if kind == "user" {
        (Some(owner), None)
    } else {
        (None, Some(owner))
    };
    sqlx::query_scalar("INSERT INTO repositories(owner_kind,owner_user_id,owner_org_id,name,disk_path,home_cell) VALUES($1,$2,$3,$4,'/cells/one/'||$4||'.git',$5) RETURNING id")
        .bind(kind)
        .bind(user)
        .bind(organization)
        .bind(name)
        .bind(home)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn issue(pool: &PgPool, repo: i64, number: i32, author: i64) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO issues(repo_id,number,author_id,title) VALUES($1,$2,$3,'Moving') RETURNING id",
    )
    .bind(repo)
    .bind(number)
    .bind(author)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn comment(pool: &PgPool, issue: i64, author: i64) -> Result<u64, sqlx::Error> {
    sqlx::query("INSERT INTO issue_comments(issue_id,author_id,body) VALUES($1,$2,'hello')")
        .bind(issue)
        .bind(author)
        .execute(pool)
        .await
        .map(|done| done.rows_affected())
}

fn refused_as_moving(error: sqlx::Error) {
    let code = error
        .as_database_error()
        .and_then(|error| error.code())
        .map(|code| code.into_owned());
    assert_eq!(code.as_deref(), Some("55000"), "{error}");
}

async fn count(pool: &PgPool, sql: &str, id: i64) -> i64 {
    sqlx::query_scalar(sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_user_moves_with_its_repositories_and_a_racing_writer_lands_before_the_freeze() {
    let mut one = cell(1).await;
    let mut two = cell(2).await;
    let (source, target) = (&*one.raw, &*two.raw);
    let alice = user(source, "alice", 1).await;
    let bob = user(source, "bob", 1).await;
    let project = repository(source, "user", alice, "project", 1).await;
    let other = repository(source, "user", bob, "other", 1).await;
    let first = issue(source, project, 1, bob).await;
    comment(source, first, bob).await.unwrap();
    sqlx::query("INSERT INTO stars(user_id,repo_id) VALUES($1,$2),($3,$4)")
        .bind(bob)
        .bind(project)
        .bind(alice)
        .bind(other)
        .execute(source)
        .await
        .unwrap();
    sqlx::query("INSERT INTO personal_access_tokens(user_id,name,token_hash,scope) VALUES($1,'cli',repeat('a',64),'write')")
        .bind(alice)
        .execute(source)
        .await
        .unwrap();
    let account = Account::new(ObjectKind::User, alice).unwrap();

    // A transaction older than the move holds it back.
    let mut older = source.begin().await.unwrap();
    sqlx::query("SELECT 1").execute(&mut *older).await.unwrap();
    let held = moves::begin(source, account, 2, Duration::from_secs(1)).await;
    assert!(held.is_err());
    older.commit().await.unwrap();
    assert_eq!(
        moves::begin(source, account, 2, WAIT).await.unwrap(),
        State::Pending
    );

    // A writer that started after the begin keeps the freeze waiting until it commits.
    let mut writer = source.begin().await.unwrap();
    sqlx::query("INSERT INTO issue_comments(issue_id,author_id,body) VALUES($1,$2,'racing')")
        .bind(first)
        .bind(bob)
        .execute(&mut *writer)
        .await
        .unwrap();
    let freezing = tokio::spawn({
        let source = source.clone();
        async move { moves::freeze(&source, account, HOLD, WAIT).await }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!freezing.is_finished());
    writer.commit().await.unwrap();
    freezing.await.unwrap().unwrap();

    refused_as_moving(comment(source, first, bob).await.unwrap_err());
    let unrelated = issue(source, other, 1, bob).await;
    comment(source, unrelated, alice).await.unwrap();

    let paths = BTreeMap::from([(project, "/cells/two/project.git".to_owned())]);
    let copied = moves::copy(source, target, account, 2, &paths)
        .await
        .unwrap();
    assert!(copied.rows >= 5 && copied.stubs >= 1, "{copied:?}");
    // The copy can run again before the switch; it replaces the first one.
    let again = moves::copy(source, target, account, 2, &paths)
        .await
        .unwrap();
    assert_eq!(again.rows, copied.rows);
    refused_as_moving(comment(target, first, alice).await.unwrap_err());
    assert!(moves::activate(source, target, account).await.is_err());
    moves::switch(source, account).await.unwrap();
    moves::activate(source, target, account).await.unwrap();
    moves::activate(source, target, account).await.unwrap();
    assert!(moves::discard(target, account, 1).await.is_err());
    refused_as_moving(comment(source, first, bob).await.unwrap_err());
    moves::release(source, account).await.unwrap();
    moves::release(source, account).await.unwrap();

    let stub =
        "SELECT count(*) FROM users WHERE id=$1 AND is_stub AND home_cell=2 AND email IS NULL";
    assert_eq!(count(source, stub, alice).await, 1);
    assert_eq!(
        count(source, "SELECT count(*) FROM repositories WHERE id=$1 AND is_stub AND home_cell=2 AND disk_path=''", project).await,
        1
    );
    assert_eq!(
        count(
            source,
            "SELECT count(*) FROM issues WHERE repo_id=$1",
            project
        )
        .await,
        0
    );
    assert_eq!(
        count(
            source,
            "SELECT count(*) FROM personal_access_tokens WHERE user_id=$1",
            alice
        )
        .await,
        0
    );
    // Alice's star on Bob's repository stays with that repository.
    assert_eq!(
        count(source, "SELECT count(*) FROM stars WHERE repo_id=$1", other).await,
        1
    );

    assert_eq!(
        count(target, "SELECT count(*) FROM users WHERE id=$1 AND NOT is_stub AND home_cell=2 AND email='alice@example.test'", alice).await,
        1
    );
    assert_eq!(
        count(target, "SELECT count(*) FROM repositories WHERE id=$1 AND NOT is_stub AND home_cell=2 AND disk_path='/cells/two/project.git' AND stars_count=1", project).await,
        1
    );
    assert_eq!(
        count(
            target,
            "SELECT count(*) FROM issue_comments WHERE issue_id=$1",
            first
        )
        .await,
        2
    );
    assert_eq!(
        count(
            target,
            "SELECT count(*) FROM personal_access_tokens WHERE user_id=$1",
            alice
        )
        .await,
        1
    );
    assert_eq!(
        count(
            target,
            "SELECT count(*) FROM users WHERE id=$1 AND is_stub AND home_cell=1",
            bob
        )
        .await,
        1
    );
    // The target takes writes for the account; it has no move of its own.
    comment(target, first, alice).await.unwrap();
    one.finish().await;
    two.finish().await;
}

#[tokio::test]
async fn a_move_that_fails_halfway_leaves_the_source_serving_and_the_freeze_lifts_by_itself() {
    let mut one = cell(1).await;
    let mut two = cell(2).await;
    let (source, target) = (&*one.raw, &*two.raw);
    let carol = user(source, "carol", 1).await;
    let project = repository(source, "user", carol, "tools", 1).await;
    let first = issue(source, project, 1, carol).await;
    // A different account already holds the name in the target, so the load fails there.
    user(target, "carol", 2).await;
    let account = Account::new(ObjectKind::User, carol).unwrap();

    moves::begin(source, account, 2, WAIT).await.unwrap();
    moves::freeze(source, account, HOLD, WAIT).await.unwrap();
    let paths = BTreeMap::from([(project, "/cells/two/tools.git".to_owned())]);
    assert!(moves::copy(source, target, account, 2, &paths)
        .await
        .is_err());
    assert!(moves::cancel(source, account).await.unwrap());
    moves::discard(target, account, 1).await.unwrap();
    comment(source, first, carol).await.unwrap();
    assert_eq!(
        count(
            target,
            "SELECT count(*) FROM repositories WHERE id=$1",
            project
        )
        .await,
        0
    );

    // A freeze nobody finishes ends on its own.
    moves::begin(source, account, 2, WAIT).await.unwrap();
    moves::freeze(source, account, Duration::from_secs(2), WAIT)
        .await
        .unwrap();
    refused_as_moving(comment(source, first, carol).await.unwrap_err());
    tokio::time::sleep(Duration::from_millis(2500)).await;
    comment(source, first, carol).await.unwrap();
    assert!(moves::switch(source, account).await.is_err());
    assert!(moves::cancel(source, account).await.unwrap());
    one.finish().await;
    two.finish().await;
}

#[tokio::test]
async fn a_committed_receiving_copy_stays_frozen_when_the_source_resumes_and_is_replaced_on_retry()
{
    let mut one = cell(1).await;
    let mut two = cell(2).await;
    let (source, target) = (&*one.raw, &*two.raw);
    let alice = user(source, "alice", 1).await;
    let repo = repository(source, "user", alice, "retry", 1).await;
    let first = issue(source, repo, 1, alice).await;
    let account = Account::new(ObjectKind::User, alice).unwrap();
    let paths = BTreeMap::from([(repo, "/cells/two/retry.git".to_owned())]);
    moves::begin(source, account, 2, WAIT).await.unwrap();
    moves::freeze(source, account, HOLD, WAIT).await.unwrap();
    moves::copy(source, target, account, 2, &paths)
        .await
        .unwrap();
    // Model process loss after the target commit, followed by source cancellation.
    assert!(moves::cancel(source, account).await.unwrap());
    comment(source, first, alice).await.unwrap();
    assert!(!moves::cancel(target, account).await.unwrap());
    assert!(moves::activate(source, target, account).await.is_err());
    refused_as_moving(comment(target, first, alice).await.unwrap_err());
    moves::begin(source, account, 2, WAIT).await.unwrap();
    moves::freeze(source, account, HOLD, WAIT).await.unwrap();
    moves::copy(source, target, account, 2, &paths)
        .await
        .unwrap();
    assert_eq!(
        count(
            target,
            "SELECT count(*) FROM issue_comments WHERE issue_id=$1",
            first
        )
        .await,
        1
    );
    moves::switch(source, account).await.unwrap();
    moves::activate(source, target, account).await.unwrap();
    moves::release(source, account).await.unwrap();
    comment(target, first, alice).await.unwrap();
    one.finish().await;
    two.finish().await;
}

#[tokio::test]
async fn a_member_removed_during_an_organization_move_stays_removed() {
    let mut one = cell(1).await;
    let mut two = cell(2).await;
    let (source, target) = (&*one.raw, &*two.raw);
    let dave = user(source, "dave", 1).await;
    let erin = user(source, "erin", 1).await;
    let organization: i64 = sqlx::query_scalar(
        "INSERT INTO organizations(name,home_cell) VALUES('acme',1) RETURNING id",
    )
    .fetch_one(source)
    .await
    .unwrap();
    for member in [dave, erin] {
        sqlx::query("INSERT INTO org_memberships(org_id,user_id) VALUES($1,$2)")
            .bind(organization)
            .bind(member)
            .execute(source)
            .await
            .unwrap();
    }
    let project = repository(source, "org", organization, "site", 1).await;
    let account = Account::new(ObjectKind::Organization, organization).unwrap();

    moves::begin(source, account, 2, WAIT).await.unwrap();
    // Removed before the freeze: the copy never sees the membership.
    sqlx::query("DELETE FROM org_memberships WHERE org_id=$1 AND user_id=$2")
        .bind(organization)
        .bind(erin)
        .execute(source)
        .await
        .unwrap();
    moves::freeze(source, account, HOLD, WAIT).await.unwrap();
    // Removed during the freeze: refused, so it cannot be lost in the copy.
    refused_as_moving(
        sqlx::query("DELETE FROM org_memberships WHERE org_id=$1 AND user_id=$2")
            .bind(organization)
            .bind(dave)
            .execute(source)
            .await
            .unwrap_err(),
    );
    let paths = BTreeMap::from([(project, "/cells/two/site.git".to_owned())]);
    moves::copy(source, target, account, 2, &paths)
        .await
        .unwrap();
    moves::switch(source, account).await.unwrap();
    moves::activate(source, target, account).await.unwrap();
    moves::release(source, account).await.unwrap();

    let members = "SELECT count(*) FROM org_memberships WHERE org_id=$1";
    assert_eq!(count(target, members, organization).await, 1);
    assert_eq!(count(source, members, organization).await, 0);
    let dave_member =
        format!("SELECT count(*) FROM org_memberships WHERE org_id=$1 AND user_id={dave}");
    assert_eq!(count(target, &dave_member, organization).await, 1);
    assert_eq!(
        count(
            target,
            "SELECT count(*) FROM users WHERE id=$1 AND is_stub AND home_cell=1",
            dave
        )
        .await,
        1
    );
    assert_eq!(
        count(
            target,
            "SELECT count(*) FROM users WHERE id=$1 AND is_stub AND home_cell=1",
            erin
        )
        .await,
        0
    );
    one.finish().await;
    two.finish().await;
}
