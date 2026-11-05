//! Home pointers select resource cells; their existing HTTP handlers own list authorization.
use axum::http::HeaderMap;
use codeyard_db_core::{
    cells::memberships::{self, Config, List, Page},
    DbPool,
};
use codeyard_identity::github_auth::{self, Authentication};
use codeyard_model::github_error::{Error, Result};
use futures_util::{stream, StreamExt};
use serde_json::Value;
use std::time::Duration;

fn failure(error: anyhow::Error) -> Error {
    if error.is::<codeyard_support::credentials::AccountRestricted>() {
        github_auth::required()
    } else {
        Error::unavailable(error)
    }
}

pub async fn remote(
    pool: &DbPool,
    auth: &Authentication,
    headers: &HeaderMap,
    list: List,
    query: Vec<(String, String)>,
    wanted: usize,
) -> Result<Option<Vec<Value>>> {
    if auth.user.is_stub || pool.foreign_authorization().is_none() {
        return Ok(None);
    }
    let config = Config::default();
    config.validate().map_err(failure)?;
    let homes = memberships::homes(pool, &auth.database_actor(), config)
        .await
        .map_err(failure)?;
    if homes.is_empty() {
        return Ok(None);
    }
    if wanted == 0 || wanted > config.maximum_scan {
        return Err(Error::invalid("Membership page exceeds its limit"));
    }
    let provider = pool
        .foreign_authorization()
        .ok_or_else(|| Error::unavailable(anyhow::anyhow!("Resource cells are unavailable")))?;
    let credential = github_auth::credential(headers).ok_or_else(github_auth::required)?;
    if codeyard_support::token_hash::token_hash(credential) != auth.token.token_hash {
        return Err(github_auth::required());
    }
    let result = tokio::time::timeout(Duration::from_secs(config.request_seconds), async {
        let mut pending = stream::iter(homes)
            .map(|home| {
                let provider = provider.clone();
                let query = query.clone();
                async move {
                    let mut values = Vec::new();
                    let mut bytes = 0usize;
                    for number in 1..=(config.maximum_scan / config.page_size + 1) {
                        let page = Page {
                            list,
                            query: query.clone(),
                            number: number as u32,
                            size: config.page_size as u16,
                        };
                        let rows = provider.membership_page(home, &page, credential).await?;
                        let done = rows.len() < config.page_size;
                        for row in rows.into_iter().take(wanted - values.len()) {
                            bytes = bytes
                                .checked_add(serde_json::to_vec(&row)?.len())
                                .ok_or_else(|| {
                                    anyhow::anyhow!("Membership response exceeds its limit")
                                })?;
                            anyhow::ensure!(
                                bytes <= config.maximum_bytes,
                                "Membership response exceeds its limit"
                            );
                            values.push(row);
                        }
                        if done || values.len() == wanted {
                            return Ok::<_, anyhow::Error>((values, bytes));
                        }
                    }
                    anyhow::bail!("Membership scan exceeds its limit")
                }
            })
            .buffer_unordered(4);
        let mut values = Vec::new();
        let mut bytes = 0usize;
        while let Some(result) = pending.next().await {
            let (rows, size) = result?;
            bytes = bytes
                .checked_add(size)
                .ok_or_else(|| anyhow::anyhow!("Membership response exceeds its limit"))?;
            anyhow::ensure!(
                bytes <= config.maximum_bytes && values.len() + rows.len() <= config.maximum_scan,
                "Membership response exceeds its limit"
            );
            values.extend(rows);
        }
        Ok::<_, anyhow::Error>(values)
    })
    .await;
    // The original home credential is read again after every remote wait, even on failure.
    memberships::homes(pool, &auth.database_actor(), config)
        .await
        .map_err(failure)?;
    let rows = result
        .map_err(|_| Error::unavailable(anyhow::anyhow!("Resource cells timed out")))?
        .map_err(failure)?;
    Ok(Some(rows))
}

pub fn page(
    mut values: Vec<Value>,
    sort: &str,
    descending: bool,
    offset: usize,
    limit: usize,
) -> Result<(Vec<Value>, bool)> {
    let config = Config::default();
    let mut bytes = 0usize;
    for value in &values {
        bytes = bytes
            .checked_add(serde_json::to_vec(value).map_err(Error::unavailable)?.len())
            .ok_or_else(|| Error::invalid("Membership response exceeds its limit"))?;
    }
    if values.len() > config.maximum_scan || bytes > config.maximum_bytes {
        return Err(Error::unavailable(anyhow::anyhow!(
            "Membership response exceeds its limit"
        )));
    }
    let mut seen = std::collections::HashSet::new();
    values.retain(|row| row["id"].as_i64().is_some_and(|id| seen.insert(id)));
    values.sort_by(|a, b| {
        let key = match sort {
            "created" => "created_at",
            "updated" => "updated_at",
            "pushed" => "pushed_at",
            "stars" => "stargazers_count",
            "watchers" => "watchers_count",
            "id" => "id",
            _ => "full_name",
        };
        let order = if matches!(sort, "id" | "stars" | "watchers") {
            a[key].as_i64().cmp(&b[key].as_i64())
        } else if sort == "full_name" {
            a[key]
                .as_str()
                .unwrap_or("")
                .to_lowercase()
                .cmp(&b[key].as_str().unwrap_or("").to_lowercase())
        } else {
            let date = |row: &Value| {
                row[key]
                    .as_str()
                    .or_else(|| row["created_at"].as_str())
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            };
            date(a).cmp(&date(b))
        }
        .then(a["id"].as_i64().cmp(&b["id"].as_i64()));
        if descending {
            order.reverse()
        } else {
            order
        }
    });
    let next = values.len().saturating_sub(offset) > limit;
    Ok((values.into_iter().skip(offset).take(limit).collect(), next))
}

pub async fn recheck(pool: &DbPool, auth: &Authentication) -> Result<()> {
    memberships::homes(pool, &auth.database_actor(), Config::default())
        .await
        .map_err(failure)?;
    Ok(())
}
