use super::*;

async fn wait_blocked(raw: &sqlx::PgPool, holder: i32, count: i64) {
    for _ in 0..400 {
        let n: i64 = sqlx::query_scalar("WITH RECURSIVE waiting(pid) AS (SELECT pid FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)) UNION SELECT a.pid FROM pg_stat_activity a JOIN waiting w ON w.pid=ANY(pg_blocking_pids(a.pid))) SELECT COUNT(*) FROM waiting")
            .bind(holder).fetch_one(raw).await.unwrap();
        if n >= count {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("requests did not reach the held lock");
}

#[tokio::test]
async fn postgres_direct_grant_changes_conflict_with_repository_authority_reads() {
    let mut f = Database::new(true).await;
    let owner = f.actor("grant_owner").await;
    let target = f.actor("grant_target").await;
    let r = repo(&f, owner, "grant_lock").await;
    let db::DbPool::Postgres(raw) = f.pool() else {
        unreachable!()
    };
    // Both the trusted DB operations and browser collaborator mutations must wait even
    // when the authority read found no direct grant to lock.
    for operation in 0..4 {
        let mut reader = raw.begin().await.unwrap();
        let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *reader)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM repositories WHERE id=$1 FOR SHARE")
            .bind(r.id)
            .execute(&mut *reader)
            .await
            .unwrap();
        let pool = f.pool().clone();
        let request = tokio::spawn(async move {
            match operation {
                0 => {
                    db::repositories::access::grant_repo_access(
                        &pool,
                        target,
                        r.id,
                        AccessLevel::Read,
                    )
                    .await
                    .unwrap();
                }
                1 => {
                    db::repositories::access::revoke_repo_access(&pool, target, r.id)
                        .await
                        .unwrap();
                }
                2 | 3 => {
                    codeyard::backend::repositories::access::change_collaborator(
                        &pool,
                        owner,
                        r.id,
                        "grant_target",
                        (operation == 2).then_some(AccessLevel::Read),
                    )
                    .await
                    .unwrap();
                }
                _ => unreachable!(),
            }
        });
        wait_blocked(raw, holder, 1).await;
        assert!(!request.is_finished());
        reader.commit().await.unwrap();
        request.await.unwrap();
    }
    f.finish().await;
}

#[tokio::test]
async fn postgres_transfer_locks_target_organization_before_a_forks_source() {
    let mut f = Database::new(true).await;
    let owner = f.actor("transfer_order_owner").await;
    organization(&f, 1001, owner).await;
    let source = repo(&f, owner, "transfer_source").await;
    let fork = db::repositories::discovery::create_forked_repository(
        f.pool(),
        db::repositories::discovery::NewForkedRepository {
            owner_id: owner,
            name: "transfer_fork",
            description: None,
            is_private: true,
            disk_path: "/fixture/fork.git",
            forked_from: source.id,
            default_branch: "main",
        },
    )
    .await
    .unwrap();
    records::transfer_repository(
        f.pool(),
        owner,
        fork.id,
        RepositoryTarget::Organization(1001),
    )
    .await
    .unwrap();
    let db::DbPool::Postgres(raw) = f.pool() else {
        unreachable!()
    };
    let mut settings = raw.begin().await.unwrap();
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *settings)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM organizations WHERE id=1001 FOR SHARE")
        .execute(&mut *settings)
        .await
        .unwrap();
    let pool = f.pool().clone();
    let transfer = tokio::spawn(async move {
        records::transfer_repository(
            &pool,
            owner,
            source.id,
            RepositoryTarget::Organization(1001),
        )
        .await
    });
    wait_blocked(raw, holder, 1).await;
    sqlx::query("SELECT id FROM repositories WHERE id=$1 FOR NO KEY UPDATE NOWAIT")
        .bind(source.id)
        .execute(&mut *settings)
        .await
        .expect("a fork's settings can still acquire its source; transfer waits before locking it");
    settings.commit().await.unwrap();
    assert_eq!(transfer.await.unwrap().unwrap().owner_org_id, Some(1001));
    f.finish().await;
}

