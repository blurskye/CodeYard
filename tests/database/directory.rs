//! Separate-directory operator commands and atomic claims across two cells.
use crate::support::native_database::PostgresDatabase;
use codeyard_db_directory::{Config, Directory};
use codeyard_types::cell_ids::Prefix;
use codeyard_types::cells::{Cell, Key, KeyKind, Object, ObjectKind, State};
use std::{path::Path, process::Output, time::Duration};

fn cell(prefix: u16, key: char) -> Cell {
    Cell {
        id: Prefix::new(prefix),
        name: format!("cell-{prefix}"),
        region: "a".into(),
        state: State::Open,
        web_urls: vec![format!("http://127.0.0.1:{}/", 10000 + u32::from(prefix))],
        ssh_addresses: vec![format!("127.0.0.1:{}", 20000 + u32::from(prefix))],
        rpc_addresses: vec![],
        node_keys: vec![key.to_string().repeat(64)],
        database_bytes: 0,
        write_rate: 0,
        maximum_database_bytes: 1000,
        maximum_write_rate: 100,
        read_regions: Vec::new(),
    }
}

async fn cli(root: &Path, url: &str, arguments: &[&str]) -> Output {
    cli_with(root, url, &[], arguments).await
}

async fn cli_with(root: &Path, url: &str, env: &[(&str, &str)], arguments: &[&str]) -> Output {
    tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_codeyard"))
            .args(["cell", "directory"])
            .args(arguments)
            .current_dir(root)
            .env_clear()
            .envs(env.iter().copied())
            .env("DIRECTORY_DATABASE_URL", url)
            // The directory command must neither open the cell DB nor prepare local state.
            .env("DATABASE_URL", "refused://must-not-open")
            .env("GITSERV_STATE_DIR", root.join("missing/state"))
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap()
}

#[tokio::test]
async fn directory_cli_owns_a_separate_baseline_and_bounds_pages_and_placement() {
    let mut database = PostgresDatabase::unmigrated().await;
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(".env"), "").unwrap();
    let output = cli(root.path(), &database.connection_url(), &["initialize"]).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!root.path().join("missing").exists());
    let cell_table: Option<String> = sqlx::query_scalar("SELECT to_regclass('users')::text")
        .fetch_one(&database.raw)
        .await
        .unwrap();
    assert_eq!(cell_table, None);
    let a = "a".repeat(64);
    let b = "b".repeat(64);
    for (prefix, name, key) in [("0", "first", a.as_str()), ("1", "second", b.as_str())] {
        let output = cli(
            root.path(),
            &database.connection_url(),
            &[
                "register",
                "--prefix",
                prefix,
                "--name",
                name,
                "--region",
                "a",
                "--web-url",
                "http://127.0.0.1:3000/",
                "--node-key",
                key,
                "--maximum-database-bytes",
                "1000",
                "--maximum-write-rate",
                "100",
                "--open",
            ],
        )
        .await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let end = Prefix::ZERO.ids().end().to_string();
    for _ in 0..2 {
        let output = cli(
            root.path(),
            &database.connection_url(),
            &[
                "allocate-prefix",
                "--node-key",
                &a,
                "--previous",
                "0",
                "--last-id",
                &end,
            ],
        )
        .await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "2");
    }
    let output = cli(
        root.path(),
        &database.connection_url(),
        &["list", "--after", "0", "--limit", "1"],
    )
    .await;
    assert!(output.status.success());
    let listed: Vec<Cell> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, Prefix::new(1));
    assert!(!cli(
        root.path(),
        &database.connection_url(),
        &["list", "--limit", "101"]
    )
    .await
    .status
    .success());
    let directory = Directory::connect(&database.connection_url(), Config::default())
        .await
        .unwrap();
    assert_eq!(
        directory.choose_cell("a").await.unwrap().unwrap().id,
        Prefix::ZERO
    );
    directory.report_load(&a, 500, 0).await.unwrap();
    assert_eq!(
        directory.choose_cell("a").await.unwrap().unwrap().id,
        Prefix::new(1)
    );
    directory.report_load(&b, 0, 50).await.unwrap();
    assert!(directory.choose_cell("a").await.unwrap().is_none());
    let mut collision = cell(3, 'b');
    collision.name = "collision".into();
    assert!(directory.register_cell(collision).await.is_err());
    assert!(
        directory.cell(Prefix::new(3)).await.unwrap().is_none(),
        "key conflict rolls back the cell too"
    );
    directory.close().await;
    database.finish().await;
}

