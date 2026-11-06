//! Pending keys are invisible until their local object has been created and activated.
//!
//! Each logical shard holds its keys and its own copy of the objects they belong to; the copy
//! in the object's own shard (`Object::shard`) is its home row. A write locks the shard map
//! rows it touches in the registry (see `shards`), opens one transaction per cluster that
//! holds them, home cluster first, and commits them all after every step succeeded. With one
//! cluster that is a single transaction. Across clusters a commit that fails after an earlier
//! one succeeded leaves some shards pending; they expire like an interrupted signup, and
//! retrying the same reservation or confirmation completes them.
use super::{shards, Directory, Refusal, MAIN_CLUSTER};
use anyhow::{ensure, Result};
use chrono::{DateTime, Utc};
use codeyard_model::cells::{Key, KeyKind, Location, Object, ObjectKind};
use codeyard_support::{cells::Prefix, token_hash::token_hash};
use sqlx::{Postgres, Transaction};

type Tx = Transaction<'static, Postgres>;
/// The shards of one cluster and its transaction; `None` is the main cluster, which shares
/// the registry's transaction and connection.
type Opened = Vec<(Vec<i16>, Option<Tx>)>;

fn pick<'a>(registry: &'a mut Tx, tx: &'a mut Option<Tx>) -> &'a mut Tx {
    match tx {
        Some(tx) => tx,
        None => registry,
    }
}

#[derive(sqlx::FromRow)]
struct Home {
    home_cell: i32,
    active: bool,
    claim_count: i32,
    key_count: i32,
    nonce_hash: String,
    live: bool,
    expires_at: Option<DateTime<Utc>>,
}
#[derive(sqlx::FromRow)]
struct Claim {
    value: String,
    object_kind: String,
    object_id: i64,
    nonce_hash: String,
    confirmed: bool,
    live: bool,
}
impl Claim {
    fn belongs(&self, object: Object, key: &Key) -> bool {
        self.object_kind == object.kind.as_str()
            && self.object_id == object.id
            && self.value == key.value()
    }
}

fn checked_keys(object: Object, keys: &[Key]) -> Result<Vec<Key>> {
    object.validate().map_err(|_| Refusal::Invalid)?;
    ensure!(keys.len() <= 1000, Refusal::Invalid);
    ensure!(
        keys.iter().all(|key| key.belongs_to(object.kind)),
        Refusal::Invalid
    );
    let mut keys = keys.to_vec();
    keys.sort_by_key(|key| (key.shard(), key.kind().as_str(), key.hash()));
    keys.dedup();
    Ok(keys)
}
fn checked(object: Object, keys: &[Key], nonce: &str) -> Result<(Vec<Key>, String)> {
    ensure!(
        codeyard_support::tokens::is_hex_token(nonce, 64),
        Refusal::Invalid
    );
    Ok((checked_keys(object, keys)?, token_hash(nonce)))
}

/// The object's home shard and the shards of its keys.
fn touched(object: Object, keys: &[Key]) -> Vec<i16> {
    let mut shards: Vec<i16> = keys
        .iter()
        .map(Key::shard)
        .chain([object.shard()])
        .collect();
    shards.sort_unstable();
    shards.dedup();
    shards
}
fn keys_in(keys: &[Key], shard: i16) -> Vec<&Key> {
    keys.iter().filter(|key| key.shard() == shard).collect()
}

async fn locked_copy(tx: &mut Tx, shard: i16, object: Object) -> Result<Option<Home>> {
    Ok(sqlx::query_as("SELECT home_cell,active,claim_count,key_count,nonce_hash,COALESCE(expires_at>clock_timestamp(),false) AS live,expires_at FROM directory_objects WHERE shard=$1 AND kind=$2 AND id=$3 FOR UPDATE")
        .bind(shard).bind(object.kind.as_str()).bind(object.id).fetch_optional(&mut **tx).await?)
}
async fn home(tx: &mut Tx, shard: i16, object: Object, cell: Prefix) -> Result<Home> {
    let row = locked_copy(tx, shard, object)
        .await?
        .ok_or(Refusal::Denied)?;
    ensure!(row.home_cell == i32::from(cell.get()), Refusal::Denied);
    Ok(row)
}

