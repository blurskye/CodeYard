use crate::support::native_database::{NativeDatabase, PostgresDatabase};
use codeyard::backend::{
    cluster::{self, InviteOptions, NodeAddress, NodeKey, Settings},
    db::{self, DbPool},
};
use std::time::Duration;

#[tokio::test]
async fn oversized_invites_are_refused_before_persisting_tokens() {
    let db = NativeDatabase::new().await;
    let result = cluster::create_invite(
        &db.pool,
        NodeKey::generate().public_key(),
        NodeAddress {
            tcp: (1..=200)
                .map(|port| format!("127.0.0.1:{port}").parse().unwrap())
                .collect(),
            ..Default::default()
        },
        &InviteOptions {
            roles: "storage".parse().unwrap(),
            reusable: false,
            requires_approval: false,
            expires_in: Duration::from_secs(3600),
        },
    )
    .await;
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("too many advertised addresses"));
    assert!(db::cluster::list_join_tokens(&db.pool)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn service_startup_refuses_roles_outside_existing_enrollment() {
    let db = PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let key = cluster::load_or_create(root.path()).unwrap();
    db::cluster::register_own_node(
        &db.pool,
        &key.public_hex(),
        "storage",
        &["storage".into()],
        "test",
    )
    .await
    .unwrap();
    let settings = Settings::from_lookup(&|name| match name {
        "CLUSTER_TCP_BIND" => Some("127.0.0.1:0".into()),
        "NODE_NAME" => Some("storage".into()),
        _ => None,
    })
    .unwrap()
    .unwrap();
    let error = cluster::start(
        &db.pool,
        root.path(),
        root.path(),
        &"web,storage".parse().unwrap(),
        settings,
    )
    .await
    .err()
    .unwrap();
    assert!(error.to_string().contains("exceed enrollment"), "{error}");
}

async fn adoption_and_index(pool: &DbPool) {
    let owner = db::users::accounts::create_user(pool, "adopter", None)
        .await
        .unwrap();
    let node = db::cluster::register_own_node(
        pool,
        &NodeKey::generate().public_hex(),
        "storage",
        &["storage".into()],
        "test",
    )
    .await
    .unwrap();
    let make = |name: &'static str| async move {
        db::repositories::records::create_repository(
            pool,
            owner.id,
            name,
            None,
            false,
            &format!("/repos/{}.git", uuid::Uuid::new_v4()),
        )
        .await
        .unwrap()
    };
    let source = make("source").await;
    let fork = make("fork").await;
    match pool {
        DbPool::Postgres(raw) => {
            sqlx::query("UPDATE repositories SET forked_from_id=$1,is_fork=true WHERE id=$2")
                .bind(source.id)
                .bind(fork.id)
                .execute(raw)
                .await
                .unwrap();
            let definition: String = sqlx::query_scalar("SELECT indexdef FROM pg_indexes WHERE schemaname=current_schema() AND indexname='idx_repositories_storage_node'").fetch_one(raw).await.unwrap();
            assert!(definition.contains("(storage_node_id, id)"));
        }
        DbPool::Sqlite(raw) => {
            sqlx::query("UPDATE repositories SET forked_from_id=?1,is_fork=true WHERE id=?2")
                .bind(source.id)
                .bind(fork.id)
                .execute(raw)
                .await
                .unwrap();
            let definition: String = sqlx::query_scalar(
                "SELECT sql FROM sqlite_master WHERE name='idx_repositories_storage_node'",
            )
            .fetch_one(raw)
            .await
            .unwrap();
            assert!(definition.contains("(storage_node_id, id)"));
        }
    }
    // A fork absent from the directory snapshot must prevent adoption of its source.
    assert!(
        db::cluster::place_fork_family(pool, source.id, node.id, &[source.id])
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        db::repositories::records::get_repository_by_id(pool, source.id)
            .await
            .unwrap()
            .unwrap()
            .storage_node_id,
        None
    );
    assert_eq!(
        db::cluster::place_fork_family(pool, source.id, node.id, &[source.id, fork.id])
            .await
            .unwrap(),
        [source.id, fork.id]
    );
    for id in [source.id, fork.id] {
        assert_eq!(
            db::repositories::records::get_repository_by_id(pool, id)
                .await
                .unwrap()
                .unwrap()
                .storage_node_id,
            Some(node.id)
        );
    }
}

#[tokio::test]
async fn adoption_refreshes_forks_committed_while_it_waits_for_the_source() {
    let db = PostgresDatabase::new().await;
    let DbPool::Postgres(raw) = &db.pool else {
        unreachable!()
    };
    let root = tempfile::tempdir().unwrap();
    let node = db::cluster::register_own_node(
        &db.pool,
        &NodeKey::generate().public_hex(),
        "storage",
        &["storage".into()],
        "test",
    )
    .await
    .unwrap();
    let owner = db::users::accounts::create_user(&db.pool, "owner", None)
        .await
        .unwrap();
    let path = root.path().join(format!("{}.git", uuid::Uuid::new_v4()));
    std::fs::create_dir(&path).unwrap();
    let source = db::repositories::records::create_repository(
        &db.pool,
        owner.id,
        "source",
        None,
        false,
        path.to_str().unwrap(),
    )
    .await
    .unwrap();
    let mut tx = raw.begin().await.unwrap();
    sqlx::query("SELECT id FROM repositories WHERE id=$1 FOR NO KEY UPDATE")
        .bind(source.id)
        .execute(&mut *tx)
        .await
        .unwrap();
    let fork_path = root.path().join(format!("{}.git", uuid::Uuid::new_v4()));
    std::fs::create_dir(&fork_path).unwrap();
    let fork: i64 = sqlx::query_scalar("INSERT INTO repositories(owner_kind,owner_user_id,name,disk_path,is_fork,forked_from_id,created_by_id) VALUES('user',$1,'fork',$2,true,$3,$1) RETURNING id")
        .bind(owner.id).bind(fork_path.to_str().unwrap()).bind(source.id).fetch_one(&mut *tx).await.unwrap();
    let pool = db.pool.clone();
    let directory = root.path().to_owned();
    let adopting =
        tokio::spawn(async move { cluster::adopt_repositories(&pool, node.id, &directory).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!adopting.is_finished());
    tx.commit().await.unwrap();
    assert_eq!(adopting.await.unwrap().unwrap(), 2);
    for id in [source.id, fork] {
        assert_eq!(
            db::repositories::records::get_repository_by_id(&db.pool, id)
                .await
                .unwrap()
                .unwrap()
                .storage_node_id,
            Some(node.id)
        );
    }
}

#[tokio::test]
async fn adoption_keeps_current_families_together_on_both_engines() {
    let sqlite = NativeDatabase::new().await;
    adoption_and_index(&sqlite.pool).await;
    let postgres = PostgresDatabase::new().await;
    adoption_and_index(&postgres.pool).await;
}
