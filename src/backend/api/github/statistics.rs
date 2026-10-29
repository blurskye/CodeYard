//! Statistics share one bounded, ref-cached history scan on local and remote storage.
use super::{contents::store_error, pagination::Pagination, repository, Error, Result};
use crate::backend::{
    api::repos::code::contributors::contributor_accounts,
    app::AppState,
    db::models::User,
    git::{AuthorWeek, CommitStatistics, ContributorInfo},
    repositories::store::repo_store,
    webhooks::payloads,
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, MethodRouter},
    Json,
};
use chrono::Datelike;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

async fn accounts(state: &AppState, stats: &CommitStatistics) -> Result<HashMap<String, User>> {
    let contributors = stats
        .authors
        .iter()
        .map(|author| ContributorInfo {
            name: author.name.clone(),
            email: author.email.clone(),
            commits: author.commits.min(u32::MAX as u64) as u32,
        })
        .collect::<Vec<_>>();
    contributor_accounts(&state.pool, &contributors)
        .await
        .map_err(Error::unavailable)
}

/// The most author-by-week entries one contributors response may build, zero weeks included.
const MAX_CONTRIBUTOR_CELLS: usize = 100_000;

fn this_week() -> i64 {
    let today = chrono::Utc::now().date_naive();
    today.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp()
        - i64::from(today.weekday().num_days_from_sunday()) * 86_400
}

fn history_weeks(stats: &CommitStatistics) -> Result<Vec<i64>> {
    let first = stats
        .weeks
        .first()
        .map(|week| week.week)
        .unwrap_or_else(this_week);
    let last = this_week().max(stats.weeks.last().map(|week| week.week).unwrap_or(first));
    if (last - first) / 604800 > 10_000 {
        return Err(Error::invalid("Statistics exceed their time range"));
    }
    Ok((first..=last).step_by(604800).collect())
}

async fn statistics(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let kind = uri.path().rsplit('/').next().unwrap_or("");
    let mode = if matches!(kind, "contributors" | "code_frequency") { crate::backend::git::StatisticsMode::Full } else { crate::backend::git::StatisticsMode::Counts };
    let stats = repo_store(&repo, state.cluster.as_ref())
        .commit_statistics(&repo.default_branch, mode)
        .await
        .map_err(store_error)?;
    if stats.commits == 0 && matches!(kind, "code_frequency" | "contributors" | "commit_activity") {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let current_week = this_week();
    let year = (0..52)
        .map(|index| current_week - (51 - index) * 604800)
        .collect::<Vec<_>>();
    let value = match kind {
        "code_frequency" => {
            if stats.commits > 10_000 {
                return Err(Error::invalid(
                    "Repository contains more than 10,000 commits",
                ));
            }
            Value::Array(
                history_weeks(&stats)?
                    .into_iter()
                    .map(|date| {
                        let week = stats.weeks.iter().find(|week| week.week == date);
                        json!([
                            date,
                            week.map_or(0, |week| week.additions as i64),
                            -week.map_or(0, |week| week.deletions as i64)
                        ])
                    })
                    .collect(),
            )
        }
        "commit_activity" => Value::Array(
            year.iter()
                .map(|date| {
                    let days = stats
                        .weeks
                        .iter()
                        .find(|week| week.week == *date)
                        .map_or([0; 7], |week| week.days);
                    json!({"week":date, "days":days, "total":days.iter().sum::<u64>()})
                })
                .collect(),
        ),
        "punch_card" => json!(stats.punch_card),
        "participation" => {
            let accounts = accounts(&state, &stats).await?;
            let mut all = Vec::new();
            let mut own = Vec::new();
            for date in &year {
                all.push(
                    stats
                        .weeks
                        .iter()
                        .find(|week| week.week == *date)
                        .map_or(0, |week| week.days.iter().sum::<u64>()),
                );
                own.push(
                    stats
                        .authors
                        .iter()
                        .filter(|author| {
                            accounts
                                .get(&author.email)
                                .is_some_and(|user| repo.is_personal_owner(user.id))
                        })
                        .flat_map(|author| &author.weeks)
                        .filter(|week| week.week == *date)
                        .map(|week| week.all_commits)
                        .sum::<u64>(),
                );
            }
            json!({"all":all, "owner":own})
        }
        "contributors" => {
            let accounts = accounts(&state, &stats).await?;
            let dates = history_weeks(&stats)?;
            // An account that commits under several addresses is one contributor, with their
            // activity combined. Addresses without an account stay apart.
            let mut authors: Vec<(u64, &str, Option<&User>, Vec<&AuthorWeek>)> = Vec::new();
            let mut by_account: HashMap<i64, usize> = HashMap::new();
            for author in stats.authors.iter().filter(|author| author.total > 0) {
                let user = accounts.get(&author.email);
                if let Some(&index) = user.and_then(|user| by_account.get(&user.id)) {
                    let (total, _, _, weeks) = &mut authors[index];
                    *total += author.total;
                    weeks.extend(&author.weeks);
                    continue;
                }
                if let Some(user) = user {
                    by_account.insert(user.id, authors.len());
                }
                authors.push((
                    author.total,
                    &author.email,
                    user,
                    author.weeks.iter().collect(),
                ));
            }
            authors.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(b.1)));
            // Every author gets a zero-filled entry for every week, so check the product first.
            if authors.len().saturating_mul(dates.len()) > MAX_CONTRIBUTOR_CELLS {
                return Err(Error::invalid("Statistics exceed their response size"));
            }
            Value::Array(authors.into_iter().map(|(total, _, user, weeks)| {
                let weeks = dates.iter().map(|date| {
                    let (a, d, c) = weeks.iter().filter(|week| week.week == *date).fold((0, 0, 0), |(a, d, c), week| (a + week.additions, d + week.deletions, c + week.commits));
                    json!({"w":date, "a":a, "d":d, "c":c})
                }).collect::<Vec<_>>();
                json!({"author":user.map(|user| payloads::simple_person(&payloads::Person::of(user))), "total":total, "weeks":weeks})
            }).collect())
        }
        _ => return Err(Error::missing()),
    };
    Ok(Json(value).into_response())
}

