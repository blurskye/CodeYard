use super::{Directory, Refusal};
use anyhow::{ensure, Result};
use codeyard_model::cells::{Cell, State};
use codeyard_support::cells::Prefix;

const COLUMNS: &str = "c.id,c.name,c.region,c.state,c.web_urls::text,c.ssh_addresses::text,c.database_bytes,c.write_rate,c.maximum_database_bytes,c.maximum_write_rate,(SELECT COALESCE(json_agg(public_key)::text,'[]') FROM (SELECT public_key FROM directory_node_keys WHERE cell_id=c.id ORDER BY public_key LIMIT 1001) keys) AS node_keys";

#[derive(sqlx::FromRow)]
struct Row {
    id: i32,
    name: String,
    region: String,
    state: String,
    web_urls: String,
    ssh_addresses: String,
    database_bytes: i64,
    write_rate: i64,
    maximum_database_bytes: i64,
    maximum_write_rate: i64,
    node_keys: String,
}
impl Row {
    fn cell(self) -> Result<Cell> {
        Cell {
            id: Prefix::try_from(i64::from(self.id))?,
            name: self.name,
            region: self.region,
            state: State::parse(&self.state).ok_or(Refusal::Unavailable)?,
            web_urls: serde_json::from_str(&self.web_urls)?,
            ssh_addresses: serde_json::from_str(&self.ssh_addresses)?,
            node_keys: serde_json::from_str(&self.node_keys)?,
            database_bytes: self.database_bytes,
            write_rate: self.write_rate,
            maximum_database_bytes: self.maximum_database_bytes,
            maximum_write_rate: self.maximum_write_rate,
        }
        .prepare()
    }
}

impl Directory {
    /// Local operator only; this method is never exposed as a cell-authenticated mutation.
    pub async fn register_cell(&self, cell: Cell) -> Result<()> {
        self.run(async {
            let cell = cell.prepare().map_err(|_| Refusal::Invalid)?;
            let id = i32::from(cell.id.get());
            let mut tx = self.begin().await?;
            sqlx::query("INSERT INTO directory_cells(id,name,region,state,web_urls,ssh_addresses,database_bytes,write_rate,maximum_database_bytes,maximum_write_rate) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT(id) DO UPDATE SET name=excluded.name,region=excluded.region,state=excluded.state,web_urls=excluded.web_urls,ssh_addresses=excluded.ssh_addresses,maximum_database_bytes=excluded.maximum_database_bytes,maximum_write_rate=excluded.maximum_write_rate")
                .bind(id).bind(&cell.name).bind(&cell.region).bind(cell.state.as_str()).bind(serde_json::to_value(&cell.web_urls)?).bind(serde_json::to_value(&cell.ssh_addresses)?).bind(cell.database_bytes).bind(cell.write_rate).bind(cell.maximum_database_bytes).bind(cell.maximum_write_rate).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO directory_cell_prefixes(prefix,cell_id) VALUES($1,$1) ON CONFLICT DO NOTHING")
                .bind(id).execute(&mut *tx).await?;
            let assigned: i32 = sqlx::query_scalar("SELECT cell_id FROM directory_cell_prefixes WHERE prefix=$1 FOR UPDATE")
                .bind(id).fetch_one(&mut *tx).await?;
            ensure!(assigned == id, Refusal::Conflict);
            sqlx::query("DELETE FROM directory_node_keys WHERE cell_id=$1 AND NOT(public_key=ANY($2))")
                .bind(id).bind(&cell.node_keys).execute(&mut *tx).await?;
            for key in &cell.node_keys {
                sqlx::query("INSERT INTO directory_node_keys(public_key,cell_id) VALUES($1,$2) ON CONFLICT DO NOTHING")
                    .bind(key).bind(id).execute(&mut *tx).await?;
            }
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM directory_node_keys WHERE cell_id=$1 AND public_key=ANY($2)")
                .bind(id).bind(&cell.node_keys).fetch_one(&mut *tx).await?;
            ensure!(count as usize == cell.node_keys.len(), Refusal::Conflict);
            tx.commit().await?;
            Ok(())
        }).await
    }

    /// At 80% use, reserve one permanent successor. Retrying an uncertain response
    /// returns the same prefix; enrollment and the previous range are retained at commit.
    pub async fn next_prefix(&self, peer: &str, previous: Prefix, last_id: i64) -> Result<Prefix> {
        self.run(async {
            ensure!(Prefix::for_id(last_id).ok() == Some(previous), Refusal::Invalid);
            ensure!(previous.needs_next_prefix(last_id), Refusal::Invalid);
            let mut tx = self.begin().await?;
            let cell = super::owner(&mut tx, peer, false).await?;
            let owned: Option<i32> = sqlx::query_scalar("SELECT cell_id FROM directory_cell_prefixes WHERE prefix=$1 FOR UPDATE")
                .bind(i32::from(previous.get())).fetch_optional(&mut *tx).await?;
            ensure!(owned == Some(i32::from(cell.get())), Refusal::Denied);
            let existing: Option<i32> = sqlx::query_scalar("SELECT prefix FROM directory_cell_prefixes WHERE previous_prefix=$1")
                .bind(i32::from(previous.get())).fetch_optional(&mut *tx).await?;
            if let Some(prefix) = existing {
                tx.commit().await?;
                return Ok(Prefix::try_from(i64::from(prefix))?);
            }
            for _ in 0..8 {
                // Prefix zero belongs to an existing single-cell installation, even
                // before an operator enrolls it. The 16-bit search is bounded.
                let prefix: Option<i32> = sqlx::query_scalar("INSERT INTO directory_cell_prefixes(prefix,cell_id,previous_prefix) SELECT candidate,$1,$2 FROM generate_series(1,65535) candidate WHERE NOT EXISTS(SELECT 1 FROM directory_cell_prefixes p WHERE p.prefix=candidate) ORDER BY candidate LIMIT 1 ON CONFLICT DO NOTHING RETURNING prefix")
                    .bind(i32::from(cell.get())).bind(i32::from(previous.get())).fetch_optional(&mut *tx).await?;
                if let Some(prefix) = prefix {
                    tx.commit().await?;
                    return Ok(Prefix::try_from(i64::from(prefix))?);
                }
            }
            anyhow::bail!(Refusal::Unavailable)
        }).await
    }