#[tokio::test]
async fn pending_directory_keys_activate_together_and_revocation_is_checked_again() {
    let mut database = PostgresDatabase::unmigrated().await;
    let directory = Directory::connect(&database.connection_url(), Config::default())
        .await
        .unwrap();
    directory.migrate().await.unwrap();
    for (prefix, key) in [(0, 'a'), (1, 'b')] {
        directory.register_cell(cell(prefix, key)).await.unwrap();
    }
    let peer = "a".repeat(64);
    let other_peer = "b".repeat(64);
    let nonce = "c".repeat(64);
    let object = Object {
        kind: ObjectKind::User,
        id: 1,
    };
    let name = Key::new(KeyKind::Name, "Alice").unwrap();
    let mail = Key::new(KeyKind::Email, "Alice@example.com").unwrap();
    let keys = vec![name.clone(), mail.clone()];
    directory
        .reserve(&peer, object, &keys, &nonce)
        .await
        .unwrap();
    assert!(directory.lookup(&name).await.unwrap().is_none());
    assert!(directory.lookup(&mail).await.unwrap().is_none());
    // A partial retry may not publish a subset of the original object.
    assert!(directory
        .confirm(&peer, object, std::slice::from_ref(&name), &nonce)
        .await
        .is_err());
    assert!(directory.lookup(&name).await.unwrap().is_none());
    assert!(directory
        .confirm(&other_peer, object, &keys, &nonce)
        .await
        .is_err());
    let mut revoked = cell(0, 'd');
    revoked.state = State::Closed;
    directory.register_cell(revoked).await.unwrap();
    assert!(directory
        .confirm(&peer, object, &keys, &nonce)
        .await
        .is_err());
    assert!(directory.report_load(&peer, 0, 0).await.is_err());
    assert!(directory.lookup(&name).await.unwrap().is_none());
    let replacement = "d".repeat(64);
    directory
        .confirm(&replacement, object, &keys, &nonce)
        .await
        .unwrap();
    directory
        .confirm(&replacement, object, &keys, &nonce)
        .await
        .unwrap();
    let location = directory
        .lookup(&Key::new(KeyKind::Name, "aLiCe").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(location.object, object);
    assert_eq!(location.cell, Prefix::ZERO);
    // A foreign cell cannot claim the same global name; all its other reservations roll back.
    let foreign = Object {
        kind: ObjectKind::User,
        id: *Prefix::new(1).ids().start(),
    };
    let unique_mail = Key::new(KeyKind::Email, "other@example.com").unwrap();
    assert!(directory
        .reserve(
            &other_peer,
            foreign,
            &[name.clone(), unique_mail.clone()],
            &nonce
        )
        .await
        .is_err());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM directory_claims WHERE object_id=$1")
        .bind(foreign.id)
        .fetch_one(&database.raw)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert!(directory.lookup(&unique_mail).await.unwrap().is_none());
    assert!(directory
        .reserve(
            &replacement,
            Object {
                kind: ObjectKind::User,
                id: 2
            },
            &keys,
            "hostile"
        )
        .await
        .is_err());
    assert!(Key::new(KeyKind::Ssh, "SHA256:../../keys").is_err());
    assert!(Key::new(KeyKind::Name, "../alice").is_err());
    directory.close().await;
    database.finish().await;
}

#[tokio::test]
async fn expired_directory_claims_do_not_publish_and_cleanup_releases_the_name() {
    let mut database = PostgresDatabase::unmigrated().await;
    let directory = Directory::connect(&database.connection_url(), Config::default())
        .await
        .unwrap();
    directory.migrate().await.unwrap();
    directory.register_cell(cell(0, 'a')).await.unwrap();
    let peer = "a".repeat(64);
    let nonce = "e".repeat(64);
    let old = Object {
        kind: ObjectKind::User,
        id: 1,
    };
    let keys = vec![Key::new(KeyKind::Name, "interrupted").unwrap()];
    directory.reserve(&peer, old, &keys, &nonce).await.unwrap();
    sqlx::query(
        "UPDATE directory_objects SET expires_at=clock_timestamp()-INTERVAL '1 second' WHERE id=$1",
    )
    .bind(old.id)
    .execute(&database.raw)
    .await
    .unwrap();
    assert!(directory.confirm(&peer, old, &keys, &nonce).await.is_err());
    directory.collect().await.unwrap();
    let new = Object {
        kind: ObjectKind::User,
        id: 2,
    };
    directory.reserve(&peer, new, &keys, &nonce).await.unwrap();
    directory.confirm(&peer, new, &keys, &nonce).await.unwrap();
    assert_eq!(
        directory.lookup(&keys[0]).await.unwrap().unwrap().object,
        new
    );
    directory.close().await;
    database.finish().await;
}

#[tokio::test]
async fn successor_prefixes_are_atomic_owned_and_retry_the_same_reservation() {
    let mut database = PostgresDatabase::unmigrated().await;
    let directory = Directory::connect(&database.connection_url(), Config::default())
        .await
        .unwrap();
    directory.migrate().await.unwrap();
    directory.register_cell(cell(1, 'a')).await.unwrap();
    directory.register_cell(cell(3, 'b')).await.unwrap();
    let first = Prefix::new(1);
    let other = Prefix::new(3);
    let a = "a".repeat(64);
    let b = "b".repeat(64);
    assert!(directory
        .next_prefix(&a, first, *first.ids().start())
        .await
        .is_err());
    assert!(directory
        .next_prefix(&a, first, *other.ids().end())
        .await
        .is_err());
    assert!(directory
        .next_prefix(&b, first, *first.ids().end())
        .await
        .is_err());
    let capacity = codeyard_types::cell_ids::IDS_PER_PREFIX - 1;
    let threshold = *first.ids().start() + capacity - capacity / 5 - 1;
    assert!(directory
        .next_prefix(&a, first, threshold - 1)
        .await
        .is_err());
    let (one, retry, two) = tokio::join!(
        directory.next_prefix(&a, first, threshold),
        directory.next_prefix(&a, first, threshold),
        directory.next_prefix(&b, other, *other.ids().end()),
    );
    let one = one.unwrap();
    let two = two.unwrap();
    assert_eq!(one, retry.unwrap());
    assert_ne!(one, two);
    assert!(one != Prefix::ZERO && two != Prefix::ZERO);
    assert_eq!(directory.cell(one).await.unwrap().unwrap().id, first);
    assert_eq!(directory.cell(two).await.unwrap().unwrap().id, other);
    let object = Object {
        kind: ObjectKind::User,
        id: *one.ids().start(),
    };
    let keys = [Key::new(KeyKind::Name, "later-range").unwrap()];
    directory
        .reserve(&a, object, &keys, &"c".repeat(64))
        .await
        .unwrap();
    directory
        .confirm(&a, object, &keys, &"c".repeat(64))
        .await
        .unwrap();
    assert_eq!(directory.object_home(object).await.unwrap(), Some(first));
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM directory_cell_prefixes")
        .fetch_one(&database.raw)
        .await
        .unwrap();
    assert_eq!(count, 4);
    // Registering another cell cannot take a prefix already reserved by a cell.
    assert!(directory.register_cell(cell(one.get(), 'd')).await.is_err());
    assert_eq!(directory.cell(one).await.unwrap().unwrap().id, first);
    directory.close().await;
    database.finish().await;
}

fn user(id: i64) -> Object {
    Object {
        kind: ObjectKind::User,
        id,
    }
}

fn name_in(shard: i16, after: usize) -> (usize, Key) {
    (after..)
        .map(|index| {
            (
                index,
                Key::new(KeyKind::Name, &format!("probe-{index}")).unwrap(),
            )
        })
        .find(|(_, key)| key.shard() == shard)
        .unwrap()
}

async fn rows_in(pool: &codeyard_database::pool::PgPool, shard: i16) -> (i64, i64) {
    sqlx::query_as("SELECT (SELECT COUNT(*) FROM directory_objects WHERE shard=$1),(SELECT COUNT(*) FROM directory_claims WHERE shard=$1)")
        .bind(shard)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn directory_shards_move_between_clusters_with_no_failed_lookup() {
    let mut registry = PostgresDatabase::unmigrated().await;
    let mut second = PostgresDatabase::unmigrated().await;
    let mut directory = Directory::connect(&registry.connection_url(), Config::default())
        .await
        .unwrap();
    assert!(directory
        .add_cluster("main", &second.connection_url())
        .await
        .is_err());
    assert!(directory
        .add_cluster("../x", &second.connection_url())
        .await
        .is_err());
    directory
        .add_cluster("second", &second.connection_url())
        .await
        .unwrap();
    directory.migrate().await.unwrap();
    directory.register_cell(cell(0, 'a')).await.unwrap();
    let peer = "a".repeat(64);
    let nonce = "c".repeat(64);
    let mut people = Vec::new();
    for id in 1..=40_i64 {
        let keys = vec![
            Key::new(KeyKind::Name, &format!("person-{id}")).unwrap(),
            Key::new(KeyKind::Email, &format!("person-{id}@example.com")).unwrap(),
        ];
        directory
            .reserve(&peer, user(id), &keys, &nonce)
            .await
            .unwrap();
        directory
            .confirm(&peer, user(id), &keys, &nonce)
            .await
            .unwrap();
        people.push((user(id), keys));
    }
    let moved = people[0].1[0].shard();
    let home = people[0].0.shard();
    assert!(rows_in(&registry.raw, moved).await.1 > 0);

    // Lookups run during every step below and must never fail or miss.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = tokio::spawn({
        let directory = directory.clone();
        let people = people.clone();
        let stop = stop.clone();
        async move {
            let mut rounds = 0_u64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                for (object, keys) in &people {
                    for key in keys {
                        let found = directory.lookup(key).await.unwrap().unwrap();
                        assert_eq!(found.object, *object);
                        assert_eq!(found.cell, Prefix::ZERO);
                    }
                    assert_eq!(
                        directory.object_home(*object).await.unwrap(),
                        Some(Prefix::ZERO)
                    );
                }
                rounds += 1;
            }
            rounds
        }
    });

    // A shard being copied refuses writes but keeps answering lookups.
    sqlx::query("UPDATE directory_shards SET state='moving',target='second' WHERE shard=$1")
        .bind(moved)
        .execute(&registry.raw)
        .await
        .unwrap();
    let (index, blocked) = name_in(moved, 0);
    assert!(directory
        .reserve(&peer, user(100), std::slice::from_ref(&blocked), &nonce)
        .await
        .is_err());
    assert!(directory.forget(&peer, people[0].0).await.is_err());
    assert!(directory.lookup(&people[0].1[0]).await.unwrap().is_some());
    assert!(directory.move_shard(moved, "elsewhere").await.is_err());
    // The command resumes the move it finds, then removes the old copy.
    let result = directory.move_shard(moved, "second").await.unwrap();
    assert_eq!(result.from.as_deref(), Some("main"));
    assert!(result.claims > 0);
    assert_eq!(rows_in(&registry.raw, moved).await, (0, 0));
    assert_eq!(
        rows_in(&second.raw, moved).await,
        (result.objects, result.claims)
    );
    if home != moved {
        let result = directory.move_shard(home, "second").await.unwrap();
        assert!(result.objects > 0);
        assert_eq!(rows_in(&registry.raw, home).await, (0, 0));
    }
    // Moving again to the same cluster changes nothing.
    assert_eq!(
        directory.move_shard(moved, "second").await.unwrap().objects,
        0
    );

    // Writes work on the new cluster, including one that spans both clusters.
    let keys = vec![
        blocked.clone(),
        Key::new(KeyKind::Email, &format!("probe-{index}@example.com")).unwrap(),
    ];
    directory
        .reserve(&peer, user(100), &keys, &nonce)
        .await
        .unwrap();
    assert!(directory.lookup(&blocked).await.unwrap().is_none());
    assert!(directory
        .confirm(&peer, user(100), std::slice::from_ref(&blocked), &nonce)
        .await
        .is_err());
    directory
        .confirm(&peer, user(100), &keys, &nonce)
        .await
        .unwrap();
    for key in &keys {
        assert_eq!(
            directory.lookup(key).await.unwrap().unwrap().object,
            user(100)
        );
    }
    // The same name is refused for another account in the other cell's range too.
    directory.register_cell(cell(1, 'b')).await.unwrap();
    assert!(directory
        .reserve(
            &"b".repeat(64),
            user(*Prefix::new(1).ids().start()),
            std::slice::from_ref(&blocked),
            &nonce
        )
        .await
        .is_err());
    directory
        .release(&peer, user(100), std::slice::from_ref(&keys[1]))
        .await
        .unwrap();
    assert!(directory.lookup(&keys[1]).await.unwrap().is_none());
    directory.forget(&peer, user(100)).await.unwrap();
    assert!(directory.lookup(&blocked).await.unwrap().is_none());
    assert_eq!(
        directory.object_home(user(100)).await.unwrap(),
        Some(Prefix::ZERO)
    );

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(reader.await.unwrap() > 0);

    // The operator commands need every cluster the map names.
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(".env"), "").unwrap();
    let output = cli(root.path(), &registry.connection_url(), &["shards"]).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let listed: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(listed.len(), 256);
    assert_eq!(listed[moved as usize]["cluster"], "second");
    let output = cli(
        root.path(),
        &registry.connection_url(),
        &["move-shard", "--shard", &moved.to_string(), "--to", "main"],
    )
    .await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("second"));
    let second_url = format!("second={}", second.connection_url());
    let output = cli_with(
        root.path(),
        &registry.connection_url(),
        &[("DIRECTORY_SHARD_DATABASE_URLS", &second_url)],
        &["move-shard", "--shard", &moved.to_string(), "--to", "main"],
    )
    .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(rows_in(&second.raw, moved).await, (0, 0));
    assert!(directory.lookup(&people[0].1[0]).await.unwrap().is_some());
    directory.close().await;
    registry.finish().await;
    second.finish().await;
}