#[derive(Deserialize)]
struct Contributors {
    anon: Option<String>,
}

async fn contributors(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<Contributors>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let stats = repo_store(&repo, state.cluster.as_ref())
        .commit_statistics(&repo.default_branch, crate::backend::git::StatisticsMode::Contributors)
        .await
        .map_err(store_error)?;
    if stats.commits == 0 {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let accounts = accounts(&state, &stats).await?;
    let mut authors = stats.authors.iter().collect::<Vec<_>>();
    authors.sort_by(|a, b| b.commits.cmp(&a.commits).then(a.email.cmp(&b.email)));
    let anonymous = matches!(query.anon.as_deref(), Some("true" | "1"));
    let mut values = Vec::<Value>::new();
    for (index, author) in authors.into_iter().enumerate() {
        if let Some(user) = accounts.get(&author.email).filter(|_| index < 500) {
            if let Some(value) = values.iter_mut().find(|value| value["id"] == user.id) {
                value["contributions"] =
                    json!(value["contributions"].as_u64().unwrap() + author.commits);
            } else {
                let mut value = payloads::simple_person(&payloads::Person::of(user));
                value["contributions"] = json!(author.commits);
                values.push(value);
            }
        } else if anonymous {
            values.push(json!({"name":author.name, "email":author.email, "type":"Anonymous", "contributions":author.commits}));
        }
    }
    values.sort_by_key(|value| std::cmp::Reverse(value["contributions"].as_u64().unwrap()));
    let (offset, limit) = page.window()?;
    let next = values.len() > offset + limit;
    page.response(
        &uri,
        values.into_iter().skip(offset).take(limit).collect(),
        next,
    )
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/get-code-frequency-stats", get(statistics)),
        ("repos/get-commit-activity-stats", get(statistics)),
        ("repos/get-contributors-stats", get(statistics)),
        ("repos/get-participation-stats", get(statistics)),
        ("repos/get-punch-card-stats", get(statistics)),
        ("repos/list-contributors", get(contributors)),
    ]
}