    /// Local operator only, with a transport-authenticated key at use time.
    pub async fn set_router_key(&self, key: &str, enabled: bool) -> Result<()> {
        self.run(async {
            ensure!(codeyard_support::tokens::is_hex_token(key, 64), Refusal::Invalid);
            let key = key.to_ascii_lowercase();
            let mut tx = self.begin().await?;
            if enabled {
                sqlx::query("INSERT INTO directory_router_keys(public_key) VALUES($1) ON CONFLICT DO NOTHING").bind(key).execute(&mut *tx).await?;
            } else {
                sqlx::query("DELETE FROM directory_router_keys WHERE public_key=$1").bind(key).execute(&mut *tx).await?;
            }
            tx.commit().await?;
            Ok(())
        }).await
    }

    /// The key is authenticated by node-key TLS, never taken from request JSON.
    pub async fn authorize_read(&self, peer: &str) -> Result<()> {
        self.run(async {
            ensure!(
                codeyard_support::tokens::is_hex_token(peer, 64),
                Refusal::Denied
            );
            let mut tx = self.begin().await?;
            super::read_owner(&mut tx, peer).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    pub async fn cell(&self, prefix: Prefix) -> Result<Option<Cell>> {
        self.cell_for(None, prefix).await
    }

    pub async fn cell_for(&self, peer: Option<&str>, prefix: Prefix) -> Result<Option<Cell>> {
        self.run(async {
            let mut tx = self.begin().await?;
            if let Some(peer) = peer { super::read_owner(&mut tx, peer).await?; }
            let row: Option<Row> = sqlx::query_as(&format!("SELECT {COLUMNS} FROM directory_cells c JOIN directory_cell_prefixes p ON p.cell_id=c.id WHERE p.prefix=$1"))
                .bind(i32::from(prefix.get())).fetch_optional(&mut *tx).await?;
            let cell = row.map(Row::cell).transpose()?;
            tx.commit().await?;
            Ok(cell)
        }).await
    }

    pub async fn cells(&self, after: Option<Prefix>, limit: i64) -> Result<Vec<Cell>> {
        self.run(async {
            ensure!((1..=100).contains(&limit), Refusal::Invalid);
            let mut tx = self.begin().await?;
            let rows: Vec<Row> = sqlx::query_as(&format!(
                "SELECT {COLUMNS} FROM directory_cells c WHERE c.id>$1 ORDER BY c.id LIMIT $2"
            ))
            .bind(after.map_or(-1, |prefix| i32::from(prefix.get())))
            .bind(limit)
            .fetch_all(&mut *tx)
            .await?;
            let cells = rows
                .into_iter()
                .map(Row::cell)
                .collect::<Result<Vec<_>>>()?;
            tx.commit().await?;
            Ok(cells)
        })
        .await
    }

    pub async fn choose_cell(&self, region: &str) -> Result<Option<Cell>> {
        self.choose_cell_for(None, region).await
    }

    pub async fn choose_cell_for(&self, peer: Option<&str>, region: &str) -> Result<Option<Cell>> {
        self.run(async {
            codeyard_model::cells::validate_region(region).map_err(|_| Refusal::Invalid)?;
            let mut tx = self.begin().await?;
            if let Some(peer) = peer { super::read_owner(&mut tx, peer).await?; }
            let row: Option<Row> = sqlx::query_as(&format!("SELECT {COLUMNS} FROM directory_cells c WHERE region=$1 AND state='open' AND database_bytes::numeric<maximum_database_bytes::numeric/2 AND write_rate::numeric<maximum_write_rate::numeric/2 AND observed_at>clock_timestamp()-$2::bigint*INTERVAL '1 second' ORDER BY database_bytes::numeric/maximum_database_bytes,id LIMIT 1"))
                .bind(region).bind(self.config.stale_seconds).fetch_optional(&mut *tx).await?;
            let cell = row.map(Row::cell).transpose()?;
            tx.commit().await?;
            Ok(cell)
        }).await
    }

    pub async fn report_load(&self, peer: &str, bytes: i64, write_rate: i64) -> Result<()> {
        self.run(async {
            ensure!(bytes >= 0 && write_rate >= 0, Refusal::Invalid);
            let mut tx = self.begin().await?;
            let cell = super::owner(&mut tx, peer, true).await?;
            sqlx::query("UPDATE directory_cells SET database_bytes=$2,write_rate=$3,observed_at=clock_timestamp() WHERE id=$1")
                .bind(i32::from(cell.get())).bind(bytes).bind(write_rate).execute(&mut *tx).await?;
            tx.commit().await?;
            Ok(())
        }).await
    }
}
