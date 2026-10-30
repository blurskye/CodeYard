//! Bounded request-local enrichment for Checks pages. Database parents, publishers and
//! pull associations are batched; Git heads are read once and released after their last use.
use super::{
    actions_app, app_value, checks, db, payloads, pull_value, repo_store, run_value,
    store_error, suite_value, AppState, Error, Repository, Result, Run, Suite, SuiteDocuments,
};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};

fn keys<'a>(suites: impl Iterator<Item = &'a Suite>) -> (Vec<i64>, Vec<String>) {
    let mut apps = BTreeSet::new();
    let mut heads = BTreeSet::new();
    for suite in suites {
        if let Some(id) = suite.app_id { apps.insert(id); }
        heads.insert(suite.head_sha.clone());
    }
    (apps.into_iter().collect(), heads.into_iter().collect())
}

struct Related {
    publishers: HashMap<i64, Value>,
    pulls: HashMap<String, Value>,
    empty_pulls: Value,
}

impl Related {
    async fn load(pool: &db::DbPool, repo: &Repository, owner: &str, apps: &[i64], heads: &[String]) -> Result<Self> {
        let publishers = checks::publishers(pool, apps).await.map_err(Error::unavailable)?
            .into_iter().map(|app| (app.id, app_value(&app))).collect();
        let mut pulls: HashMap<String, Vec<Value>> = HashMap::new();
        for pull in checks::pulls_many(pool, repo.id, heads).await.map_err(Error::unavailable)? {
            pulls.entry(pull.head_sha.clone()).or_default().push(pull_value(repo, owner, &pull));
        }
        Ok(Self {
            publishers,
            pulls: pulls.into_iter().map(|(sha, rows)| (sha, Value::Array(rows))).collect(),
            empty_pulls: Value::Array(Vec::new()),
        })
    }

    fn publisher(&self, id: Option<i64>) -> &Value {
        id.and_then(|id| self.publishers.get(&id)).unwrap_or(&Value::Null)
    }

    fn pulls(&self, sha: &str) -> &Value {
        self.pulls.get(sha).unwrap_or(&self.empty_pulls)
    }
}

pub(super) async fn runs(state: &AppState, repo: &Repository, owner: &str, rows: &[Run]) -> Result<Vec<Value>> {
    let ids: Vec<_> = rows.iter().map(|run| run.id).collect();
    let metadata: HashMap<_, _> = checks::run_details(&state.pool, repo.id, &ids).await.map_err(Error::unavailable)?
        .into_iter().map(|detail| (detail.run_id, detail)).collect();
    let (apps, heads) = keys(metadata.values().map(|detail| &detail.suite));
    let related = Related::load(&state.pool, repo, owner, &apps, &heads).await?;
    rows.iter().map(|run| {
        let detail = metadata.get(&run.id).ok_or_else(Error::missing)?;
        Ok(run_value(repo, owner, run, &detail.suite, detail.annotation_count, related.publisher(detail.suite.app_id), related.pulls(&detail.suite.head_sha)))
    }).collect()
}

pub(super) async fn suites(state: &AppState, repo: &Repository, owner: &str, rows: &[Suite]) -> Result<Vec<Value>> {
    suite_page(Some(state), &state.pool, repo, owner, rows, &mut HashMap::new()).await
}

pub(super) async fn suites_at(pool: &db::DbPool, repo: &Repository, owner: &str, rows: &[Suite], heads: &mut HashMap<String, Value>) -> Result<Vec<Value>> {
    suite_page(None, pool, repo, owner, rows, heads).await
}

async fn suite_page(state: Option<&AppState>, pool: &db::DbPool, repo: &Repository, owner: &str, rows: &[Suite], heads: &mut HashMap<String, Value>) -> Result<Vec<Value>> {
    let ids: Vec<_> = rows.iter().map(|suite| suite.id).collect();
    let metadata: HashMap<_, _> = checks::suite_details(pool, repo.id, &ids).await.map_err(Error::unavailable)?
        .into_iter().map(|detail| (detail.suite_id, detail)).collect();
    let (apps, shas) = keys(rows.iter());
    let related = Related::load(pool, repo, owner, &apps, &shas).await?;
    let actions = if rows.iter().any(|suite| suite.ci_run_id.is_some()) {
        actions_app(pool, repo, owner).await?
    } else { Value::Null };
    let mut remaining = HashMap::new();
    for suite in rows { *remaining.entry(suite.head_sha.clone()).or_insert(0_usize) += 1; }
    let mut documents = Vec::with_capacity(rows.len());
    for suite in rows {
        if !heads.contains_key(&suite.head_sha) {
            let state = state.ok_or_else(Error::missing)?;
            let commit = repo_store(repo, state.cluster.as_ref()).commit(&suite.head_sha).await.map_err(store_error)?;
            heads.insert(suite.head_sha.clone(), payloads::simple_commit(&commit));
        }
        let detail = metadata.get(&suite.id);
        let native_state = if suite.ci_run_id.is_some() {
            let detail = detail.ok_or_else(Error::missing)?;
            db::action_runs::state_with_approval(detail.ci_status.as_deref().ok_or_else(Error::missing)?, detail.ci_approval_required)
        } else { (&*suite.status, suite.conclusion.as_deref()) };
        let links = SuiteDocuments {
            publisher: if suite.ci_run_id.is_some() { &actions } else { related.publisher(suite.app_id) },
            head_commit: heads.get(&suite.head_sha).ok_or_else(Error::missing)?,
            pull_requests: related.pulls(&suite.head_sha),
        };
        documents.push(suite_value(repo, owner, suite, detail.map_or(0, |detail| detail.run_count), native_state, links));
        if let Some(left) = remaining.get_mut(&suite.head_sha) {
            *left -= 1;
            if *left == 0 { heads.remove(&suite.head_sha); }
        }
    }
    Ok(documents)
}
