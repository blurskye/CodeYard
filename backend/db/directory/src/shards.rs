//! The map from the 256 logical shards to the clusters that hold them, and moving one shard
//! to another cluster with no failed lookup.
//!
//! Writes hold their shards' map rows (`FOR SHARE`) until every cluster they touched has
//! committed. A move first marks the shard `moving`, which waits for those writes and refuses
//! new ones; lookups keep reading the old cluster. It copies the shard, checks the copy,
//! switches the map, waits until lookups that read the old map have finished, and only then
//! removes the old copy. Running the command again resumes an interrupted move.
use super::{Directory, Refusal};
use anyhow::{ensure, Result};
use chrono::{DateTime, Utc};
use codeyard_database::pool::PgPool;
use sqlx::{Postgres, Transaction};
use std::collections::BTreeMap;

/// Shards grouped by the cluster that holds them, in cluster name order.
pub(super) type Groups = Vec<(String, Vec<i16>)>;

const PAGE: i64 = 1000;

fn group(rows: impl IntoIterator<Item = (i16, String)>) -> Groups {
    let mut groups: BTreeMap<String, Vec<i16>> = BTreeMap::new();
    for (shard, cluster) in rows {
        groups.entry(cluster).or_default().push(shard);
    }
    groups.into_iter().collect()
}

/// The home shard's cluster first, then the others by name; within a cluster the home shard
/// first, then the rest in order. Every writer takes its locks in this order.
pub(super) fn ordered(mut groups: Groups, origin: i16) -> Groups {
    for (_, shards) in groups.iter_mut() {
        shards.sort_by_key(|shard| (*shard != origin, *shard));
    }
    groups.sort_by_key(|(_, shards)| !shards.contains(&origin));
    groups
}

/// Holds the shards' map rows until the registry transaction ends. A moving shard refuses.
pub(super) async fn lock_for_write(
    registry: &mut Transaction<'static, Postgres>,
    shards: &[i16],
) -> Result<Groups> {
    let rows: Vec<(i16, String, String)> = sqlx::query_as(
        "SELECT shard,cluster,state FROM directory_shards WHERE shard=ANY($1) ORDER BY shard FOR SHARE",
    )
    .bind(shards)
    .fetch_all(&mut **registry)
    .await?;
    ensure!(
        rows.len() == shards.len() && rows.iter().all(|(_, _, state)| state == "serving"),
        Refusal::Unavailable
    );
    Ok(group(
        rows.into_iter().map(|(shard, cluster, _)| (shard, cluster)),
    ))
}

/// Holds every map row. Returns the serving shards by cluster, and the moving shards by the
/// cluster they are copied from.
pub(super) async fn lock_all(
    registry: &mut Transaction<'static, Postgres>,
) -> Result<(Groups, Groups)> {
    let rows: Vec<(i16, String, String)> =
        sqlx::query_as("SELECT shard,cluster,state FROM directory_shards ORDER BY shard FOR SHARE")
            .fetch_all(&mut **registry)
            .await?;
    ensure!(rows.len() == 256, Refusal::Unavailable);
    let (serving, moving): (Vec<_>, Vec<_>) = rows
        .into_iter()
        .partition(|(_, _, state)| state == "serving");
    let strip = |rows: Vec<(i16, String, String)>| {
        group(rows.into_iter().map(|(shard, cluster, _)| (shard, cluster)))
    };
    Ok((strip(serving), strip(moving)))
}

