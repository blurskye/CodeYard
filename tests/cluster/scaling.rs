use crate::support::{database::Database, http_client::Client};
use axum::http::StatusCode;
use codeyard_model::cluster::ScalingPolicy;
use codeyard_support::node_identity::NodeKey;

async fn reset_minute(db: &Database) {
    db.execute(
        "UPDATE cluster_leases SET evaluated_at='2020-01-01T00:00:00.000000Z'",
        &[],
    )
    .await;
    db.execute(
        "UPDATE cluster_scaling_policies SET last_action_at=NULL",
        &[],
    )
    .await;
}

#[tokio::test]
async fn site_policy_forms_leader_pause_dry_run_and_storage_rules_agree_on_both_engines() {
    for pg in [false, true] {
        let mut db = Database::new(pg).await;
        db.complete_setup().await;
        codeyard_db_cluster::cluster::enable_cluster(db.pool())
            .await
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let router = db.router(root.path()).await;
        let user = db.actor("scaling-admin").await;
        db.execute("UPDATE users SET is_admin=true WHERE id=$1", &[user])
            .await;
        let admin = Client::login(&router, db.pool(), user).await;
        let policies = codeyard_db_cluster::scaling::policies(db.pool())
            .await
            .unwrap();
        assert_eq!(policies.len(), 4);
        assert!(policies
            .iter()
            .all(|policy| policy.paused && policy.dry_run));
        let fields = [
            ("scaling_role", "storage"),
            ("scaling_minimum", "2"),
            ("scaling_maximum", "3"),
            ("scaling_step", "1"),
            ("scaling_cooldown", "60"),
            ("scaling_threshold", "20"),
            ("scaling_duration", "600"),
            ("scaling_idle", "900"),
            ("scaling_dry_run", "on"),
        ];
        assert_eq!(
            admin.form_reply("/admin/settings", &fields).await.status,
            StatusCode::SEE_OTHER
        );
        let source = super::drain::node(&db, "scaling-source", 9000).await;
        let other = super::drain::node(&db, "scaling-other", 9000).await;
        let action = codeyard_db_cluster::scaling::evaluate(db.pool(), source)
            .await
            .unwrap();
        // Two nodes already satisfy the minimum; free space is plentiful.
        assert!(action.is_none());
        assert!(codeyard_db_cluster::scaling::evaluate(db.pool(), other)
            .await
            .unwrap()
            .is_none());
        let actor = (user, admin.session.clone()).into();
        let mut policy = ScalingPolicy::defaults("storage");
        policy.minimum = 3;
        policy.maximum = 4;
        policy.paused = false;
        codeyard_db_cluster::scaling::save(db.pool(), &actor, &policy)
            .await
            .unwrap();
        reset_minute(&db).await;
        let action = codeyard_db_cluster::scaling::evaluate(db.pool(), source)
            .await
            .unwrap()
            .unwrap();
        assert!(action.dry_run);
        assert_eq!(action.count, 1);
        assert!(action.drain_node.is_none());
        assert_eq!(db.scalar("SELECT COUNT(*) FROM join_tokens", &[]).await, 0);
        assert!(codeyard_db_cluster::scaling::evaluate(db.pool(), source)
            .await
            .unwrap()
            .is_none());
        policy.paused = true;
        codeyard_db_cluster::scaling::save(db.pool(), &actor, &policy)
            .await
            .unwrap();
        reset_minute(&db).await;
        assert!(codeyard_db_cluster::scaling::evaluate(db.pool(), source)
            .await
            .unwrap()
            .is_none());
        policy.paused = false;
        policy.minimum = 0;
        codeyard_db_cluster::scaling::save(db.pool(), &actor, &policy)
            .await
            .unwrap();
        reset_minute(&db).await;
        // Storage never scales down, even with minimum zero and old, idle nodes.
        db.execute(
            "UPDATE nodes SET created_at='2020-01-01T00:00:00.000000Z'",
            &[],
        )
        .await;
        assert!(codeyard_db_cluster::scaling::evaluate(db.pool(), source)
            .await
            .unwrap()
            .is_none());
        db.execute("UPDATE nodes SET disk_free=1000", &[]).await;
        reset_minute(&db).await;
        assert!(codeyard_db_cluster::scaling::evaluate(db.pool(), source)
            .await
            .unwrap()
            .is_none());
        db.execute(
            "UPDATE cluster_scaling_policies SET trigger_since='2020-01-01T00:00:00.000000Z'",
            &[],
        )
        .await;
        reset_minute(&db).await;
        assert!(codeyard_db_cluster::scaling::evaluate(db.pool(), source)
            .await
            .unwrap()
            .is_some());
        let key = NodeKey::generate();
        let runner = codeyard_db_cluster::cluster::register_own_node(
            db.pool(),
            &key.public_hex(),
            "idle-runner",
            &["runner".into()],
            "test",
        )
        .await
        .unwrap();
        codeyard_db_cluster::cluster::record_node_heartbeat(
            db.pool(),
            &key.public_hex(),
            "test",
            "{}",
            None,
        )
        .await
        .unwrap();
        db.execute(
            "UPDATE nodes SET created_at='2020-01-01T00:00:00.000000Z' WHERE id=$1",
            &[runner.id],
        )
        .await;
        policy.paused = true;
        codeyard_db_cluster::scaling::save(db.pool(), &actor, &policy)
            .await
            .unwrap();
        let mut runner_policy = ScalingPolicy::defaults("runner");
        runner_policy.paused = false;
        codeyard_db_cluster::scaling::save(db.pool(), &actor, &runner_policy)
            .await
            .unwrap();
        reset_minute(&db).await;
        let action = codeyard_db_cluster::scaling::evaluate(db.pool(), source)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(action.drain_node.as_deref(), Some("idle-runner"));
        assert!(action.dry_run);
        db.execute("UPDATE users SET is_admin=false WHERE id=$1", &[user])
            .await;
        assert_eq!(
            admin.form_reply("/admin/settings", &fields).await.status,
            StatusCode::FORBIDDEN
        );
        db.finish().await;
    }
}
