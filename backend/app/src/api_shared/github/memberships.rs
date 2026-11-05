//! Home pointers select resource cells; their existing HTTP handlers own list authorization.
use axum::http::HeaderMap;
use codeyard_db_core::{
    cells::memberships::{self, Budget, Config, List, Page, PageResult},
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
    let credential = github_auth::credential(headers).ok_or_else(github_auth::required)?;
    if codeyard_support::token_hash::token_hash(credential) != auth.token.token_hash {
        return Err(github_auth::required());
    }
    Ok(read(
        pool,
        &auth.database_actor(),
        auth.user.is_stub,
        credential,
        list,
        query,
        wanted,
    )
    .await?
    .map(|page| page.rows))
}

pub async fn browser(
    pool: &DbPool,
    actor: &codeyard_db_core::queries::credentials::Actor,
    stub: bool,
    session: &str,
    list: List,
    query: Vec<(String, String)>,
    wanted: usize,
) -> Result<Option<PageResult>> {
    if !list.browser()
        || !matches!(actor.credential(), Some(codeyard_db_core::queries::users::accounts::AccountCredential::Session(value)) if value == session)
    {
        return Err(github_auth::required());
    }
    codeyard_support::cells::Secret::cookie(session).map_err(|_| github_auth::required())?;
    read(pool, actor, stub, session, list, query, wanted).await
}

async fn read(
    pool: &DbPool,
    actor: &codeyard_db_core::queries::credentials::Actor,
    stub: bool,
    credential: &str,
    list: List,
    query: Vec<(String, String)>,
    wanted: usize,
) -> Result<Option<PageResult>> {
    if stub || pool.foreign_authorization().is_none() {
        return Ok(None);
    }
    let config = Config::default();
    config.validate().map_err(failure)?;
    let homes = memberships::homes(pool, actor, config)
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
    let page_size = list.page_size(config);
    let result = tokio::time::timeout(Duration::from_secs(config.request_seconds), async {
        let mut pending = stream::iter(homes)
            .map(|home| {
                let provider = provider.clone();
                let mut query = query.clone();
                async move {
                    let mut values = Vec::new();
                    let mut budget = Budget::new(config)?;
                    let mut total = None;
                    for number in 1..=(config.maximum_scan / page_size + 1) {
                        let page = Page {
                            list,
                            query: query.clone(),
                            number: number as u32,
                            size: page_size as u16,
                        };
                        let result = provider.membership_page(home, &page, credential).await?;
                        anyhow::ensure!(
                            result.rows.len() <= page_size
                                && result
                                    .total
                                    .is_none_or(|total| total >= result.rows.len() as i64),
                            "Invalid membership page"
                        );
                        if number == 1 {
                            total = result.total;
                        }
                        let done = result.rows.len() < page_size;
                        if matches!(list, List::BrowserOrganizations) && !done {
                            let after = result
                                .rows
                                .last()
                                .and_then(|row| row["id"].as_i64())
                                .ok_or_else(|| anyhow::anyhow!("Invalid organization page"))?;
                            query.retain(|(key, _)| key != "after");
                            query.push(("after".into(), after.to_string()));
                        }
                        for row in result.rows.into_iter().take(wanted - values.len()) {
                            budget.add(&row)?;
                            values.push(row);
                        }
                        if done || values.len() == wanted {
                            return Ok::<_, anyhow::Error>((values, budget, total));
                        }
                    }
                    anyhow::bail!("Membership scan exceeds its limit")
                }
            })
            .buffer_unordered(4);
        let mut values = Vec::new();
        let mut budget = Budget::new(config)?;
        let mut total = Some(0_i64);
        while let Some(result) = pending.next().await {
            let (rows, used, count) = result?;
            total = match (total, count) {
                (Some(a), Some(b)) => Some(
                    a.checked_add(b)
                        .ok_or_else(|| anyhow::anyhow!("Membership count exceeds its limit"))?,
                ),
                _ => None,
            };
            budget.merge(&used)?;
            values.extend(rows);
        }
        Ok::<_, anyhow::Error>(PageResult {
            rows: values,
            total,
        })
    })
    .await;
    // The original home credential is read again after every remote wait, even on failure.
    memberships::homes(pool, actor, config)
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
    let mut budget = Budget::new(config).map_err(Error::unavailable)?;
    for value in &values {
        budget.add(value).map_err(Error::unavailable)?;
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