#[tokio::test]
async fn directory_search_comments_are_bounded_and_pending_reviews_stay_private() {
    use codeyard_database::cells::search::{self, Item};
    let mut database = PostgresDatabase::new().await;
    let user = codeyard_db_identity::users::accounts::create_user(&database.pool, "indexer", None)
        .await
        .unwrap();
    let repo = codeyard_db_repos::repositories::records::create_repository(
        &database.pool,
        user.id,
        "comments",
        None,
        false,
        "/unused/search.git",
    )
    .await
    .unwrap();
    sqlx::query("INSERT INTO node_configuration(id,search_index) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET search_index=TRUE")
        .execute(&database.raw).await.unwrap();
    let issue: i64 = sqlx::query_scalar(
        "INSERT INTO issues(repo_id,number,author_id,title) VALUES($1,1,$2,'Issue') RETURNING id",
    )
    .bind(repo.id)
    .bind(user.id)
    .fetch_one(&database.raw)
    .await
    .unwrap();
    sqlx::query("INSERT INTO issue_comments(issue_id,author_id,body) SELECT $1,$2,CASE WHEN n=1 THEN 'oldest' ELSE repeat('é',2000) END FROM generate_series(1,101) n ORDER BY n")
        .bind(issue).bind(user.id).execute(&database.raw).await.unwrap();
    let text = search::comment_text(&database.raw, Item::Issue { id: issue })
        .await
        .unwrap();
    assert_eq!(text.lines().count(), 100);
    assert!(text.lines().all(|line| line.chars().count() == 1024));
    assert!(!text.contains("oldest"));
    assert!(text.len() <= search::MAXIMUM_COMMENT_BYTES);
    let pull: i64 = sqlx::query_scalar("INSERT INTO pull_requests(repo_id,number,author_id,title,head_branch,base_branch,head_sha,base_sha) VALUES($1,2,$2,'Pull','branch','main',repeat('a',40),repeat('b',40)) RETURNING id")
        .bind(repo.id).bind(user.id).fetch_one(&database.raw).await.unwrap();
    let review: i64 = sqlx::query_scalar("INSERT INTO pr_reviews(pull_request_id,author_id,state) VALUES($1,$2,'pending') RETURNING id")
        .bind(pull).bind(user.id).fetch_one(&database.raw).await.unwrap();
    sqlx::query("INSERT INTO pr_comments(pull_request_id,author_id,body,review_id) VALUES($1,$2,'draftsecret',$3),($1,$2,'publicword',NULL)")
        .bind(pull).bind(user.id).bind(review).execute(&database.raw).await.unwrap();
    let item = Item::Pull { id: pull };
    assert_eq!(
        search::comment_text(&database.raw, item).await.unwrap(),
        "publicword"
    );
    for state in ["commented", "pending"] {
        sqlx::query("DELETE FROM cell_outbox WHERE topic='search'")
            .execute(&database.raw)
            .await
            .unwrap();
        sqlx::query("UPDATE pr_reviews SET state=$2 WHERE id=$1")
            .bind(review)
            .bind(state)
            .execute(&database.raw)
            .await
            .unwrap();
        let text = search::comment_text(&database.raw, item).await.unwrap();
        assert_eq!(text.contains("draftsecret"), state == "commented");
        let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cell_outbox WHERE topic='search' AND payload::jsonb->>'item'='pull'")
            .fetch_one(&database.raw).await.unwrap();
        assert_eq!(queued, 1);
    }
    // Edits and deletion both invalidate the parent document, for each comment family.
    for (table, parent, id, kind) in [
        ("issue_comments", "issue_id", issue, "issue"),
        ("pr_comments", "pull_request_id", pull, "pull"),
    ] {
        for change in [
            format!("UPDATE {table} SET body='editedword' WHERE {parent}=$1"),
            format!("DELETE FROM {table} WHERE {parent}=$1"),
        ] {
            sqlx::query("DELETE FROM cell_outbox WHERE topic='search'")
                .execute(&database.raw)
                .await
                .unwrap();
            sqlx::query(sqlx::AssertSqlSafe(change.as_str()))
                .bind(id)
                .execute(&database.raw)
                .await
                .unwrap();
            let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cell_outbox WHERE topic='search' AND payload::jsonb->>'item'=$1")
                .bind(kind).fetch_one(&database.raw).await.unwrap();
            assert_eq!(queued, 1);
        }
    }
    assert!(search::comment_text(&database.raw, item)
        .await
        .unwrap()
        .is_empty());
    database.finish().await;
}