/// The cell that owns the prefix of the object's id, which is its home until the directory
/// records one.
async fn prefix_owner(registry: &mut Tx, object: Object) -> Result<Option<i32>> {
    Ok(
        sqlx::query_scalar("SELECT cell_id FROM directory_cell_prefixes WHERE prefix=$1")
            .bind(i32::from(
                Prefix::for_id(object.id)
                    .map_err(|_| Refusal::Invalid)?
                    .get(),
            ))
            .fetch_optional(&mut **registry)
            .await?,
    )
}

/// Only the object's home cell, or the prefix owner of an object never recorded, removes it.
async fn removal_home(
    tx: &mut Tx,
    object: Object,
    prefix_owner: Option<i32>,
    cell: Prefix,
) -> Result<()> {
    let owner = match locked_copy(tx, object.shard(), object).await? {
        Some(row) => Some(row.home_cell),
        None => prefix_owner,
    };
    ensure!(owner == Some(i32::from(cell.get())), Refusal::Denied);
    Ok(())
}

async fn claim(tx: &mut Tx, key: &Key) -> Result<Claim> {
    Ok(sqlx::query_as("SELECT value,object_kind,object_id,nonce_hash,confirmed,COALESCE(expires_at>clock_timestamp(),false) AS live FROM directory_claims WHERE shard=$1 AND kind=$2 AND key_hash=$3 FOR UPDATE")
        .bind(key.shard()).bind(key.kind().as_str()).bind(key.hash().to_vec()).fetch_optional(&mut **tx).await?.ok_or(Refusal::Conflict)?)
}

/// Commits every cluster in order, then the registry, whose shard locks fence a move until
/// the clusters are done. Any failure rolls back what has not committed yet.
async fn finish<T>(registry: Tx, clusters: Opened, result: Result<T>) -> Result<T> {
    let mut clusters = clusters.into_iter().filter_map(|(_, tx)| tx);
    let failure = match result {
        Ok(value) => {
            let mut failure = None;
            for tx in clusters.by_ref() {
                if let Err(error) = tx.commit().await {
                    failure = Some(error.into());
                    break;
                }
            }
            match failure {
                None => {
                    registry.commit().await?;
                    return Ok(value);
                }
                Some(error) => error,
            }
        }
        Err(error) => error,
    };
    for tx in clusters {
        let _ = tx.rollback().await;
    }
    let _ = registry.rollback().await;
    Err(failure)
}

impl Directory {
    /// Opens the clusters of the locked shards, the object's home cluster first.
    async fn open_clusters(&self, groups: shards::Groups, origin: i16) -> Result<Opened> {
        let mut opened = Vec::new();
        for (cluster, shards) in shards::ordered(groups, origin) {
            let tx = if cluster == MAIN_CLUSTER {
                None
            } else {
                Some(self.begin_on(self.cluster(&cluster)?).await?)
            };
            opened.push((shards, tx));
        }
        Ok(opened)
    }