#[tokio::test]
async fn postgres_privileged_node_and_crawler_writes_retain_their_session() {
    let mut f = Database::new(true).await;
    let admin = f.actor("retained_admin").await;
    f.execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[admin])
        .await;
    db::node_config::upsert_node_configuration(f.pool(), "public", true)
        .await
        .unwrap();
    let key = codeyard::backend::cluster::NodeKey::generate().public_hex();
    let node = db::cluster::register_own_node(
        f.pool(),
        &key,
        "retained-node",
        &["storage".to_owned()],
        "test",
    )
    .await
    .unwrap();
    for crawler in [false, true] {
        let original = session(f.pool(), admin).await;
        let db::DbPool::Postgres(raw) = f.pool() else {
            unreachable!()
        };
        let mut holder = raw.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *holder)
            .await
            .unwrap();
        let (table, id) = if crawler {
            ("node_configuration", 1)
        } else {
            ("nodes", node.id)
        };
        sqlx::query(&format!("SELECT id FROM {table} WHERE id=$1 FOR UPDATE"))
            .bind(id)
            .execute(&mut *holder)
            .await
            .unwrap();
        let pool = f.pool().clone();
        let session = original.clone();
        let mutation = tokio::spawn(async move {
            if crawler {
                assert!(
                    db::node_config::set_ai_crawlers(&pool, admin, &session, "allow")
                        .await
                        .unwrap()
                );
            } else {
                assert!(matches!(
                    db::cluster::set_node_status(
                        &pool,
                        db::cluster::Authority::Administrator {
                            user_id: admin,
                            session: &session
                        },
                        "retained-node",
                        "active"
                    )
                    .await
                    .unwrap(),
                    db::models::NodeStatusChange::Changed { .. }
                ));
            }
        });
        wait_blocked(raw, pid, 1).await;
        let pool = f.pool().clone();
        let revocation = tokio::spawn(async move {
            db::users::sessions::delete_session(&pool, &original)
                .await
                .unwrap();
        });
        wait_blocked(raw, pid, 2).await;
        assert!(!revocation.is_finished());
        holder.commit().await.unwrap();
        mutation.await.unwrap();
        revocation.await.unwrap();
    }
    f.finish().await;
}

#[tokio::test]
async fn postgres_unstar_locks_parent_before_the_star() {
    let mut f = Database::new(true).await;
    let owner = f.actor("star_owner").await;
    let r = repo(&f, owner, "star").await;
    db::repositories::engagement::star_repo(f.pool(), owner, r.id)
        .await
        .unwrap();
    let db::DbPool::Postgres(raw) = f.pool() else {
        unreachable!()
    };
    let mut deleting = raw.begin().await.unwrap();
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *deleting)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM repositories WHERE id=$1 FOR UPDATE")
        .bind(r.id)
        .execute(&mut *deleting)
        .await
        .unwrap();
    let pool = f.pool().clone();
    let request =
        tokio::spawn(
            async move { db::repositories::engagement::unstar_repo(&pool, owner, r.id).await },
        );
    wait_blocked(raw, holder, 1).await;
    sqlx::query("SELECT user_id FROM stars WHERE repo_id=$1 FOR UPDATE NOWAIT")
        .bind(r.id)
        .execute(&mut *deleting)
        .await
        .expect("unstar must not hold the child before its parent");
    sqlx::query("DELETE FROM repositories WHERE id=$1")
        .bind(r.id)
        .execute(&mut *deleting)
        .await
        .unwrap();
    deleting.commit().await.unwrap();
    assert!(!request.await.unwrap().unwrap());
    f.finish().await;
}

#[tokio::test]
async fn postgres_collaborator_takes_membership_lock_before_accounts() {
    let mut f = Database::new(true).await;
    let owner = f.actor("collaborator_owner").await;
    let admin = f.actor("collaborator_admin").await;
    let r = repo(&f, owner, "collaborator").await;
    let db::DbPool::Postgres(raw) = f.pool() else {
        unreachable!()
    };
    let mut banning = raw.begin().await.unwrap();
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *banning)
        .await
        .unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext(current_schema()),37002)")
        .execute(&mut *banning)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR NO KEY UPDATE")
        .bind(admin)
        .execute(&mut *banning)
        .await
        .unwrap();
    let pool = f.pool().clone();
    let request = tokio::spawn(async move {
        codeyard::backend::repositories::access::change_collaborator(
            &pool,
            owner,
            r.id,
            "collaborator_admin",
            Some(AccessLevel::Read),
        )
        .await
    });
    wait_blocked(raw, holder, 1).await;
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR NO KEY UPDATE NOWAIT")
        .bind(owner)
        .execute(&mut *banning)
        .await
        .expect("membership lock precedes the actor row");
    banning.commit().await.unwrap();
    request.await.unwrap().unwrap();
    f.finish().await;
}

#[tokio::test]
async fn postgres_opposing_organization_transfers_retry_deadlocks() {
    let mut f = Database::new(true).await;
    let owner = f.actor("transfer_owner").await;
    organization(&f, 1001, owner).await;
    organization(&f, 1002, owner).await;
    let a = org_repo(&f, owner, 1001, "first").await;
    let b = org_repo(&f, owner, 1002, "second").await;
    let db::DbPool::Postgres(raw) = f.pool() else {
        unreachable!()
    };
    let mut reader = raw.begin().await.unwrap();
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *reader)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM organizations WHERE id IN (1001,1002) ORDER BY id FOR SHARE")
        .execute(&mut *reader)
        .await
        .unwrap();
    let pool = f.pool().clone();
    let first = tokio::spawn(async move {
        records::transfer_repository(&pool, owner, a.id, RepositoryTarget::Organization(1002)).await
    });
    let pool = f.pool().clone();
    let second = tokio::spawn(async move {
        records::transfer_repository(&pool, owner, b.id, RepositoryTarget::Organization(1001)).await
    });
    wait_blocked(raw, holder, 2).await;
    reader.commit().await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(20), first)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .owner_org_id,
        Some(1002)
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(20), second)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .owner_org_id,
        Some(1001)
    );
    f.finish().await;
}

