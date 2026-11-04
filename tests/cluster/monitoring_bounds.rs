use crate::support::database::Database;

#[tokio::test]
async fn metrics_refuse_to_truncate_storage_nodes() {
    for pg in [false, true] {
        let mut database = Database::new(pg).await;
        let insert = if pg {
            "INSERT INTO nodes(public_key,name,roles)
             SELECT lpad(to_hex(i),64,'0'), 'node-' || i, ARRAY['storage']
             FROM generate_series(1,1001) AS i"
        } else {
            "WITH RECURSIVE ids(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM ids WHERE i<1001)
             INSERT INTO nodes(public_key,name,roles)
             SELECT printf('%064x',i), 'node-' || i, '[\"storage\"]' FROM ids"
        };
        database.execute(insert, &[]).await;
        assert!(codeyard_db_cluster::metrics::storage_nodes(database.pool())
            .await
            .is_err());
        database
            .execute("DELETE FROM nodes WHERE name='node-1001'", &[])
            .await;
        assert_eq!(
            codeyard_db_cluster::metrics::storage_nodes(database.pool())
                .await
                .unwrap()
                .len(),
            1000
        );
        database.finish().await;
    }
}