    /// `peer` is supplied only by the authenticated transport, never by a request field.
    /// The caller reserves keys, creates its private pending local object, then confirms.
    pub async fn reserve(
        &self,
        peer: &str,
        object: Object,
        keys: &[Key],
        nonce: &str,
    ) -> Result<Option<DateTime<Utc>>> {
        self.run(async {
            let (keys, nonce) = checked(object, keys, nonce)?;
            let origin = object.shard();
            let mut registry = self.begin().await?;
            let mut opened = Vec::new();
            let result = async {
                let cell = super::owner(&mut registry, peer, false).await?;
                let owned = prefix_owner(&mut registry, object).await? == Some(i32::from(cell.get()));
                let open: bool = if object.kind == ObjectKind::Repository {
                    true
                } else {
                    sqlx::query_scalar("SELECT state='open' AND database_bytes::numeric<maximum_database_bytes::numeric/2 AND write_rate::numeric<maximum_write_rate::numeric/2 FROM directory_cells WHERE id=$1 AND observed_at>clock_timestamp()-$2::bigint*INTERVAL '1 second'")
                        .bind(i32::from(cell.get())).bind(self.config.stale_seconds).fetch_optional(&mut *registry).await?.unwrap_or(false)
                };
                let groups = shards::lock_for_write(&mut registry, &touched(object, &keys)).await?;
                opened = self.open_clusters(groups, origin).await?;
                // (active, nonce hash, expiry) of the home row, which every copy follows.
                let mut root: Option<(bool, String, Option<DateTime<Utc>>)> = None;
                for (shards, tx) in opened.iter_mut() {
                    let tx = pick(&mut registry, tx);
                    for &shard in shards.iter() {
                        let mine = keys_in(&keys, shard);
                        let copy = if shard == origin {
                            if owned {
                                sqlx::query("INSERT INTO directory_objects(shard,kind,id,home_cell,key_count,nonce_hash,expires_at) VALUES($1,$2,$3,$4,$5,$6,clock_timestamp()+$7::bigint*INTERVAL '1 second') ON CONFLICT DO NOTHING")
                                    .bind(shard).bind(object.kind.as_str()).bind(object.id).bind(i32::from(cell.get()))
                                    .bind(keys.len() as i32).bind(&nonce).bind(self.config.claim_seconds).execute(&mut **tx).await?;
                            }
                            let row = home(tx, shard, object, cell).await?;
                            if !row.active {
                                ensure!(row.live && row.nonce_hash == nonce, Refusal::Conflict);
                                ensure!(row.key_count as usize == keys.len(), Refusal::Conflict);
                                ensure!(object.kind == ObjectKind::Repository || keys.iter().filter(|key| key.kind()==KeyKind::Name).count()==1, Refusal::Invalid);
                                ensure!(open, Refusal::Unavailable);
                            }
                            root = Some((row.active, row.nonce_hash.clone(), row.expires_at));
                            row
                        } else {
                            let (active, root_nonce, expires_at) = root.clone().ok_or(Refusal::Unavailable)?;
                            sqlx::query("INSERT INTO directory_objects(shard,kind,id,home_cell,active,nonce_hash,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT DO NOTHING")
                                .bind(shard).bind(object.kind.as_str()).bind(object.id).bind(i32::from(cell.get()))
                                .bind(active).bind(&root_nonce).bind(expires_at).execute(&mut **tx).await?;
                            let mut row = home(tx, shard, object, cell).await?;
                            // A copy left pending by an interrupted confirmation follows its active home row.
                            if active && !row.active {
                                sqlx::query("UPDATE directory_objects SET active=true,expires_at=NULL WHERE shard=$1 AND kind=$2 AND id=$3")
                                    .bind(shard).bind(object.kind.as_str()).bind(object.id).execute(&mut **tx).await?;
                                row.active = true;
                            }
                            if !row.active {
                                ensure!(row.live && row.nonce_hash == nonce, Refusal::Conflict);
                            }
                            row
                        };
                        for key in &mine {
                            sqlx::query("INSERT INTO directory_claims(shard,kind,key_hash,value,object_kind,object_id,nonce_hash,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,clock_timestamp()+$8::bigint*INTERVAL '1 second') ON CONFLICT(shard,kind,key_hash) DO UPDATE SET object_kind=excluded.object_kind,object_id=excluded.object_id,nonce_hash=excluded.nonce_hash,expires_at=excluded.expires_at WHERE directory_claims.value=excluded.value AND NOT directory_claims.confirmed AND directory_claims.expires_at<=clock_timestamp()")
                                .bind(key.shard()).bind(key.kind().as_str()).bind(key.hash().to_vec()).bind(key.value()).bind(object.kind.as_str()).bind(object.id).bind(&nonce).bind(self.config.claim_seconds).execute(&mut **tx).await?;
                            let row = claim(tx, key).await?;
                            ensure!(row.belongs(object, key) && (row.confirmed || (row.live && row.nonce_hash==nonce)), Refusal::Conflict);
                        }
                        if !copy.active {
                            let current = home(tx, shard, object, cell).await?;
                            ensure!(current.claim_count as usize == mine.len(), Refusal::Conflict);
                        }
                    }
                }
                let (_, _, expires_at) = root.ok_or(Refusal::Unavailable)?;
                Ok::<_, anyhow::Error>(expires_at)
            }
            .await;
            finish(registry, opened, result).await
        }).await
    }