#[tokio::test]
async fn directory_ranked_search_bounds_input_counts_and_last_page() {
    use codeyard_database::cells::search::{Filter, Kind, Query};
    let mut database = PostgresDatabase::unmigrated().await;
    let directory = Directory::connect(
        &database.connection_url(),
        Config {
            search_query_bytes: 64,
            ..Config::default()
        },
    )
    .await
    .unwrap();
    directory.migrate().await.unwrap();
    directory.register_cell(cell(0, 'a')).await.unwrap();
    let key = "a".repeat(64);
    sqlx::query("INSERT INTO search_repositories(id,cell_id,event_id,attempt,document,public,name,description) SELECT n,0,1,1,jsonb_build_object('id',n),TRUE,'boundedname','boundedword' FROM generate_series(1,1005) AS n")
        .execute(&database.raw).await.unwrap();
    let mut query = Query {
        kind: Kind::Repositories,
        viewer: None,
        filter: Filter::Term("text".into(), "boundedword".into()),
        sort: None,
        ascending: false,
        offset: 0,
        limit: 100,
    };
    let found = directory.search(&key, &query).await.unwrap();
    assert_eq!(found.total, 1000);
    assert_eq!(found.items.len(), 100);
    assert_eq!(found.items[0].id, 1);
    query.offset = 999;
    let found = directory.search(&key, &query).await.unwrap();
    assert_eq!(found.total, 1000);
    assert_eq!(found.items.len(), 1);
    assert_eq!(found.items[0].id, 1000);
    query.offset = 1000;
    assert!(directory.search(&key, &query).await.is_err());
    query.offset = 0;
    query.filter = Filter::Term("text".into(), "boundedw".into());
    assert_eq!(directory.search(&key, &query).await.unwrap().total, 0);
    query.filter = Filter::Term("text".into(), "boundedn".into());
    assert_eq!(directory.search(&key, &query).await.unwrap().total, 1000);

    sqlx::query("UPDATE search_repositories SET description=CASE id WHEN 1 THEN 'the descriptionword' ELSE 'the descriptionword descriptionword descriptionword' END WHERE id IN (1,2)")
        .execute(&database.raw).await.unwrap();
    query.filter = Filter::Term("text".into(), "descriptionword".into());
    let found = directory.search(&key, &query).await.unwrap();
    assert_eq!(
        found.items.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        [2, 1]
    );
    assert!(found.items[0].score > found.items[1].score);
    // The schema's simple dictionary retains stop words too.
    query.filter = Filter::Term("text".into(), "the".into());
    assert_eq!(directory.search(&key, &query).await.unwrap().total, 2);
    sqlx::query("UPDATE search_repositories SET public=FALSE WHERE id=2")
        .execute(&database.raw)
        .await
        .unwrap();
    assert_eq!(directory.search(&key, &query).await.unwrap().total, 1);
    query.filter = Filter::Term("text".into(), "x".repeat(61));
    let error = directory.search(&key, &query).await.unwrap_err();
    assert!(matches!(
        error.downcast_ref::<codeyard_db_directory::Refusal>(),
        Some(codeyard_db_directory::Refusal::Invalid)
    ));
    // A hostile operator string cannot turn a text match into a match-all.
    query.filter = Filter::Term("text".into(), "\"' & | ! :* <->".into());
    assert_eq!(directory.search(&key, &query).await.unwrap().total, 0);
    directory.close().await;
    database.finish().await;
}
