//! Regions where an organization's reads are served by its home cell's regional web nodes.
//! Every write still goes to the one home cell.
use super::{Directory, Refusal};
use anyhow::{ensure, Result};

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

    /// Local operator only. Returns whether presence changed. The organization's directory row
    /// is locked first, so two additions cannot both pass the region limit.
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
            let known: Option<i64> = sqlx::query_scalar(
                "SELECT id FROM directory_objects WHERE kind='organization' AND id=$1 AND active FOR UPDATE",
            )
            .bind(organization)
            .fetch_optional(&mut *tx)
            .await?;
            ensure!(known.is_some(), Refusal::Invalid);
            let changed = if present {
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM directory_presence WHERE organization_id=$1 AND region<>$2",
                )
                .bind(organization)
                .bind(region)
                .fetch_one(&mut *tx)
                .await?;
                ensure!(count < MAXIMUM_PRESENCE_REGIONS, Refusal::Invalid);
                sqlx::query("INSERT INTO directory_presence(organization_id,region,manual) VALUES($1,$2,true) ON CONFLICT(organization_id,region) DO NOTHING")
                    .bind(organization)
                    .bind(region)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected()
                    > 0
            } else {
                sqlx::query("DELETE FROM directory_presence WHERE organization_id=$1 AND region=$2")
                    .bind(organization)
                    .bind(region)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected()
                    > 0
            };
            tx.commit().await?;
            Ok(changed)
        })
        .await
    }
}
