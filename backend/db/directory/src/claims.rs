//! Pending keys are invisible until their local object has been created and activated.
use super::{Directory, Refusal};
use anyhow::{ensure, Result};
use chrono::{DateTime, Utc};
use codeyard_model::cells::{Key, KeyKind, Location, Object, ObjectKind};
use codeyard_support::{cells::Prefix, token_hash::token_hash};
use sqlx::{Postgres, Transaction};

#[derive(sqlx::FromRow)]
struct Home {
    home_cell: i32,
    active: bool,
    claim_count: i32,
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

async fn locked_home(tx: &mut Transaction<'_, Postgres>, object: Object) -> Result<Option<Home>> {
    Ok(sqlx::query_as("SELECT home_cell,active,claim_count,nonce_hash,COALESCE(expires_at>clock_timestamp(),false) AS live,expires_at FROM directory_objects WHERE kind=$1 AND id=$2 FOR UPDATE")
        .bind(object.kind.as_str()).bind(object.id).fetch_optional(&mut **tx).await?)
}
async fn home(tx: &mut Transaction<'_, Postgres>, object: Object, cell: Prefix) -> Result<Home> {
    let row = locked_home(tx, object).await?.ok_or(Refusal::Denied)?;
    ensure!(row.home_cell == i32::from(cell.get()), Refusal::Denied);
    Ok(row)
}

async fn removal_home(
    tx: &mut Transaction<'_, Postgres>,
    object: Object,
    cell: Prefix,
) -> Result<()> {
    let owner = locked_home(tx, object).await?.map(|row| row.home_cell);
    let owner = match owner {
        Some(owner) => Some(owner),
        None => {
            sqlx::query_scalar("SELECT cell_id FROM directory_cell_prefixes WHERE prefix=$1")
                .bind(i32::from(
                    Prefix::for_id(object.id)
                        .map_err(|_| Refusal::Invalid)?
                        .get(),
                ))
                .fetch_optional(&mut **tx)
                .await?
        }
    };
    ensure!(owner == Some(i32::from(cell.get())), Refusal::Denied);
    Ok(())
}

async fn claim(tx: &mut Transaction<'_, Postgres>, key: &Key) -> Result<Claim> {
    Ok(sqlx::query_as("SELECT value,object_kind,object_id,nonce_hash,confirmed,COALESCE(expires_at>clock_timestamp(),false) AS live FROM directory_claims WHERE shard=$1 AND kind=$2 AND key_hash=$3 FOR UPDATE")
        .bind(key.shard()).bind(key.kind().as_str()).bind(key.hash().to_vec()).fetch_optional(&mut **tx).await?.ok_or(Refusal::Conflict)?)
}

impl Directory {
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
            let mut tx = self.begin().await?;
            let cell = super::owner(&mut tx, peer, false).await?;
            let prefix = Prefix::for_id(object.id).map_err(|_| Refusal::Invalid)?;
            let owned: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM directory_cell_prefixes WHERE prefix=$1 AND cell_id=$2)")
                .bind(i32::from(prefix.get())).bind(i32::from(cell.get())).fetch_one(&mut *tx).await?;
            if owned {
                sqlx::query("INSERT INTO directory_objects(kind,id,home_cell,nonce_hash,expires_at) VALUES($1,$2,$3,$4,clock_timestamp()+$5::bigint*INTERVAL '1 second') ON CONFLICT DO NOTHING")
                    .bind(object.kind.as_str()).bind(object.id).bind(i32::from(cell.get())).bind(&nonce).bind(self.config.claim_seconds).execute(&mut *tx).await?;
            }
            let root = home(&mut tx, object, cell).await?;
            if !root.active {
                ensure!(root.live && root.nonce_hash == nonce, Refusal::Conflict);
                ensure!(object.kind == ObjectKind::Repository || keys.iter().filter(|key| key.kind()==KeyKind::Name).count()==1, Refusal::Invalid);
                if object.kind != ObjectKind::Repository {
                    let open: bool = sqlx::query_scalar("SELECT state='open' AND database_bytes::numeric<maximum_database_bytes::numeric/2 AND write_rate::numeric<maximum_write_rate::numeric/2 FROM directory_cells WHERE id=$1 AND observed_at>clock_timestamp()-$2::bigint*INTERVAL '1 second'")
                        .bind(i32::from(cell.get())).bind(self.config.stale_seconds).fetch_one(&mut *tx).await?;
                    ensure!(open, Refusal::Unavailable);
                }
            }
            for key in &keys {
                sqlx::query("INSERT INTO directory_claims(shard,kind,key_hash,value,object_kind,object_id,nonce_hash,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,clock_timestamp()+$8::bigint*INTERVAL '1 second') ON CONFLICT(shard,kind,key_hash) DO UPDATE SET object_kind=excluded.object_kind,object_id=excluded.object_id,nonce_hash=excluded.nonce_hash,expires_at=excluded.expires_at WHERE directory_claims.value=excluded.value AND NOT directory_claims.confirmed AND directory_claims.expires_at<=clock_timestamp()")
                    .bind(key.shard()).bind(key.kind().as_str()).bind(key.hash().to_vec()).bind(key.value()).bind(object.kind.as_str()).bind(object.id).bind(&nonce).bind(self.config.claim_seconds).execute(&mut *tx).await?;
                let row = claim(&mut tx, key).await?;
                ensure!(row.belongs(object, key) && (row.confirmed || (row.live && row.nonce_hash==nonce)), Refusal::Conflict);
            }
            if !root.active {
                let current = home(&mut tx, object, cell).await?;
                ensure!(current.claim_count as usize == keys.len(), Refusal::Conflict);
            }
            tx.commit().await?;
            Ok(root.expires_at)
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
            let mut tx = self.begin().await?;
            let cell = super::owner(&mut tx, peer, false).await?;
            let root = home(&mut tx, object, cell).await?;
            if !root.active {
                ensure!(root.live && root.nonce_hash==nonce, Refusal::Conflict);
                ensure!(root.claim_count as usize == keys.len(), Refusal::Conflict);
            }
            for key in &keys {
                let row = claim(&mut tx, key).await?;
                ensure!(row.belongs(object, key) && (row.confirmed || (row.live && row.nonce_hash==nonce)), Refusal::Conflict);
                sqlx::query("UPDATE directory_claims SET confirmed=true,expires_at=NULL WHERE shard=$1 AND kind=$2 AND key_hash=$3")
                    .bind(key.shard()).bind(key.kind().as_str()).bind(key.hash().to_vec()).execute(&mut *tx).await?;
            }
            sqlx::query("UPDATE directory_objects SET active=true,expires_at=NULL WHERE kind=$1 AND id=$2")
                .bind(object.kind.as_str()).bind(object.id).execute(&mut *tx).await?;
            tx.commit().await?;
            Ok(())
        }).await
    }

    /// Removing a key never removes a replacement owned by another object.
    pub async fn release(&self, peer: &str, object: Object, keys: &[Key]) -> Result<()> {
        self.run(async {
            let keys = checked_keys(object, keys)?;
            let mut tx = self.begin().await?;
            let cell = super::owner(&mut tx, peer, false).await?;
            removal_home(&mut tx, object, cell).await?;
            for key in keys {
                sqlx::query("DELETE FROM directory_claims WHERE shard=$1 AND kind=$2 AND key_hash=$3 AND value=$4 AND object_kind=$5 AND object_id=$6")
                    .bind(key.shard()).bind(key.kind().as_str()).bind(key.hash().to_vec())
                    .bind(key.value()).bind(object.kind.as_str()).bind(object.id)
                    .execute(&mut *tx).await?;
            }
            tx.commit().await?;
            Ok(())
        }).await
    }

    /// Used only after native deletion, or while the native namespace lock proves an orphan.
    pub async fn forget(&self, peer: &str, object: Object) -> Result<()> {
        self.run(async {
            object.validate().map_err(|_| Refusal::Invalid)?;
            let mut tx = self.begin().await?;
            let cell = super::owner(&mut tx, peer, false).await?;
            removal_home(&mut tx, object, cell).await?;
            // The schema bounds an object's keys to 1000; its existing FK removes them together.
            sqlx::query("DELETE FROM directory_objects WHERE kind=$1 AND id=$2 AND home_cell=$3")
                .bind(object.kind.as_str())
                .bind(object.id)
                .bind(i32::from(cell.get()))
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    pub async fn lookup(&self, key: &Key) -> Result<Option<Location>> {
        self.lookup_for(None, key).await
    }

    pub async fn lookup_for(&self, peer: Option<&str>, key: &Key) -> Result<Option<Location>> {
        self.run(async {
            let mut tx = self.begin().await?;
            if let Some(peer) = peer { super::read_owner(&mut tx, peer).await?; }
            let row: Option<(i32,String,i64)> = sqlx::query_as("SELECT o.home_cell,o.kind,o.id FROM directory_claims k JOIN directory_objects o ON o.kind=k.object_kind AND o.id=k.object_id WHERE k.shard=$1 AND k.kind=$2 AND k.key_hash=$3 AND k.value=$4 AND k.confirmed AND o.active")
                .bind(key.shard()).bind(key.kind().as_str()).bind(key.hash().to_vec()).bind(key.value()).fetch_optional(&mut *tx).await?;
            let location = row.map(|(cell,kind,id)| -> Result<_> { Ok(Location { cell: Prefix::try_from(i64::from(cell))?, object: Object { kind: ObjectKind::parse(&kind).ok_or(Refusal::Unavailable)?, id } }) }).transpose()?;
            tx.commit().await?;
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
            let mut tx = self.begin().await?;
            if let Some(peer) = peer {
                super::read_owner(&mut tx, peer).await?;
            }
            let cell: Option<i32> = sqlx::query_scalar(
                "SELECT home_cell FROM directory_objects WHERE kind=$1 AND id=$2 AND active",
            )
            .bind(object.kind.as_str())
            .bind(object.id)
            .fetch_optional(&mut *tx)
            .await?;
            let cell = match cell {
                Some(cell) => Some(Prefix::try_from(i64::from(cell))?),
                None => {
                    let origin = Prefix::for_id(object.id)?;
                    let cell: Option<i32> = sqlx::query_scalar(
                        "SELECT cell_id FROM directory_cell_prefixes WHERE prefix=$1",
                    )
                    .bind(i32::from(origin.get()))
                    .fetch_optional(&mut *tx)
                    .await?;
                    cell.map(|cell| Prefix::try_from(i64::from(cell)))
                        .transpose()?
                }
            };
            tx.commit().await?;
            Ok(cell)
        })
        .await
    }

    /// Cleanup is bounded and pending objects cascade to their unpublished key reservations.
    pub async fn collect(&self) -> Result<()> {
        self.run(async {
            let mut tx = self.begin().await?;
            sqlx::query("DELETE FROM directory_objects WHERE (kind,id) IN (SELECT kind,id FROM directory_objects WHERE NOT active AND expires_at<=clock_timestamp() ORDER BY expires_at,kind,id LIMIT 100 FOR UPDATE SKIP LOCKED)")
                .execute(&mut *tx).await?;
            sqlx::query("DELETE FROM directory_claims WHERE (shard,kind,key_hash) IN (SELECT shard,kind,key_hash FROM directory_claims WHERE NOT confirmed AND expires_at<=clock_timestamp() ORDER BY expires_at,shard,kind,key_hash LIMIT 100 FOR UPDATE SKIP LOCKED)")
                .execute(&mut *tx).await?;
            tx.commit().await?;
            Ok(())
        }).await
    }
}