#[tokio::test]
async fn postgres_inbox_and_transfer_retry_scope_deadlocks() {
    let mut f = Database::new(true).await;
    let owner = f.actor("scope_owner").await;
    organization(&f, 1001, owner).await;
    let personal = repo(&f, owner, "personal").await;
    let organization = org_repo(&f, owner, 1001, "organization").await;
    notice(f.pool(), owner, Some(personal.id), "personal").await;
    notice(f.pool(), owner, Some(organization.id), "organization").await;
    let session = session(f.pool(), owner).await;
    let db::DbPool::Postgres(raw) = f.pool() else {
        unreachable!()
    };
    let mut reader = raw.begin().await.unwrap();
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *reader)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM organizations WHERE id=1001 FOR SHARE")
        .execute(&mut *reader)
        .await
        .unwrap();
    let pool = f.pool().clone();
    let transfer = tokio::spawn(async move {
        records::transfer_repository(
            &pool,
            owner,
            personal.id,
            RepositoryTarget::Organization(1001),
        )
        .await
    });
    wait_blocked(raw, holder, 1).await;
    let pool = f.pool().clone();
    let inbox = tokio::spawn(async move {
        db::notifications::records::notification_inbox(
            &pool,
            &session,
            owner,
            NotificationInboxAction::List {
                filter: NotificationFilter::inbox(false),
                limit: 100,
                before: None,
            },
        )
        .await
    });
    // The inbox may now finish before the transfer: the transfer no longer holds the
    // personal repository while waiting for its target organization. Otherwise observe
    // both real waiters before releasing the holder.
    tokio::time::timeout(Duration::from_secs(10), async {
        while !inbox.is_finished() {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid))",
            )
            .bind(holder)
            .fetch_one(raw)
            .await
            .unwrap();
            if waiting >= 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    reader.commit().await.unwrap();
    tokio::time::timeout(Duration::from_secs(20), transfer)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(20), inbox)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .unwrap()
            .notifications
            .len(),
        2
    );
    f.finish().await;
}

#[tokio::test]
async fn postgres_family_promotion_refuses_a_fork_committed_during_its_lock_wait() {
    let mut f = Database::new(true).await;
    let owner = f.actor("family_owner").await;
    let r = repo(&f, owner, "family").await;
    let mut nodes = Vec::new();
    for name in ["former", "target"] {
        nodes.push(
            db::cluster::register_own_node(
                f.pool(),
                &codeyard::backend::cluster::NodeKey::generate().public_hex(),
                name,
                &["storage".to_owned()],
                "test",
            )
            .await
            .unwrap()
            .id,
        );
    }
    let (from, to) = (nodes[0], nodes[1]);
    f.execute(
        "UPDATE repositories SET storage_node_id=$1 WHERE id=$2",
        &[from, r.id],
    )
    .await;
    f.execute(
        "UPDATE nodes SET last_seen='2000-01-01T00:00:00.000000Z' WHERE id=$1",
        &[from],
    )
    .await;
    f.execute(
        "UPDATE nodes SET last_seen=CURRENT_TIMESTAMP WHERE id=$1",
        &[to],
    )
    .await;
    f.execute(
        "INSERT INTO repository_replicas(repository_id,node_id,applied_version) VALUES($1,$2,0)",
        &[r.id, to],
    )
    .await;
    let db::DbPool::Postgres(raw) = f.pool() else {
        unreachable!()
    };
    let mut fork = raw.begin().await.unwrap();
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *fork)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM repositories WHERE id=$1 FOR NO KEY UPDATE")
        .bind(r.id)
        .execute(&mut *fork)
        .await
        .unwrap();
    let child: i64 = sqlx::query_scalar("INSERT INTO repositories(owner_kind,owner_user_id,name,disk_path,is_private,is_fork,forked_from_id,storage_node_id,created_by_id) VALUES('user',$1,'new-fork',$2,TRUE,TRUE,$3,$4,$1) RETURNING id")
        .bind(owner).bind(uuid::Uuid::new_v4().to_string()).bind(r.id).bind(from).fetch_one(&mut *fork).await.unwrap();
    let pool = f.pool().clone();
    let promotion =
        tokio::spawn(async move { db::cluster::promote_fork_family(&pool, r.id, from, to).await });
    wait_blocked(raw, holder, 1).await;
    fork.commit().await.unwrap();
    assert_eq!(
        promotion.await.unwrap().unwrap(),
        db::cluster::FamilyOutcome::Refused
    );
    assert_eq!(f.repository(r.id).await.storage_node_id, Some(from));
    assert_eq!(f.repository(child).await.storage_node_id, Some(from));
    f.finish().await;
}