    /// All keys of a new object activate together. Retrying the same confirmation is safe.
    pub async fn confirm(
        &self,
        peer: &str,
        object: Object,
        keys: &[Key],
        nonce: &str,
    ) -> Result<()> {
        self.run(async {
            let (keys, nonce) = checked(object, keys, nonce)?;
            let origin = object.shard();
            let mut registry = self.begin().await?;
            let mut opened = Vec::new();
            let result = async {
                let cell = super::owner(&mut registry, peer, false).await?;
                let groups = shards::lock_for_write(&mut registry, &touched(object, &keys)).await?;
                opened = self.open_clusters(groups, origin).await?;
                for (shards, tx) in opened.iter_mut() {
                    let tx = pick(&mut registry, tx);
                    for &shard in shards.iter() {
                        let mine = keys_in(&keys, shard);
                        let copy = home(tx, shard, object, cell).await?;
                        if !copy.active {
                            ensure!(copy.live && copy.nonce_hash==nonce, Refusal::Conflict);
                            ensure!(copy.claim_count as usize == mine.len(), Refusal::Conflict);
                            // A partial confirmation may not publish a subset of the reservation.
                            ensure!(shard != origin || copy.key_count as usize == keys.len(), Refusal::Conflict);
                        }
                        for key in mine {
                            let row = claim(tx, key).await?;
                            ensure!(row.belongs(object, key) && (row.confirmed || (row.live && row.nonce_hash==nonce)), Refusal::Conflict);
                            sqlx::query("UPDATE directory_claims SET confirmed=true,expires_at=NULL WHERE shard=$1 AND kind=$2 AND key_hash=$3")
                                .bind(key.shard()).bind(key.kind().as_str()).bind(key.hash().to_vec()).execute(&mut **tx).await?;
                        }
                        sqlx::query("UPDATE directory_objects SET active=true,expires_at=NULL WHERE shard=$1 AND kind=$2 AND id=$3")
                            .bind(shard).bind(object.kind.as_str()).bind(object.id).execute(&mut **tx).await?;
                    }
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            finish(registry, opened, result).await
        }).await
    }

    /// Removing a key never removes a replacement owned by another object.
    pub async fn release(&self, peer: &str, object: Object, keys: &[Key]) -> Result<()> {
        self.run(async {
            let keys = checked_keys(object, keys)?;
            let origin = object.shard();
            let mut registry = self.begin().await?;
            let mut opened = Vec::new();
            let result = async {
                let cell = super::owner(&mut registry, peer, false).await?;
                let prefix_owner = prefix_owner(&mut registry, object).await?;
                let groups = shards::lock_for_write(&mut registry, &touched(object, &keys)).await?;
                opened = self.open_clusters(groups, origin).await?;
                for (shards, tx) in opened.iter_mut() {
                    let tx = pick(&mut registry, tx);
                    for &shard in shards.iter() {
                        if shard == origin {
                            removal_home(tx, object, prefix_owner, cell).await?;
                        }
                        for key in keys_in(&keys, shard) {
                            sqlx::query("DELETE FROM directory_claims WHERE shard=$1 AND kind=$2 AND key_hash=$3 AND value=$4 AND object_kind=$5 AND object_id=$6")
                                .bind(key.shard()).bind(key.kind().as_str()).bind(key.hash().to_vec())
                                .bind(key.value()).bind(object.kind.as_str()).bind(object.id)
                                .execute(&mut **tx).await?;
                        }
                    }
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            finish(registry, opened, result).await
        }).await
    }

    /// Used only after native deletion, or while the native namespace lock proves an orphan.
    /// It locks the whole shard map: a shard that is being copied and holds a copy of the
    /// object refuses until the move is done; the cell's cleanup retries.
    pub async fn forget(&self, peer: &str, object: Object) -> Result<()> {
        self.run(async {
            object.validate().map_err(|_| Refusal::Invalid)?;
            let origin = object.shard();
            let mut registry = self.begin().await?;
            let mut opened = Vec::new();
            let result = async {
                let cell = super::owner(&mut registry, peer, false).await?;
                let prefix_owner = prefix_owner(&mut registry, object).await?;
                let (groups, moving) = shards::lock_all(&mut registry).await?;
                for (cluster, shards) in moving {
                    let query = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM directory_objects WHERE kind=$1 AND id=$2 AND shard=ANY($3))")
                        .bind(object.kind.as_str()).bind(object.id).bind(shards);
                    let held: bool = if cluster == MAIN_CLUSTER {
                        query.fetch_one(&mut *registry).await?
                    } else {
                        query.fetch_one(self.cluster(&cluster)?).await?
                    };
                    ensure!(!held, Refusal::Unavailable);
                }
                opened = self.open_clusters(groups, origin).await?;
                for (shards, tx) in opened.iter_mut() {
                    let tx = pick(&mut registry, tx);
                    if shards.contains(&origin) {
                        removal_home(tx, object, prefix_owner, cell).await?;
                    }
                    // The schema bounds each copy's keys to 1000; its FK removes them together.
                    sqlx::query("DELETE FROM directory_objects WHERE kind=$1 AND id=$2 AND home_cell=$3 AND shard=ANY($4)")
                        .bind(object.kind.as_str())
                        .bind(object.id)
                        .bind(i32::from(cell.get()))
                        .bind(&*shards)
                        .execute(&mut **tx)
                        .await?;
                }
                if object.kind == ObjectKind::Organization {
                    sqlx::query("DELETE FROM directory_presence WHERE organization_id=$1")
                        .bind(object.id)
                        .execute(&mut *registry)
                        .await?;
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            finish(registry, opened, result).await
        })
        .await
    }

    pub async fn lookup(&self, key: &Key) -> Result<Option<Location>> {
        self.lookup_for(None, key).await
    }

    pub async fn lookup_for(&self, peer: Option<&str>, key: &Key) -> Result<Option<Location>> {
        self.run(async {
            let mut registry = self.begin().await?;
            if let Some(peer) = peer { super::read_owner(&mut registry, peer).await?; }
            let cluster = shards::cluster_of(&mut registry, key.shard()).await?;
            let query = sqlx::query_as("SELECT o.home_cell,o.kind,o.id FROM directory_claims k JOIN directory_objects o ON o.shard=k.shard AND o.kind=k.object_kind AND o.id=k.object_id WHERE k.shard=$1 AND k.kind=$2 AND k.key_hash=$3 AND k.value=$4 AND k.confirmed AND o.active")
                .bind(key.shard()).bind(key.kind().as_str()).bind(key.hash().to_vec()).bind(key.value());
            let row: Option<(i32,String,i64)> = if cluster == MAIN_CLUSTER {
                query.fetch_optional(&mut *registry).await?
            } else {
                query.fetch_optional(self.cluster(&cluster)?).await?
            };
            let location = row.map(|(cell,kind,id)| -> Result<_> { Ok(Location { cell: Prefix::try_from(i64::from(cell))?, object: Object { kind: ObjectKind::parse(&kind).ok_or(Refusal::Unavailable)?, id } }) }).transpose()?;
            registry.commit().await?;
            Ok(location)
        }).await
    }

    pub async fn object_home(&self, object: Object) -> Result<Option<Prefix>> {
        self.object_home_for(None, object).await
    }

    pub async fn object_home_for(
        &self,
        peer: Option<&str>,
        object: Object,
    ) -> Result<Option<Prefix>> {
        self.run(async {
            object.validate().map_err(|_| Refusal::Invalid)?;
            let mut registry = self.begin().await?;
            if let Some(peer) = peer {
                super::read_owner(&mut registry, peer).await?;
            }
            let cell = match self.active_home(&mut registry, object).await? {
                Some(cell) => Some(cell),
                None => prefix_owner(&mut registry, object).await?,
            };
            let cell = cell
                .map(|cell| Prefix::try_from(i64::from(cell)))
                .transpose()?;
            registry.commit().await?;
            Ok(cell)
        })
        .await
    }

    /// The home cell recorded on the object's home row, read from the cluster the map names.
    pub(super) async fn active_home(
        &self,
        registry: &mut Tx,
        object: Object,
    ) -> Result<Option<i32>> {
        let shard = object.shard();
        let cluster = shards::cluster_of(registry, shard).await?;
        let query = sqlx::query_scalar(
            "SELECT home_cell FROM directory_objects WHERE shard=$1 AND kind=$2 AND id=$3 AND active",
        )
        .bind(shard)
        .bind(object.kind.as_str())
        .bind(object.id);
        Ok(if cluster == MAIN_CLUSTER {
            query.fetch_optional(&mut **registry).await?
        } else {
            query.fetch_optional(self.cluster(&cluster)?).await?
        })
    }

    /// The object's recorded home, or for an object the directory never recorded the owner
    /// of its id's prefix, must be `cell`. Search writes use it.
    pub(super) async fn require_home(
        &self,
        registry: &mut Tx,
        object: Object,
        cell: Prefix,
    ) -> Result<()> {
        let shard = object.shard();
        let cluster = shards::cluster_of(registry, shard).await?;
        let query = sqlx::query_scalar(
            "SELECT home_cell FROM directory_objects WHERE shard=$1 AND kind=$2 AND id=$3",
        )
        .bind(shard)
        .bind(object.kind.as_str())
        .bind(object.id);
        let home: Option<i32> = if cluster == MAIN_CLUSTER {
            query.fetch_optional(&mut **registry).await?
        } else {
            query.fetch_optional(self.cluster(&cluster)?).await?
        };
        let owner = match home {
            Some(home) => Some(home),
            None => prefix_owner(registry, object).await?,
        };
        ensure!(owner == Some(i32::from(cell.get())), Refusal::Denied);
        Ok(())
    }

    /// Cleanup is bounded and pending objects cascade to their unpublished key reservations.
    /// It holds the shard map rows of the shards it cleans, so a move never copies a shard
    /// while rows of it are being removed.
    pub async fn collect(&self) -> Result<()> {
        self.run(async {
            let mut registry = self.begin().await?;
            let mut opened = Vec::new();
            let result = async {
                let (groups, _) = shards::lock_all(&mut registry).await?;
                opened = self.open_clusters(groups, 0).await?;
                for (shards, tx) in opened.iter_mut() {
                    let tx = pick(&mut registry, tx);
                    sqlx::query("DELETE FROM directory_objects WHERE (shard,kind,id) IN (SELECT shard,kind,id FROM directory_objects WHERE NOT active AND expires_at<=clock_timestamp() AND shard=ANY($1) ORDER BY expires_at,shard,kind,id LIMIT 100 FOR UPDATE SKIP LOCKED)")
                        .bind(&*shards).execute(&mut **tx).await?;
                    sqlx::query("DELETE FROM directory_claims WHERE (shard,kind,key_hash) IN (SELECT shard,kind,key_hash FROM directory_claims WHERE NOT confirmed AND expires_at<=clock_timestamp() AND shard=ANY($1) ORDER BY expires_at,shard,kind,key_hash LIMIT 100 FOR UPDATE SKIP LOCKED)")
                        .bind(&*shards).execute(&mut **tx).await?;
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            finish(registry, opened, result).await
        }).await
    }
}
