//! Home pointers select resource cells; their existing HTTP handlers own list authorization.
use axum::http::HeaderMap;
use codeyard_database::{
    cells::pointers::{self, Budget, Config, List, Page, PageResult},
    DbPool,
};
use codeyard_identity::github_auth::{self, Authentication};
use codeyard_types::github_error::{Error, Result};
use futures_util::{stream, StreamExt};
use serde_json::Value;
use std::time::Duration;

fn failure(error: anyhow::Error) -> Error {
    if error.is::<codeyard_types::credentials::AccountRestricted>() {
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
        (auth.user.id, auth.user.is_stub),
        Some(&auth.database_actor()),
        Some(credential),
        list,
        query,
        wanted,
    )
    .await?
    .map(|page| page.rows))
}

pub async fn browser(
    pool: &DbPool,
    actor: &codeyard_database::queries::credentials::Actor,
    stub: bool,
    session: &str,
    list: List,
    query: Vec<(String, String)>,
    wanted: usize,
) -> Result<Option<PageResult>> {
    if !list.browser()
        || !matches!(actor.credential(), Some(codeyard_database::queries::users::accounts::AccountCredential::Session(value)) if value == session)
    {
        return Err(github_auth::required());
    }
    codeyard_types::cell_ids::Secret::cookie(session).map_err(|_| github_auth::required())?;
    read(
        pool,
        (actor.user_id, stub),
        Some(actor),
        Some(session),
        list,
        query,
        wanted,
    )
    .await
}

/// A star pointer selects candidates only; the original viewer authorizes every resource read.
pub async fn stars(
    pool: &DbPool,
    owner: &codeyard_database::models::User,
    actor: Option<&codeyard_database::queries::credentials::Actor>,
    credential: Option<&str>,
    list: List,
    query: Vec<(String, String)>,
    wanted: usize,
) -> Result<Option<PageResult>> {
    if !list.stars() || actor.is_some() != credential.is_some() {
        return Err(github_auth::required());
    }
    if let (Some(actor), Some(value)) = (actor, credential) {
        use codeyard_database::queries::users::accounts::AccountCredential;
        let valid = match actor.credential() {
            Some(AccountCredential::Session(session)) => session == value,
            Some(AccountCredential::Token(_, hash) | AccountCredential::ForeignToken(_, hash)) => {
                codeyard_support::token_hash::token_hash(value) == hash
            }
            _ => false,
        };
        if !valid {
            return Err(github_auth::required());
        }
    }
    read(
        pool,
        (owner.id, owner.is_stub),
        actor,
        credential,
        list,
        query,
        wanted,
    )
    .await
}

pub async fn recheck_stars(
    pool: &DbPool,
    owner: &codeyard_database::models::User,
    actor: Option<&codeyard_database::queries::credentials::Actor>,
) -> Result<()> {
    if !owner.is_stub && pool.foreign_authorization().is_some() {
        pointers::star_homes(pool, owner.id, actor, Config::default())
            .await
            .map_err(failure)?;
    }
    Ok(())
}

async fn homes(
    pool: &DbPool,
    owner: i64,
    actor: Option<&codeyard_database::queries::credentials::Actor>,
    list: List,
    config: Config,
) -> Result<Vec<codeyard_types::cell_ids::Prefix>> {
    if list.stars() {
        pointers::star_homes(pool, owner, actor, config)
            .await
            .map_err(failure)
    } else {
        pointers::homes(pool, actor.ok_or_else(github_auth::required)?, config)
            .await
            .map_err(failure)
    }
}

async fn read(
    pool: &DbPool,
    owner: (i64, bool),
    actor: Option<&codeyard_database::queries::credentials::Actor>,
    credential: Option<&str>,
    list: List,
    query: Vec<(String, String)>,
    wanted: usize,
) -> Result<Option<PageResult>> {
    let (owner, stub) = owner;
    if stub || pool.foreign_authorization().is_none() {
        return Ok(None);
    }
    let config = Config::default();
    config.validate().map_err(failure)?;
    let selected = homes(pool, owner, actor, list, config).await?;
    if selected.is_empty() {
        return Ok(None);
    }
    if wanted == 0 || wanted > config.maximum_scan {
        return Err(Error::invalid("Resource page exceeds its limit"));
    }
    let provider = pool
        .foreign_authorization()
        .ok_or_else(|| Error::unavailable(anyhow::anyhow!("Resource cells are unavailable")))?;
    let page_size = list.page_size(config);
    let result = tokio::time::timeout(Duration::from_secs(config.request_seconds), async {
        let mut pending = stream::iter(selected)
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
                        let result = provider.resource_page(home, &page, credential).await?;
                        anyhow::ensure!(
                            result.rows.len() <= page_size
                                && result
                                    .total
                                    .is_none_or(|total| total >= result.rows.len() as i64),
                            "Invalid resource page"
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
                    anyhow::bail!("Resource scan exceeds its limit")
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
                        .ok_or_else(|| anyhow::anyhow!("Resource count exceeds its limit"))?,
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
    homes(pool, owner, actor, list, config).await?;
    let rows = result
        .map_err(|_| Error::unavailable(anyhow::anyhow!("Resource cells timed out")))?
        .map_err(failure)?;
    Ok(Some(rows))
}

fn resource(row: &Value) -> &Value {
    row.get("repo").unwrap_or(row)
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
    values.retain(|row| {
        resource(row)["id"]
            .as_i64()
            .is_some_and(|id| seen.insert(id))
    });
    values.sort_by(|left, right| {
        let a = resource(left);
        let b = resource(right);
        let key = match sort {
            "starred" => "starred_at",
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
            if sort == "starred" {
                date(left).cmp(&date(right))
            } else {
                date(a).cmp(&date(b))
            }
        }
        .then_with(|| {
            if sort == "stars" && left.get("repo").is_some() {
                left["starred_at"]
                    .as_str()
                    .cmp(&right["starred_at"].as_str())
            } else {
                std::cmp::Ordering::Equal
            }
        })
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
    pointers::homes(pool, &auth.database_actor(), Config::default())
        .await
        .map_err(failure)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn star_pages_merge_by_star_time_and_keep_independent_pages() {
        let row = |id, at| serde_json::json!({"repo":{"id":id,"created_at":"2020-01-01T00:00:00Z","stargazers_count":1},"starred_at":at});
        let rows = vec![
            row(2, "2026-01-01T00:00:00Z"),
            row(1, "2026-02-01T00:00:00Z"),
            row(2, "2026-01-01T00:00:00Z"),
        ];
        let (first, more) = page(rows.clone(), "starred", true, 0, 1).unwrap();
        assert!(more);
        assert_eq!(first[0]["repo"]["id"], 1);
        let (second, more) = page(rows.clone(), "starred", true, 1, 1).unwrap();
        assert!(!more);
        assert_eq!(second[0]["repo"]["id"], 2);
        let (stars, _) = page(rows, "stars", true, 0, 2).unwrap();
        assert_eq!(stars[0]["repo"]["id"], 1);
    }
}
