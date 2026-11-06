//! Regions where an organization's reads are served by its home cell's regional web nodes.
//! Every write still goes to the one home cell.
use super::{Directory, Refusal};
use anyhow::{ensure, Result};
use codeyard_model::cells::{Object, ObjectKind};
use sqlx::{Postgres, Transaction};

/// An organization is present in at most this many regions besides its home.
pub const MAXIMUM_PRESENCE_REGIONS: i64 = 16;
const MAXIMUM_ID: i64 = 9_007_199_254_740_991;

impl Directory {
    pub async fn presence(&self, organization: i64) -> Result<Vec<String>> {
        self.presence_for(None, organization).await
    }

    pub async fn presence_for(&self, peer: Option<&str>, organization: i64) -> Result<Vec<String>> {
        self.run(async {
            ensure!((1..=MAXIMUM_ID).contains(&organization), Refusal::Invalid);
            let mut tx = self.begin().await?;
            if let Some(peer) = peer {
                super::read_owner(&mut tx, peer).await?;
            }
            let regions: Vec<String> = sqlx::query_scalar(
                "SELECT region FROM directory_presence WHERE organization_id=$1 ORDER BY region LIMIT $2",
            )
            .bind(organization)
            .bind(MAXIMUM_PRESENCE_REGIONS)
            .fetch_all(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok(regions)
        })
        .await
    }

    /// Local operator only. Returns whether presence changed. Hand-set presence is never
    /// removed by a cell's automatic evaluation.
    pub async fn set_presence(
        &self,
        organization: i64,
        region: &str,
        present: bool,
    ) -> Result<bool> {
        self.run(async {
            ensure!((1..=MAXIMUM_ID).contains(&organization), Refusal::Invalid);
            codeyard_model::cells::validate_region(region).map_err(|_| Refusal::Invalid)?;
            let mut tx = self.begin().await?;
            let result = async {
                let home = self
                    .active_home(&mut tx, organization_object(organization))
                    .await?;
                let home = home.ok_or(Refusal::Invalid)?;
                // Presence changes of one cell's organizations take turns, so two additions
                // cannot both pass the region limit.
                sqlx::query("SELECT id FROM directory_cells WHERE id=$1 FOR UPDATE")
                    .bind(home)
                    .fetch_optional(&mut *tx)
                    .await?
                    .ok_or(Refusal::Invalid)?;
                // Forgetting the organization holds the same row; read the home again under it.
                let current = self
                    .active_home(&mut tx, organization_object(organization))
                    .await?;
                ensure!(current == Some(home), Refusal::Invalid);
                change(&mut tx, organization, region, present, true).await
            }
            .await;
            super::settle(tx, result).await
        })
        .await
    }

    /// The organization's home cell adds or removes presence it computed from counted member
    /// requests. Its own region is never a presence region, and hand-set rows stay.
    pub async fn set_presence_for(
        &self,
        peer: &str,
        organization: i64,
        region: &str,
        present: bool,
    ) -> Result<bool> {
        self.run(async {
            ensure!((1..=MAXIMUM_ID).contains(&organization), Refusal::Invalid);
            codeyard_model::cells::validate_region(region).map_err(|_| Refusal::Invalid)?;
            let mut tx = self.begin().await?;
            let result = async {
                // The exclusive cell lock also makes this cell's presence changes take turns.
                let cell = super::owner(&mut tx, peer, true).await?;
                let home = self
                    .active_home(&mut tx, organization_object(organization))
                    .await?;
                ensure!(home == Some(i32::from(cell.get())), Refusal::Denied);
                let home: String =
                    sqlx::query_scalar("SELECT region FROM directory_cells WHERE id=$1")
                        .bind(i32::from(cell.get()))
                        .fetch_one(&mut *tx)
                        .await?;
                if home == region {
                    return Ok(false);
                }
                change(&mut tx, organization, region, present, false).await
            }
            .await;
            super::settle(tx, result).await
        })
        .await
    }
}

fn organization_object(id: i64) -> Object {
    Object {
        kind: ObjectKind::Organization,
        id,
    }
}

/// The caller holds the home cell's row, so two additions cannot both pass the region limit.
async fn change(
    tx: &mut Transaction<'_, Postgres>,
    organization: i64,
    region: &str,
    present: bool,
    manual: bool,
) -> Result<bool> {
    Ok(if present {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM directory_presence WHERE organization_id=$1 AND region<>$2",
        )
        .bind(organization)
        .bind(region)
        .fetch_one(&mut **tx)
        .await?;
        ensure!(count < MAXIMUM_PRESENCE_REGIONS, Refusal::Invalid);
        // A hand-set row stays hand-set; an operator's addition marks an automatic row as such.
        sqlx::query("INSERT INTO directory_presence(organization_id,region,manual) VALUES($1,$2,$3) ON CONFLICT(organization_id,region) DO UPDATE SET manual=true WHERE $3 AND NOT directory_presence.manual")
            .bind(organization)
            .bind(region)
            .bind(manual)
            .execute(&mut **tx)
            .await?
            .rows_affected()
            > 0
    } else {
        sqlx::query(
            "DELETE FROM directory_presence WHERE organization_id=$1 AND region=$2 AND ($3 OR NOT manual)",
        )
        .bind(organization)
        .bind(region)
        .bind(manual)
        .execute(&mut **tx)
        .await?
        .rows_affected()
            > 0
    })
}