/// Lookups take no lock: a moving shard is still read where it was, and a moved shard keeps
/// its old copy until reads that saw the old map have finished.
pub(super) async fn cluster_of(
    registry: &mut Transaction<'static, Postgres>,
    shard: i16,
) -> Result<String> {
    Ok(
        sqlx::query_scalar("SELECT cluster FROM directory_shards WHERE shard=$1")
            .bind(shard)
            .fetch_optional(&mut **registry)
            .await?
            .ok_or(Refusal::Unavailable)?,
    )
}

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct ShardRow {
    pub shard: i16,
    pub cluster: String,
    pub state: String,
    pub target: Option<String>,
    pub moved_from: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Moved {
    pub shard: i16,
    pub from: Option<String>,
    pub to: String,
    pub objects: i64,
    pub claims: i64,
}

#[derive(sqlx::FromRow)]
struct ObjectRow {
    kind: String,
    id: i64,
    home_cell: i32,
    active: bool,
    key_count: i32,
    nonce_hash: String,
    expires_at: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
struct ClaimRow {
    kind: String,
    key_hash: Vec<u8>,
    value: String,
    object_kind: String,
    object_id: i64,
    nonce_hash: String,
    confirmed: bool,
    expires_at: Option<DateTime<Utc>>,
}

impl Directory {
    pub async fn shards(&self) -> Result<Vec<ShardRow>> {
        self.run(async {
            Ok(sqlx::query_as(
                "SELECT shard,cluster,state,target,moved_from FROM directory_shards ORDER BY shard",
            )
            .fetch_all(&self.pool)
            .await?)
        })
        .await
    }

    /// Clusters the shard map names that this process was not given.
    pub async fn missing_clusters(&self) -> Result<Vec<String>> {
        let named: Vec<String> = self
            .run(async {
                Ok(sqlx::query_scalar("SELECT DISTINCT name FROM (SELECT cluster AS name FROM directory_shards UNION SELECT target FROM directory_shards UNION SELECT moved_from FROM directory_shards) AS names WHERE name IS NOT NULL ORDER BY name")
                    .fetch_all(&self.pool)
                    .await?)
            })
            .await?;
        Ok(named
            .into_iter()
            .filter(|name| self.cluster(name).is_err())
            .collect())
    }

    /// Moves one shard to the cluster `to`, or finishes an interrupted move to it.
    pub async fn move_shard(&self, shard: i16, to: &str) -> Result<Moved> {
        ensure!(
            (0..=255).contains(&shard) && super::valid_cluster(to),
            Refusal::Invalid
        );
        self.cluster(to)?;
        let row = self.shard_row(shard).await?;
        let mut moved = Moved {
            shard,
            from: None,
            to: to.to_owned(),
            objects: 0,
            claims: 0,
        };
        if row.state == "serving" && row.cluster != to {
            ensure!(row.moved_from.is_none(), Refusal::Conflict);
            // Waits for running writes on the shard; later ones are refused until the switch.
            let started = self
                .update_map(sqlx::query("UPDATE directory_shards SET state='moving',target=$2 WHERE shard=$1 AND state='serving' AND cluster=$3 AND moved_from IS NULL")
                    .bind(shard).bind(to).bind(&row.cluster))
                .await?;
            ensure!(started == 1, Refusal::Conflict);
        } else if row.state == "moving" {
            ensure!(row.target.as_deref() == Some(to), Refusal::Conflict);
        }
        let row = self.shard_row(shard).await?;
        if row.state == "moving" {
            moved.from = Some(row.cluster.clone());
            let (objects, claims) = self.copy_shard(shard, &row.cluster, to).await?;
            moved.objects = objects;
            moved.claims = claims;
            let switched = self
                .update_map(sqlx::query("UPDATE directory_shards SET cluster=target,state='serving',target=NULL,moved_from=cluster,moved_at=clock_timestamp() WHERE shard=$1 AND state='moving' AND target=$2")
                    .bind(shard).bind(to))
                .await?;
            ensure!(switched == 1, Refusal::Conflict);
        }
        let row = self.shard_row(shard).await?;
        ensure!(
            row.cluster == to && row.state == "serving",
            Refusal::Conflict
        );
        if let Some(from) = row.moved_from {
            moved.from = Some(from.clone());
            self.drop_old_copy(shard, &from).await?;
        }
        Ok(moved)
    }

    async fn shard_row(&self, shard: i16) -> Result<ShardRow> {
        self.run(async {
            Ok(sqlx::query_as(
                "SELECT shard,cluster,state,target,moved_from FROM directory_shards WHERE shard=$1",
            )
            .bind(shard)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(Refusal::Unavailable)?)
        })
        .await
    }

    /// Replaces whatever an earlier attempt left in the target, then copies objects before the
    /// claims that reference them; the claim trigger recounts each copy's claims.
    async fn copy_shard(&self, shard: i16, from: &str, to: &str) -> Result<(i64, i64)> {
        let source = self.cluster(from)?.clone();
        let target = self.cluster(to)?.clone();
        self.remove_shard_rows(shard, &target).await?;
        let mut after = (String::new(), 0_i64);
        loop {
            let page = self
                .run(async {
                    let rows: Vec<ObjectRow> = sqlx::query_as("SELECT kind,id,home_cell,active,key_count,nonce_hash,expires_at FROM directory_objects WHERE shard=$1 AND (kind,id)>($2,$3) ORDER BY kind,id LIMIT $4")
                        .bind(shard).bind(&after.0).bind(after.1).bind(PAGE)
                        .fetch_all(&source).await?;
                    let mut tx = self.begin_on(&target).await?;
                    sqlx::query("INSERT INTO directory_objects(shard,kind,id,home_cell,active,key_count,nonce_hash,expires_at) SELECT $1,* FROM UNNEST($2::text[],$3::bigint[],$4::integer[],$5::boolean[],$6::integer[],$7::text[],$8::timestamptz[])")
                        .bind(shard)
                        .bind(rows.iter().map(|row| row.kind.clone()).collect::<Vec<_>>())
                        .bind(rows.iter().map(|row| row.id).collect::<Vec<_>>())
                        .bind(rows.iter().map(|row| row.home_cell).collect::<Vec<_>>())
                        .bind(rows.iter().map(|row| row.active).collect::<Vec<_>>())
                        .bind(rows.iter().map(|row| row.key_count).collect::<Vec<_>>())
                        .bind(rows.iter().map(|row| row.nonce_hash.clone()).collect::<Vec<_>>())
                        .bind(rows.iter().map(|row| row.expires_at).collect::<Vec<_>>())
                        .execute(&mut *tx).await?;
                    tx.commit().await?;
                    Ok(rows.last().map(|row| (row.kind.clone(), row.id)))
                })
                .await?;
            match page {
                Some(last) => after = last,
                None => break,
            }
        }
        let mut after = (String::new(), Vec::<u8>::new());
        loop {
            let page = self
                .run(async {
                    let rows: Vec<ClaimRow> = sqlx::query_as("SELECT kind,key_hash,value,object_kind,object_id,nonce_hash,confirmed,expires_at FROM directory_claims WHERE shard=$1 AND (kind,key_hash)>($2,$3) ORDER BY kind,key_hash LIMIT $4")
                        .bind(shard).bind(&after.0).bind(&after.1).bind(PAGE)
                        .fetch_all(&source).await?;
                    let mut tx = self.begin_on(&target).await?;
                    sqlx::query("INSERT INTO directory_claims(shard,kind,key_hash,value,object_kind,object_id,nonce_hash,confirmed,expires_at) SELECT $1,* FROM UNNEST($2::text[],$3::bytea[],$4::text[],$5::text[],$6::bigint[],$7::text[],$8::boolean[],$9::timestamptz[])")
                        .bind(shard)
                        .bind(rows.iter().map(|row| row.kind.clone()).collect::<Vec<_>>())
                        .bind(rows.iter().map(|row| row.key_hash.clone()).collect::<Vec<_>>())
                        .bind(rows.iter().map(|row| row.value.clone()).collect::<Vec<_>>())
                        .bind(rows.iter().map(|row| row.object_kind.clone()).collect::<Vec<_>>())
                        .bind(rows.iter().map(|row| row.object_id).collect::<Vec<_>>())
                        .bind(rows.iter().map(|row| row.nonce_hash.clone()).collect::<Vec<_>>())
                        .bind(rows.iter().map(|row| row.confirmed).collect::<Vec<_>>())
                        .bind(rows.iter().map(|row| row.expires_at).collect::<Vec<_>>())
                        .execute(&mut *tx).await?;
                    tx.commit().await?;
                    Ok(rows.last().map(|row| (row.kind.clone(), row.key_hash.clone())))
                })
                .await?;
            match page {
                Some(last) => after = last,
                None => break,
            }
        }
        let count = |pool: PgPool| async move {
            let counts: (i64, i64, i64) = sqlx::query_as("SELECT (SELECT COUNT(*) FROM directory_objects WHERE shard=$1),(SELECT COUNT(*) FROM directory_claims WHERE shard=$1),(SELECT COALESCE(SUM(claim_count),0) FROM directory_objects WHERE shard=$1)")
                .bind(shard)
                .fetch_one(&pool)
                .await?;
            anyhow::Ok(counts)
        };
        let copied = self.run(count(source.clone())).await?;
        let checked = self.run(count(target.clone())).await?;
        ensure!(copied == checked, Refusal::Unavailable);
        Ok((copied.0, copied.1))
    }

    /// Lookups that read the map before the switch finish within one request time; the old
    /// copy goes only after twice that.
    async fn drop_old_copy(&self, shard: i16, from: &str) -> Result<()> {
        let pool = self.cluster(from)?.clone();
        let wait: f64 = self
            .run(async {
                Ok(sqlx::query_scalar("SELECT GREATEST(0,EXTRACT(EPOCH FROM moved_at+$2::bigint*INTERVAL '1 second'-clock_timestamp()))::float8 FROM directory_shards WHERE shard=$1 AND moved_from=$3")
                    .bind(shard)
                    .bind((2 * self.config.request_seconds + 1) as i64)
                    .bind(from)
                    .fetch_optional(&self.pool)
                    .await?
                    .ok_or(Refusal::Conflict)?)
            })
            .await?;
        tokio::time::sleep(std::time::Duration::from_secs_f64(wait.min(121.0))).await;
        self.remove_shard_rows(shard, &pool).await?;
        self.update_map(
            sqlx::query("UPDATE directory_shards SET moved_from=NULL,moved_at=NULL WHERE shard=$1 AND moved_from=$2")
                .bind(shard)
                .bind(from),
        )
        .await?;
        Ok(())
    }

    /// Map changes wait for the writes that hold the row, bounded by the lock timeout.
    async fn update_map(
        &self,
        query: sqlx::query::Query<'_, Postgres, sqlx::postgres::PgArguments>,
    ) -> Result<u64> {
        self.run(async {
            let mut tx = self.begin().await?;
            let changed = query.execute(&mut *tx).await?.rows_affected();
            tx.commit().await?;
            Ok(changed)
        })
        .await
    }

    /// Removes a shard's rows from a cluster that does not serve it, a page at a time.
    async fn remove_shard_rows(&self, shard: i16, pool: &PgPool) -> Result<()> {
        loop {
            let removed = self
                .run(async {
                    // Objects cascade to their claims; a page bounds each statement.
                    Ok(sqlx::query("DELETE FROM directory_objects WHERE (shard,kind,id) IN (SELECT shard,kind,id FROM directory_objects WHERE shard=$1 LIMIT $2)")
                        .bind(shard)
                        .bind(PAGE)
                        .execute(pool)
                        .await?
                        .rows_affected())
                })
                .await?;
            if removed == 0 {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn writers_start_with_the_home_shard_and_keep_cluster_order() {
        let groups = vec![
            ("a".to_owned(), vec![1, 9]),
            ("b".to_owned(), vec![3, 7, 5]),
            ("c".to_owned(), vec![2]),
        ];
        assert_eq!(
            super::ordered(groups, 7),
            vec![
                ("b".to_owned(), vec![7, 3, 5]),
                ("a".to_owned(), vec![1, 9]),
                ("c".to_owned(), vec![2]),
            ]
        );
        assert_eq!(crate::MAIN_CLUSTER, "main");
    }
}
