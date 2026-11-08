//! New installations choose majority; selecting the legacy policy never rewrites Git data.
use crate::support::database::Database;

#[tokio::test]
async fn majority_is_default_and_legacy_is_explicit_on_both_engines() {
    for pg in [false, true] {
        let mut db = Database::empty(pg).await;
        db.complete_setup().await;
        assert!(codeyard_db_placement::cluster::enable_cluster(db.pool())
            .await
            .unwrap());
        assert_eq!(
            codeyard_db_placement::cluster::writes::write_settings(db.pool())
                .await
                .unwrap()
                .write_quorum
                .as_deref(),
            Some("majority")
        );
        assert!(
            !codeyard_db_placement::cluster::enable_cluster_with_quorum(db.pool(), "1")
                .await
                .unwrap()
        );
        assert_eq!(
            codeyard_db_placement::cluster::writes::write_settings(db.pool())
                .await
                .unwrap()
                .write_quorum
                .as_deref(),
            Some("majority")
        );
        db.finish().await;
    }
}

#[tokio::test]
async fn enabling_refuses_live_cores_legacy_repositories_invalid_policy_and_failed_commit() {
    for pg in [false, true] {
        let mut db = Database::empty(pg).await;
        db.complete_setup().await;
        let owner = db.actor("legacy-quorum-owner").await;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(format!("{}.git", uuid::Uuid::new_v4()));
        git2::Repository::init_bare(&path).unwrap();
        let repository = codeyard_db_repos::repositories::records::create_repository(
            db.pool(),
            owner,
            "saved",
            None,
            false,
            path.to_str().unwrap(),
        )
        .await
        .unwrap();
        assert!(codeyard_db_placement::cluster::enable_cluster_with_quorum(
            db.pool(),
            "majority; DELETE FROM repositories"
        )
        .await
        .is_err());
        assert!(codeyard_db_placement::cluster::enable_cluster(db.pool())
            .await
            .is_err());
        assert!(!codeyard_db_placement::cluster::cluster_enabled(db.pool())
            .await
            .unwrap());
        assert_eq!(db.count().await, 1);
        assert_eq!(
            db.repository(repository.id).await.disk_path,
            path.to_str().unwrap()
        );
        assert!(path.join("HEAD").is_file());
        let live = codeyard_db_placement::cluster::serving_lock(db.pool(), "quorum-default-core")
            .await
            .unwrap();
        assert!(
            codeyard_db_placement::cluster::enable_cluster_with_quorum(db.pool(), "1")
                .await
                .is_err()
        );
        live.release().await;
        if pg {
            db.execute("CREATE FUNCTION owned_enable_failure() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.cluster_enabled THEN RAISE EXCEPTION 'owned configuration outage'; END IF; RETURN NEW; END $$",&[]).await;
            db.execute("CREATE TRIGGER owned_enable_failure BEFORE UPDATE ON node_configuration FOR EACH ROW EXECUTE FUNCTION owned_enable_failure()",&[]).await;
        } else {
            db.execute("CREATE TRIGGER owned_enable_failure BEFORE UPDATE ON node_configuration WHEN NEW.cluster_enabled BEGIN SELECT RAISE(ABORT,'owned configuration outage'); END",&[]).await;
        }
        assert!(
            codeyard_db_placement::cluster::enable_cluster_with_quorum(db.pool(), "1")
                .await
                .is_err()
        );
        assert!(!codeyard_db_placement::cluster::cluster_enabled(db.pool())
            .await
            .unwrap());
        assert!(
            codeyard_db_placement::cluster::writes::write_settings(db.pool())
                .await
                .unwrap()
                .write_quorum
                .is_none()
        );
        db.execute(
            if pg {
                "DROP TRIGGER owned_enable_failure ON node_configuration"
            } else {
                "DROP TRIGGER owned_enable_failure"
            },
            &[],
        )
        .await;
        assert!(
            codeyard_db_placement::cluster::enable_cluster_with_quorum(db.pool(), "1")
                .await
                .unwrap()
        );
        assert!(
            codeyard_db_placement::cluster::writes::write_settings(db.pool())
                .await
                .unwrap()
                .write_quorum
                .is_none()
        );
        assert_eq!(db.count().await, 1);
        db.finish().await;
    }
}

#[tokio::test]
async fn cli_validates_policy_before_enabling_and_preserves_the_selected_mode() {
    for policy in ["majority", "1"] {
        let mut db = Database::empty(true).await;
        db.complete_setup().await;
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".env"), "").unwrap();
        let repos = root.path().join("repos");
        let state = root.path().join("state");
        let url = db.connection_url();
        let bad = super::backups::cli(
            root.path(),
            &url,
            None,
            "default-core",
            &repos,
            &state,
            &["cluster", "enable", "--write-quorum", "untrusted"],
        )
        .await;
        assert!(!bad.status.success());
        assert!(!codeyard_db_placement::cluster::cluster_enabled(db.pool())
            .await
            .unwrap());
        let args = if policy == "majority" {
            vec!["cluster", "enable"]
        } else {
            vec!["cluster", "enable", "--write-quorum", "1"]
        };
        let output = super::backups::cli(
            root.path(),
            &url,
            None,
            "default-core",
            &repos,
            &state,
            &args,
        )
        .await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(codeyard_db_placement::cluster::cluster_enabled(db.pool())
            .await
            .unwrap());
        assert_eq!(
            codeyard_db_placement::cluster::writes::write_settings(db.pool())
                .await
                .unwrap()
                .write_quorum
                .as_deref(),
            if policy == "majority" {
                Some("majority")
            } else {
                None
            }
        );
        db.finish().await;
    }
}
