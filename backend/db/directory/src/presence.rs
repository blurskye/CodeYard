//! Regions where an organization's reads are served by its home cell's regional web nodes.
//! Every write still goes to the one home cell.
use super::{Directory, Refusal};
use anyhow::{ensure, Result};
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
                let home: Option<i32> = sqlx::query_scalar(
                    "SELECT home_cell FROM directory_objects WHERE kind='organization' AND id=$1 AND active FOR UPDATE",
                )
                .bind(organization)
                .fetch_optional(&mut *tx)
                .await?;
                ensure!(home.is_some(), Refusal::Invalid);
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
                let cell = super::owner(&mut tx, peer, false).await?;
                let home: Option<String> = sqlx::query_scalar(
                    "SELECT c.region FROM directory_objects o JOIN directory_cells c ON c.id=o.home_cell WHERE o.kind='organization' AND o.id=$1 AND o.active AND o.home_cell=$2 FOR UPDATE OF o",
                )
                .bind(organization)
                .bind(i32::from(cell.get()))
                .fetch_optional(&mut *tx)
                .await?;
                let home = home.ok_or(Refusal::Denied)?;
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

/// The caller holds the organization's directory row, so two additions cannot both pass
/// the region limit.
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
